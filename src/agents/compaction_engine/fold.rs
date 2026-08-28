//! Hard-fold helpers — deterministic collapse of a history slice when the
//! summarizer is unavailable or its output fails the quality audit.

use crate::providers::{ChatMessage, ContentPart};

pub(super) fn find_incremental_range(
    history: &[ChatMessage],
    boundary: usize,
) -> (usize, usize, usize, Option<String>) {
    let last_summary = history[..boundary].iter().rposition(|m| {
        m.role == "user"
            && m.text_content()
                .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
    });
    match last_summary {
        Some(idx) => {
            let existing = history[idx].text_content();
            (idx, idx + 1, boundary, Some(existing))
        }
        None => (0, 0, boundary, None),
    }
}

pub(super) fn is_empty_compact_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("no content to compact")
}

/// If history ends with one or more unanswered `user` messages, return the
/// index of the first real (non-compaction-summary) open user in that trailing
/// streak.
///
/// Compaction must never set `compact_end` past this index — the open user
/// (and any preceding pure-attachment user siblings in the trailing streak)
/// must survive `apply_compaction` as live history so the model still sees
/// the current request. Leading compaction-summary user messages inside the
/// trailing streak are *not* protected (they can be re-folded) and are skipped
/// when computing the index.
pub(super) fn open_user_protected_from(history: &[ChatMessage]) -> Option<usize> {
    if history.is_empty() {
        return None;
    }
    // Start of the trailing user-role streak (may include summaries + reminders).
    let mut streak_start = history.len();
    while streak_start > 0 && history[streak_start - 1].role == "user" {
        streak_start -= 1;
    }
    if streak_start == history.len() {
        return None; // last message is not user → no open user turn
    }
    // Skip compaction summaries at the front of the streak; protect from the
    // first non-summary user (attachment reminder or real request).
    let mut protected = streak_start;
    while protected < history.len()
        && history[protected]
            .text_content()
            .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
    {
        protected += 1;
    }
    if protected >= history.len() {
        // Trailing streak is only prior summary/summaries — no open request.
        None
    } else {
        Some(protected)
    }
}

/// Strip a leading `<system-reminder>...</system-reminder>` block so hard-fold
/// keeps the real user body instead of burning the per-msg budget on injection.
fn strip_leading_system_reminder(text: &str) -> &str {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("<system-reminder>") {
        if let Some(end) = rest.find("</system-reminder>") {
            return rest[end + "</system-reminder>".len()..].trim();
        }
    }
    text
}

/// Max chars kept from a prior summary when hard-folding an update on top.
pub(super) const HARD_PRIOR_SUMMARY_CHARS: usize = 6_000;
/// Per-message body budget in hard-fold (user / assistant text).
const HARD_MSG_CHARS: usize = 400;
/// Per tool-result body budget in hard-fold.
const HARD_TOOL_CHARS: usize = 200;
/// Total hard-fold body budget (before evidence index).
pub(super) const HARD_TOTAL_CHARS: usize = 12_000;

pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Truncate preferring the tail (user intent often at the end after injection).
fn truncate_chars_prefer_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let skip = count.saturating_sub(keep);
    let mut out = String::from("…");
    out.extend(s.chars().skip(skip));
    out
}

/// User text for hard-fold: strip system-reminder, keep body, prefer tail if long.
fn user_text_for_hard_fold(text: &str) -> String {
    if text.starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]") {
        return format!(
            "[prior compaction summary, {} chars]",
            text.chars().count()
        );
    }
    let body = strip_leading_system_reminder(text);
    if body.is_empty() {
        if text.contains("<system-reminder>") {
            return "[system-reminder only]".to_string();
        }
        return String::new();
    }
    // Short bodies kept whole; long bodies keep the tail (request text).
    if body.chars().count() <= HARD_MSG_CHARS {
        body.to_string()
    } else {
        truncate_chars_prefer_tail(body, HARD_MSG_CHARS)
    }
}

/// Collect File-part path markers so hard-fold does not drop media references.
/// `text_content()` only joins Text parts; pure-image user turns would otherwise
/// fold to empty / system-reminder-only and lose `[image: path]`.
fn media_markers_for_hard_fold(msg: &ChatMessage) -> String {
    let mut markers = Vec::new();
    for part in &msg.parts {
        if let ContentPart::File {
            path, mime_type, ..
        } = part
        {
            markers.push(crate::providers::media::marker_for_file(
                path,
                mime_type.as_deref(),
            ));
        }
    }
    markers.join(" ")
}

/// Deterministic, LLM-free fold of a message slice into a compact text body.
/// Preserves user turns, assistant text (or tool names), short tool results,
/// and media path markers from File parts.
pub(super) fn hard_fold_history(msgs: &[ChatMessage]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(
        "[HARD FOLD] Summarizer skipped or failed; deterministic history fold. \
         Do NOT invent user tasks from this fold; only continue work the user \
         explicitly requested in clear text."
            .to_string(),
    );

    let mut user_n = 0usize;
    let mut tool_n = 0usize;
    let mut asst_n = 0usize;

    for m in msgs {
        match m.role.as_str() {
            "user" => {
                user_n += 1;
                let text = m.text_content();
                let media = media_markers_for_hard_fold(m);
                // Prior compaction markers are re-injected separately when present.
                if text.starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]") {
                    lines.push(format!(
                        "U{user_n}: [prior compaction summary, {} chars]",
                        text.chars().count()
                    ));
                    continue;
                }
                let body = user_text_for_hard_fold(&text);
                let line = match (body.is_empty(), media.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => media,
                    (false, true) => body,
                    (false, false) => format!("{media} {body}"),
                };
                lines.push(format!("U{user_n}: {line}"));
            }
            "assistant" => {
                asst_n += 1;
                let text = m.text_content();
                let tools = m
                    .tool_calls
                    .as_ref()
                    .map(|tcs| {
                        tcs.iter()
                            .map(|tc| tc.name.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() && !tools.is_empty() {
                    lines.push(format!("A{asst_n}: [tools: {tools}]"));
                } else if !tools.is_empty() {
                    lines.push(format!(
                        "A{asst_n}: {} [tools: {tools}]",
                        truncate_chars(text.trim(), HARD_MSG_CHARS)
                    ));
                } else {
                    lines.push(format!(
                        "A{asst_n}: {}",
                        truncate_chars(text.trim(), HARD_MSG_CHARS)
                    ));
                }
            }
            "tool" => {
                tool_n += 1;
                let name = m.name.as_deref().unwrap_or("tool");
                let id = m.tool_call_id.as_deref().unwrap_or("?");
                let body = m.text_content();
                let err = m.is_error.unwrap_or(false);
                let flag = if err { " ERR" } else { "" };
                lines.push(format!(
                    "T{tool_n}{flag} {name}#{id}: {}",
                    truncate_chars(body.trim(), HARD_TOOL_CHARS)
                ));
            }
            other => {
                lines.push(format!(
                    "{other}: {}",
                    truncate_chars(m.text_content().trim(), HARD_MSG_CHARS)
                ));
            }
        }
    }

    lines.push(format!(
        "\n## Counts\nuser={user_n} assistant={asst_n} tool={tool_n} total={}",
        msgs.len()
    ));

    let mut out = lines.join("\n");
    if out.chars().count() > HARD_TOTAL_CHARS {
        // Keep head + tail so early user goal and late tool results survive.
        let head_budget = HARD_TOTAL_CHARS * 2 / 3;
        let tail_budget = HARD_TOTAL_CHARS / 3;
        let head: String = out.chars().take(head_budget).collect();
        let total_chars = out.chars().count();
        let tail: String = out
            .chars()
            .skip(total_chars.saturating_sub(tail_budget))
            .collect();
        out = format!("{head}\n…\n{tail}");
    }
    out
}

pub(super) fn strip_images(msg: &ChatMessage) -> ChatMessage {
    let mut cleaned = msg.clone();
    cleaned.parts = cleaned
        .parts
        .into_iter()
        .map(|part| match part {
            ContentPart::File {
                path, mime_type, ..
            } => ContentPart::Text {
                text: crate::providers::media::marker_for_file(&path, mime_type.as_deref()),
            },
            other => other,
        })
        .collect();
    cleaned
}
