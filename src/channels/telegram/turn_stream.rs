//! Telegram streaming preview (edit-on-stream).
//!
//! Extracted from `channel.rs` (#151 Phase 8b): `TelegramTurnStream` —
//! the `TurnStream` implementation that live-edits a single Telegram
//! message as the turn progresses (progress mode: tool lines + status;
//! partial mode: accumulated text), plus its rendering helpers
//! (`escape_html`, `clip_detail`, `resolve_tool_display`,
//! `format_tool_line`, `tool_line_with_status`).
//!
//! The stream holds a `TelegramChannel` clone and drives its raw
//! `deleteMessage` wrapper; `channel.rs` constructs the stream in
//! `spawn_stream` (same module tree, `channels::telegram`).

use crate::api::turn_event::TurnEvent;
use crate::channels::{StreamDelivery, TurnStream};

use super::channel::TelegramChannel;

/// Throttle interval for streaming preview edits (edit-on-stream).
const STREAM_THROTTLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Maximum preview text length (codepoints) to stay under Telegram's 4096-char
/// limit for `sendMessage` / `editMessageText` during streaming.
const STREAM_PREVIEW_LIMIT: usize = 4000;

// ── Telegram streaming preview (edit-on-stream) ───────────────────────────────

/// Escape HTML special characters for Telegram parse_mode=HTML.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Clip detail text to max 300 chars (matching OpenClaw's clipTelegramProgressText).
fn clip_detail(s: &str) -> String {
    const MAX: usize = 300;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= MAX {
        return s.to_string();
    }
    let clipped: String = chars.into_iter().take(MAX - 1).collect();
    format!("{}…", clipped.trim_end())
}

/// Resolve (emoji, label, detail) for a tool call, OpenClaw-style.
fn resolve_tool_display(name: &str, args: &serde_json::Value) -> (String, String, String) {
    let key = name.to_lowercase();
    let (emoji, label) = match key.as_str() {
        "shell" => ("🛠️", "Shell"),
        "file_read" | "read_file" | "read" => ("📖", "Read"),
        "file_write" | "write_file" | "write" => ("✍️", "Write"),
        "file_edit" | "edit" | "patch" => ("📝", "Edit"),
        "web_search" => ("🔍", "Search"),
        "http_request" => ("🌐", "HTTP"),
        "content_search" | "grep" => ("🔎", "Grep"),
        "glob_search" | "glob" => ("📂", "Glob"),
        "list_dir" | "ls" => ("📂", "List"),
        "memory_manage" => ("🧠", "Memory"),
        "memory_search" => ("🧠", "Memory"),
        "agent_delegate" | "delegate" => ("✨", "Delegate"),
        "calculator" => ("🔢", "Calc"),
        "view_image" => ("🖼️", "Image"),
        "view_video" => ("🎬", "Video"),
        "hear_audio" => ("🎵", "Audio"),
        "skill_view" => ("📜", "Skill"),
        "skill_manage" => ("📜", "Skill"),
        "send_message" => ("💬", "Send"),
        "ask_user" => ("❓", "Ask"),
        "task_create" => ("📋", "Task"),
        "task_update" => ("📋", "Task"),
        "task_list" => ("📋", "Task"),
        "task_delete" => ("📋", "Task"),
        "shell_poll" => ("📊", "Poll"),
        _ => ("🔧", name),
    }
    .to_owned();

    // Extract a short detail string from the most relevant arg field.
    let detail_keys: &[&str] = match key.as_str() {
        "shell" | "shell_poll" => &["command", "cmd"],
        "file_read" | "read_file" | "read" => &["path", "file_path"],
        "file_write" | "write_file" | "write" => &["path", "file_path"],
        "file_edit" | "edit" | "patch" => &["path", "file_path"],
        "web_search" => &["query", "q"],
        "http_request" => &["url", "method"],
        "content_search" | "grep" => &["pattern", "regex"],
        "glob_search" | "glob" => &["pattern"],
        "list_dir" | "ls" => &["path"],
        "memory_manage" => &["name", "action"],
        "memory_search" => &["query"],
        "agent_delegate" | "delegate" => &["agent", "task"],
        "calculator" => &["expression"],
        "view_image" => &["path"],
        "view_video" => &["path"],
        "hear_audio" => &["path"],
        "skill_view" | "skill_manage" => &["name", "action"],
        "task_create" => &["subject"],
        "task_update" => &["task_id", "status"],
        "task_delete" => &["task_id"],
        _ => &[],
    };

    let detail = detail_keys
        .iter()
        .find_map(|&k| {
            args.get(k)
                .and_then(|v| v.as_str())
                .map(|s| {
                    let s = s.trim();
                    if s.chars().count() > 50 {
                        let truncated: String = s.chars().take(47).collect();
                        format!("{truncated}…")
                    } else {
                        s.to_string()
                    }
                })
        })
        .unwrap_or_default();

    (emoji.to_string(), label.to_string(), detail)
}

/// Format a single tool-call progress line as Telegram Markdown.
///
/// Output: `**📖 Read** \`/path/to/file\``
/// With optional status: `… _failed_`
fn format_tool_line(name: &str, args: &serde_json::Value) -> String {
    let (emoji, label, detail) = resolve_tool_display(name, args);
    let label_full = format!("{emoji} {label}");
    if detail.is_empty() {
        format!("**{label_full}**")
    } else {
        let detail_clipped = clip_detail(&detail);
        format!("**{label_full}** `{detail_clipped}`")
    }
}

/// Re-format a tool line to append a status suffix (e.g. `_failed_`).
fn tool_line_with_status(line: &str, success: bool) -> String {
    if success {
        line.to_string()
    } else {
        format!("{line} _failed_")
    }
}

/// Per-turn streaming handle for Telegram.
///
/// Two modes:
/// - **Partial**: accumulates ALL text chunks and live-edits a preview
///   message. The final edit replaces it with the complete answer.
/// - **Progress**: shows only tool-call progress lines with per-tool emoji,
///   label, and arg detail (e.g. `📖 Read /path`), rendered as rich markdown.
///   When the turn completes, the preview collapses to a one-line summary
///   (e.g. `🛠️ 4 tool calls · ⏱️ 21s`) and the final answer is sent as a
///   separate message via the normal `send_message` path.
struct TelegramTurnStream {
    channel: TelegramChannel,
    chat_id: i64,
    thread_id: Option<String>,
    /// Original reply_target — used to remove from `streaming_targets`.
    reply_target: String,
    mode: crate::config::channel::StreamingMode,
    /// Message being live-edited; `None` until first flush.
    msg_id: Option<i64>,
    /// Accumulated text (partial mode only).
    accumulated: String,
    /// Tool-call progress lines (progress mode): `["🔧 file_read", …]`.
    tool_lines: Vec<String>,
    /// Tool-call count (progress mode, for collapse summary).
    tool_count: usize,
    /// Thinking step count (progress mode, for collapse summary).
    thinking_steps: usize,
    /// Commentary notes count (progress mode, for collapse summary).
    commentary_notes: usize,
    /// Estimated thinking token count for the current round (progress mode).
    thinking_tokens: usize,
    /// Whether thinking is currently active (progress mode).
    thinking_active: bool,
    /// Pending commentary text accumulated from Chunk events; flushed to a
    /// 💬 line when a ToolCall arrives (text before tools = commentary;
    /// text after last tool = final answer, discarded on Done).
    pending_commentary: String,
    /// 单 preview (2026-08-12): body of the preview message taken over from
    /// a previous (origin) turn — rendered as the leading block so prior
    /// progress lines stay visible (保留历史行追加). `None` on fresh streams.
    inherited_preview: Option<String>,
    /// 单 preview (2026-08-12): intermediate (silenced) resume turn — `Done`
    /// keeps the preview lines (no collapse); the final resume turn
    /// collapses. Set via `TurnStream::defer_collapse`.
    defer_collapse: bool,
    /// 单 preview (2026-08-12): FINAL (loud) resume turn of an
    /// async-delegation suspension that took over the origin's preview —
    /// `Done` collapses it into the one-line summary; the final answer is
    /// delivered by the `send_message` fallback as a separate message.
    /// Set via `TurnStream::final_takeover`.
    final_takeover: bool,
    /// Turn start time (progress mode, for collapse summary).
    start: std::time::Instant,
    /// 单 preview (2026-08-12): wall-clock start (unix seconds) of the
    /// ORIGIN turn of the whole suspension flow — taken-over streams keep it
    /// so the summary's ⏱️ spans the whole message, not just the last turn.
    absolute_start: std::time::SystemTime,
    last_edit: std::time::Instant,
    delivery: StreamDelivery,
    finished: bool,
}

impl TelegramTurnStream {
    /// Build a fresh stream (#151 Phase 8b: constructor moved here with the
    /// struct — `channel.rs`'s `create_stream_folding` calls it; field
    /// initializers stay next to the field definitions).
    ///
    /// 单 preview (2026-08-12): the fold carries the cumulative counters
    /// and wall-clock start so the taken-over stream keeps counting the
    /// WHOLE message across turns (origin → silenced notice → final loud)
    /// — the FINAL summary line must reflect everything, not just the
    /// last turn's activity ("summary 没有累计", user-confirmed).
    pub(crate) fn new_stream(
        channel: TelegramChannel,
        chat_id: i64,
        thread_id: Option<String>,
        reply_target: &str,
        mode: crate::config::channel::StreamingMode,
        fold: Option<crate::channels::FoldCandidate>,
    ) -> Self {
        let (fold_msg_id, inherited, fold_steps, fold_tools, fold_notes, fold_started_at) =
            match fold {
                Some(f) => (
                    f.msg_id.parse::<i64>().ok(),
                    Some(f.text),
                    f.thinking_steps,
                    f.tool_count,
                    f.commentary_notes,
                    f.started_at_unix_secs,
                ),
                None => (None, None, 0, 0, 0, None),
            };
        // Re-anchor `start` to the origin's wall-clock moment so the
        // summary's ⏱️ spans the whole flow (including sub-agent runtime).
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let start = match fold_started_at {
            Some(u) => {
                std::time::Instant::now() - std::time::Duration::from_secs(now_unix.saturating_sub(u))
            }
            None => std::time::Instant::now(),
        };
        let absolute_start = match fold_started_at {
            Some(u) => std::time::UNIX_EPOCH + std::time::Duration::from_secs(u),
            None => std::time::SystemTime::now(),
        };

        Self {
            channel,
            chat_id,
            thread_id,
            reply_target: reply_target.to_string(),
            mode,
            msg_id: fold_msg_id,
            // Partial mode: seed accumulated with the inherited body so a
            // resumed turn keeps prior text instead of replacing it.
            accumulated: if mode == crate::config::channel::StreamingMode::Partial {
                inherited.clone().unwrap_or_default()
            } else {
                String::new()
            },
            tool_lines: Vec::new(),
            tool_count: fold_tools,
            thinking_steps: fold_steps,
            commentary_notes: fold_notes,
            thinking_tokens: 0,
            thinking_active: false,
            pending_commentary: String::new(),
            inherited_preview: inherited,
            defer_collapse: false,
            final_takeover: false,
            start,
            absolute_start,
            last_edit: std::time::Instant::now() - STREAM_THROTTLE,
            delivery: StreamDelivery::Pending,
            finished: false,
        }
    }

    fn is_progress(&self) -> bool {
        self.mode == crate::config::channel::StreamingMode::Progress
    }

    /// 单 preview (2026-08-12): the Done-event text for a resume turn —
    /// streamed commentary wins when present, else the provider's final
    /// `text` (non-streaming fallback). Pure so tests can pin it.
    fn done_note(&mut self, text: String) -> String {
        if !self.pending_commentary.trim().is_empty() {
            std::mem::take(&mut self.pending_commentary)
        } else {
            text
        }
    }

    /// Flush the current thinking round into the step list as a retained line.
    fn flush_completed_thinking(&mut self) {
        if self.thinking_active {
            self.thinking_active = false;
            if self.thinking_tokens > 0 {
                self.tool_lines.push(format!(
                    "🧠 Thinking… (~{} tokens)",
                    self.thinking_tokens
                ));
            }
            self.thinking_tokens = 0;
        }
    }

    /// Build the preview text for the current mode.
    fn preview_text(&self) -> String {
        if self.is_progress() {
            // 单 preview: inherited body (from the origin turn) becomes the
            // leading lines — prior progress is kept VERBATIM (no clip), new
            // lines append below (保留历史行追加). Under STREAM_PREVIEW_LIMIT
            // the OLDEST lines (inherited first, then this turn's tool lines)
            // are dropped with a "… N earlier" marker so the newest content
            // always wins. The previous 300-char clip of the WHOLE body wiped
            // most of the origin's progress on takeover ("origin progress 被
            // 恢复轮覆盖", user-confirmed).
            let mut body: Vec<String> = self
                .inherited_preview
                .as_deref()
                .map(|inh| {
                    inh.trim()
                        .split("\n\n")
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            // Headline: pending commentary shown as bold when no steps yet.
            if !self.pending_commentary.trim().is_empty()
                && self.tool_lines.is_empty()
                && !self.thinking_active
            {
                let text = clip_detail(self.pending_commentary.trim());
                body.push(format!("**{}**", text));
            }

            // Tail: pending commentary (💬) and live thinking (🧠).
            let mut tail = Vec::new();
            if !self.pending_commentary.trim().is_empty() && !self.tool_lines.is_empty() {
                let text = clip_detail(self.pending_commentary.trim());
                tail.push(format!("💬 {}", text));
            }
            // Live thinking line at end (most recent activity).
            if self.thinking_active && self.thinking_tokens > 0 {
                tail.push(format!(
                    "🧠 Thinking… (~{} tokens)",
                    self.thinking_tokens
                ));
            }

            // Truncate oldest lines so total preview stays under
            // STREAM_PREVIEW_LIMIT (Telegram editMessageText 4096-char cap).
            // The tail always survives; skipping starts at the inherited head.
            let all: Vec<&String> = body.iter().chain(self.tool_lines.iter()).collect();
            let total = all.len();
            let mut skip = total;
            for s in 0..total {
                let mut test: Vec<String> = all[s..].iter().map(|x| (*x).clone()).collect();
                test.extend(tail.clone());
                if test.join("\n\n").chars().count() <= STREAM_PREVIEW_LIMIT {
                    skip = s;
                    break;
                }
            }

            let mut lines = Vec::new();
            if skip > 0 && skip < total {
                lines.push(format!("… {} earlier", skip));
            }
            lines.extend(all[skip..].iter().map(|x| (*x).clone()));
            lines.extend(tail);

            lines.join("\n\n")
        } else {
            self.accumulated.chars().take(STREAM_PREVIEW_LIMIT).collect()
        }
    }

    /// Send or edit the preview message.
    async fn flush_preview(&mut self) {
        let preview = self.preview_text();
        if preview.is_empty() {
            return;
        }
        // Loop so an edit failure (message deleted server-side, e.g. the
        // taken-over preview was removed) falls back to sending a new message
        // in the same flush instead of wedging the stream on a dead msg_id.
        loop {
            match self.msg_id {
                Some(mid) => {
                    if self
                        .channel
                        .edit_message_rich(self.chat_id, mid, &preview)
                        .await
                        .is_ok()
                    {
                        self.delivery = StreamDelivery::Visible;
                        break;
                    }
                    // Edit failed — drop the stale id and retry as a send.
                    self.msg_id = None;
                }
                None => {
                    if let Ok(Some(id)) = self
                        .channel
                        .send_rich_message_simple(
                            &self.chat_id.to_string(),
                            &preview,
                            self.thread_id.as_deref(),
                        )
                        .await
                    {
                        self.msg_id = Some(id);
                        self.delivery = StreamDelivery::Visible;
                    }
                    break;
                }
            }
        }
        self.last_edit = std::time::Instant::now();
    }

    /// Delete the preview message (transition to `send_message` fallback).
    async fn delete_preview(&mut self) {
        if let Some(mid) = self.msg_id.take() {
            let _ = self.channel.delete_message_raw(self.chat_id, mid).await;
        }
    }

    /// Build the collapse summary line, OpenClaw-style.
    ///
    /// `🧠 2 thoughts · 🛠️ 4 tool calls · ⏱️ 21s`
    fn collapse_summary(&self) -> String {
        let elapsed = self.start.elapsed().as_secs().max(1);
        let mut parts = Vec::new();
        if self.thinking_steps > 0 {
            let plural = if self.thinking_steps == 1 { "thought" } else { "thoughts" };
            parts.push(format!("🧠 {} {plural}", self.thinking_steps));
        }
        if self.commentary_notes > 0 {
            let plural = if self.commentary_notes == 1 { "note" } else { "notes" };
            parts.push(format!("💬 {} {plural}", self.commentary_notes));
        }
        if self.tool_count > 0 {
            let plural = if self.tool_count == 1 { "tool call" } else { "tool calls" };
            parts.push(format!("🛠️ {} {plural}", self.tool_count));
        }
        parts.push(format!("⏱️ {elapsed}s"));
        parts.join(" · ")
    }

    /// Edit the preview message into a collapse summary.
    async fn collapse_to_summary(&mut self) {
        let summary = self.collapse_summary();
        if let Some(mid) = self.msg_id {
            if self
                .channel
                .edit_message_rich(self.chat_id, mid, &summary)
                .await
                .is_ok()
            {
                return;
            }
        }
        // Fallback: delete if edit failed or no msg_id.
        self.delete_preview().await;
    }

    /// Remove this target from the streaming tracker.
    fn untrack(&self) {
        self.channel
            .streaming_targets
            .lock()
            .remove(&self.reply_target);
    }
}

#[async_trait]
impl TurnStream for TelegramTurnStream {
    async fn push(&mut self, event: TurnEvent) -> anyhow::Result<StreamDelivery> {
        if self.finished {
            return Ok(self.delivery);
        }

        if self.is_progress() {
            // ── Progress mode ───────────────────────────────────────────────
            match event {
                TurnEvent::Chunk { delta } => {
                    // Thinking ends when text starts; retain completed round.
                    self.flush_completed_thinking();
                    // Accumulate text chunks. If a tool call follows, this
                    // text was commentary (intermediate explanation); if Done
                    // follows, it was the final answer streaming (discarded).
                    self.pending_commentary.push_str(&delta);
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::ToolCall { name, args, .. } => {
                    // Thinking ends when a tool call starts; retain completed round.
                    self.flush_completed_thinking();
                    // Flush pending commentary as a 💬 line before the tool call.
                    if !self.pending_commentary.trim().is_empty() {
                        self.commentary_notes += 1;
                        let text = clip_detail(self.pending_commentary.trim());
                        self.tool_lines
                            .push(format!("💬 {}", text));
                        self.pending_commentary.clear();
                    }
                    self.tool_count += 1;
                    self.tool_lines.push(format_tool_line(&name, &args));
                    // Throttle: avoid edit-storm on rapid tool calls.
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::Thinking { delta } => {
                    // Count bursts (thinking rounds), not individual deltas.
                    // Each transition from non-thinking to thinking is one round.
                    if !self.thinking_active {
                        // Flush pending commentary before new thinking round
                        // (preserves chronological ordering in step list).
                        if !self.pending_commentary.trim().is_empty() {
                            self.commentary_notes += 1;
                            let text = clip_detail(self.pending_commentary.trim());
                            self.tool_lines.push(format!("💬 {}", text));
                            self.pending_commentary.clear();
                        }
                        self.thinking_steps += 1;
                        self.thinking_tokens = 0;
                    }
                    self.thinking_active = true;
                    // Rough token estimate: ~1 token per 4 chars, minimum 1 per event.
                    let est = (delta.len() / 4).max(1);
                    self.thinking_tokens += est;
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::ToolResult { name, is_error, .. } => {
                    // Annotate the matching tool line with `_failed_`
                    // (OpenClaw-style) using the tool layer's own
                    // success/error signal — not text-sniffed from output,
                    // which false-positives on content merely mentioning
                    // words like "failed:" (issue #103).
                    if is_error {
                        // Find the last line for this tool name.
                        let label = resolve_tool_display(&name, &serde_json::Value::Null).1;
                        if let Some(line) = self
                            .tool_lines
                            .iter_mut()
                            .rev()
                            .find(|l| l.contains(&label) && !l.contains("_failed_"))
                        {
                            *line = tool_line_with_status(line, false);
                        }
                    }
                }
                TurnEvent::Done { text } => {
                    if self.defer_collapse {
                        // 单 preview (2026-08-12): KEEP the preview in
                        // progress form — two callers:
                        //   • silenced resume turn: the model output is
                        //     intermediate progress, appended as a 💬 line;
                        //   • the ORIGIN turn that spawned async delegations
                        //     (set in Agent::run): its preview must survive
                        //     for the notice turns to take over — collapsing
                        //     here would surface the summary too early
                        //     ("先 summary 再 progress", user-confirmed).
                        // The FINAL resume turn collapses (final_takeover →
                        // summary; the answer is delivered as a separate
                        // message). `pending_commentary` carries the
                        // streamed text; `text` is the fallback for
                        // non-streaming providers.
                        let note = self.done_note(text);
                        if !note.trim().is_empty() {
                            self.commentary_notes += 1;
                            self.tool_lines
                                .push(format!("💬 {}", clip_detail(note.trim())));
                        }
                        self.flush_preview().await;
                        self.finished = true;
                    } else if self.final_takeover {
                        // 单 preview (2026-08-12): FINAL loud resume turn —
                        // collapse the taken-over preview into the one-line
                        // summary (the origin's progress message ENDS as the
                        // summary — "最终才 summary"). The final answer is NOT
                        // merged into this message: `process_turn`'s fallback
                        // (`suspended_turn == false` on the loud final wheel)
                        // delivers `turn_result.text` as a SEPARATE message
                        // right after — user-confirmed final shape is TWO
                        // messages: summary + standalone answer. Do NOT report
                        // FinalDelivered here — that would suppress the
                        // fallback and lose the answer.
                        self.collapse_to_summary().await;
                        self.finished = true;
                    } else {
                        // Collapse preview into a one-line summary; the final
                        // answer is sent by `send_message` (delivery != FinalDelivered).
                        self.collapse_to_summary().await;
                        self.finished = true;
                    }
                }
                TurnEvent::Cancelled { .. }
                | TurnEvent::Error { .. }
                | TurnEvent::EmptyResponse { .. } => {
                    self.delete_preview().await;
                    self.finished = true;
                }
            }
        } else {
            // ── Partial mode (legacy) ──────────────────────────────────────
            match event {
                TurnEvent::Chunk { delta } => {
                    self.accumulated.push_str(&delta);
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::Done { text } => {
                    if self.defer_collapse {
                        // 单 preview (silenced resume turn): append the turn's
                        // intermediate text to the inherited body; keep the
                        // message (no delete, no FinalDelivered — the
                        // suspended-turn gate skips the fallback send).
                        self.accumulated.push_str(&text);
                        self.finished = true;
                        self.flush_preview().await;
                    } else {
                        self.accumulated = text;
                        self.finished = true;
                        if self.accumulated.chars().count() > STREAM_PREVIEW_LIMIT {
                            self.delete_preview().await;
                            // Leave delivery as Visible/Pending → triggers fallback.
                        } else {
                            self.flush_preview().await;
                            self.delivery = StreamDelivery::FinalDelivered;
                        }
                    }
                }
                TurnEvent::Error { .. } | TurnEvent::EmptyResponse { .. } => {
                    self.finished = true;
                }
                TurnEvent::Cancelled { partial } => {
                    self.accumulated = partial;
                    self.finished = true;
                    self.flush_preview().await;
                }
                _ => {}
            }
        }
        Ok(self.delivery)
    }

    fn status(&self) -> StreamDelivery {
        self.delivery
    }

    fn fold_candidate(&self) -> Option<FoldCandidate> {
        // Only a flushed preview message can be repurposed; if it was
        // deleted (partial mode past the 4096 cap) there is nothing to fold.
        let msg_id = self.msg_id?;
        // Report what the user currently sees: progress mode collapses to
        // the one-line summary on Done — EXCEPT for defer_collapse resume
        // turns, which keep the full preview lines (单 preview takeover
        // needs the real body to seed the next turn's inherited history).
        let text = if self.finished && self.is_progress() && !self.defer_collapse {
            self.collapse_summary()
        } else {
            self.preview_text()
        };
        Some(FoldCandidate {
            msg_id: msg_id.to_string(),
            text,
            // 单 preview (2026-08-12): cumulative counters + wall-clock start
            // ride along so a taken-over stream keeps counting the WHOLE
            // message across turns ("summary 没有累计", user-confirmed).
            thinking_steps: self.thinking_steps,
            tool_count: self.tool_count,
            commentary_notes: self.commentary_notes,
            started_at_unix_secs: self
                .absolute_start
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs()),
        })
    }

    fn defer_collapse(&mut self) {
        self.defer_collapse = true;
    }

    fn final_takeover(&mut self) {
        self.final_takeover = true;
    }

    async fn finish(self: Box<Self>) -> StreamDelivery {
        let mut s = *self;
        s.untrack();
        if !s.finished && !s.accumulated.is_empty() {
            s.flush_preview().await;
        }
        s.delivery
    }

    async fn abort(self: Box<Self>) {
        self.untrack();
        // Best-effort: delete the preview message if it was never finalized.
        if let (Some(mid), false) = (self.msg_id, self.finished) {
            let _ = self.channel.delete_message_raw(self.chat_id, mid).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::channel::tests::make_config;

    /// Test helper: a fresh stream in the default (Pending, unfinished) state.
    fn make_stream() -> TelegramTurnStream {
        TelegramTurnStream {
            channel: TelegramChannel::new(make_config()),
            chat_id: 42,
            thread_id: None,
            reply_target: "t".to_string(),
            mode: crate::config::channel::StreamingMode::Partial,
            msg_id: None,
            accumulated: String::new(),
            tool_lines: vec![],
            tool_count: 0,
            thinking_steps: 0,
            commentary_notes: 0,
            thinking_tokens: 0,
            thinking_active: false,
            pending_commentary: String::new(),
            inherited_preview: None,
            defer_collapse: false,
            final_takeover: false,
            start: std::time::Instant::now(),
            absolute_start: std::time::SystemTime::now(),
            last_edit: std::time::Instant::now(),
            delivery: StreamDelivery::Pending,
            finished: false,
        }
    }

    /// issue #103: a successful tool result whose *output text* happens to
    /// contain a failure-sniff trigger word ("failed:") must NOT be marked
    /// `_failed_` — the structured `is_error` flag decides, not text sniffing.
    #[tokio::test]
    async fn tool_result_success_is_not_marked_failed_despite_output_text() {
        let mut s = make_stream();
        s.mode = crate::config::channel::StreamingMode::Progress;
        s.push(TurnEvent::ToolCall {
            id: "1".into(),
            name: "skill_view".into(),
            args: serde_json::json!({"name": "github"}),
        })
        .await
        .unwrap();
        s.push(TurnEvent::ToolResult {
            id: "1".into(),
            name: "skill_view".into(),
            output: "View a run and see which steps failed:\n...".into(),
            is_error: false,
        })
        .await
        .unwrap();
        assert!(
            s.tool_lines.iter().all(|l| !l.contains("_failed_")),
            "successful tool result was falsely annotated as failed: {:?}",
            s.tool_lines
        );
    }

    /// issue #103: a genuinely failed tool result must still be annotated,
    /// even when its output text doesn't start with "error"/"Error" or
    /// contain any of the old sniff-rule substrings.
    #[tokio::test]
    async fn tool_result_error_is_marked_failed_via_flag() {
        let mut s = make_stream();
        s.mode = crate::config::channel::StreamingMode::Progress;
        s.push(TurnEvent::ToolCall {
            id: "1".into(),
            name: "skill_view".into(),
            args: serde_json::json!({"name": "github"}),
        })
        .await
        .unwrap();
        s.push(TurnEvent::ToolResult {
            id: "1".into(),
            name: "skill_view".into(),
            output: "skill not found".into(),
            is_error: true,
        })
        .await
        .unwrap();
        assert!(
            s.tool_lines.iter().any(|l| l.contains("_failed_")),
            "genuinely failed tool result was not annotated: {:?}",
            s.tool_lines
        );
    }

    /// 单 preview (2026-08-12): the inherited origin body is kept VERBATIM
    /// in the resumed preview — the old 300-char clip of the whole body used
    /// to wipe most of the origin's progress on takeover ("origin progress
    /// 被恢复轮覆盖", user-confirmed).
    #[test]
    fn preview_text_keeps_inherited_body_verbatim() {
        let mut s = make_stream();
        s.mode = crate::config::channel::StreamingMode::Progress;
        let long_body = (0..20)
            .map(|i| format!("line {i}: {}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(long_body.chars().count() > 300);
        s.inherited_preview = Some(long_body.clone());
        let out = s.preview_text();
        assert!(out.contains("line 0:"));
        assert!(out.contains("line 19:"));
        // No clip: the whole body survives (plus separator joins).
        assert!(out.chars().count() >= long_body.chars().count());
    }

    /// 单 preview (2026-08-12): under STREAM_PREVIEW_LIMIT the OLDEST lines
    /// (inherited head first) are dropped with the "… N earlier" marker —
    /// newest content (this turn's tool lines) always survives.
    #[test]
    fn preview_text_truncates_oldest_lines_under_cap() {
        let mut s = make_stream();
        s.mode = crate::config::channel::StreamingMode::Progress;
        let big_body = (0..80)
            .map(|i| format!("row {i}: {}", "y".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n\n");
        s.inherited_preview = Some(big_body);
        s.tool_lines = vec!["🔧 newest-tool".to_string()];
        let out = s.preview_text();
        assert!(out.contains("newest-tool"));
        assert!(out.contains("earlier"));
        assert!(out.chars().count() <= STREAM_PREVIEW_LIMIT);
    }

    /// 单 preview (2026-08-12): `TurnStream::final_takeover` marks the stream
    /// so the FINAL loud resume turn collapses the taken-over preview into
    /// the summary line; the final answer is delivered by `process_turn`'s
    /// fallback as a SEPARATE message (user-confirmed shape: 2 messages).
    #[test]
    fn final_takeover_marks_stream() {
        let mut s = make_stream();
        assert!(!s.final_takeover);
        s.final_takeover();
        assert!(s.final_takeover);
    }

    /// 单 preview (2026-08-12): `done_note` prefers the streamed commentary
    /// (draining it) over the provider's final `text` — the non-streaming
    /// fallback. Pure, so it is testable without network.
    #[test]
    fn done_note_uses_pending_commentary_over_text() {
        let mut s = make_stream();
        s.pending_commentary = "  streamed line  ".to_string();
        let note = s.done_note("provider final text".to_string());
        assert_eq!(note, "  streamed line  ");
        assert!(s.pending_commentary.is_empty());

        // No streamed commentary → provider text wins.
        let mut s2 = make_stream();
        assert_eq!(
            s2.done_note("provider final text".to_string()),
            "provider final text"
        );
    }

}
