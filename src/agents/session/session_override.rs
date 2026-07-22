//! Per-session runtime overrides applied by slash commands.

use crate::providers::capability_chat::ChatMessage;

// ── SessionOverride ───────────────────────────────────────────────────────────

/// Per-session runtime overrides applied by slash commands.
///
/// Each field is `None` = use global config default.
/// Persisted in `meta.json` so overrides survive restarts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionOverride {
    /// Force a specific model ID instead of the routing default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override thinking/reasoning mode. None = use model's `reasoning` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// Thinking effort level when thinking is enabled ("low"/"medium"/"high").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Override permission mode for this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::config::agent::PermissionMode>,
    /// Override run mode for this session (Interactive vs Background).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_mode: Option<crate::config::agent::RunMode>,
    /// Override max tool calls per turn (0 = unlimited).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<usize>,
    /// Override compaction trigger threshold (0.0..1.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<f64>,
    /// Override number of recent work units to retain during compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retain_work_units: Option<usize>,
    /// Replace the assembled system prompt with this string. Used by
    /// `DelegationCoordinator` so sub-agent turns can run through
    /// `SessionContext::process_turn` while still seeing their
    /// minimal AGENT.md identity prompt instead of the full builder
    /// output. Not persisted — set per-context-lifecycle.
    #[serde(skip)]
    pub system_prompt_override: Option<String>,
}

impl SessionOverride {
    /// Returns true if all fields are None (no active overrides).
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.thinking.is_none()
            && self.effort.is_none()
            && self.permission_mode.is_none()
            && self.run_mode.is_none()
            && self.max_tool_calls.is_none()
            && self.compact_threshold.is_none()
            && self.retain_work_units.is_none()
            && self.system_prompt_override.is_none()
    }

    /// Resolve the optional thinking/effort fields into a `ThinkingConfig`.
    /// `None` = use the model's own reasoning config (no override).
    pub fn to_thinking_config(&self) -> Option<crate::providers::ThinkingConfig> {
        match self.thinking {
            Some(true) => Some(crate::providers::ThinkingConfig {
                enabled: true,
                effort: self.effort.clone(),
            }),
            Some(false) => Some(crate::providers::ThinkingConfig {
                enabled: false,
                effort: None,
            }),
            None => None,
        }
    }
}

/// Remove orphan tool results (tool messages whose tool_call_id has no matching
/// assistant tool_call in the history). Also removes trailing assistant messages
/// with tool_calls that have no corresponding tool results (incomplete round,
/// e.g. process crashed during tool execution).
pub fn sanitize_history(history: &mut Vec<ChatMessage>) {
    // Step 1: Collect all tool_call_ids declared by assistant messages.
    let mut known_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in history.iter() {
        if msg.role == "assistant" {
            if let Some(ref tcs) = msg.tool_calls {
                for tc in tcs {
                    known_tool_ids.insert(tc.id.clone());
                }
            }
        }
    }

    // Step 2: Remove orphan tool results (no matching assistant tool_call).
    let before = history.len();
    history.retain(|msg| {
        if msg.role == "tool" {
            if let Some(ref tc_id) = msg.tool_call_id {
                return known_tool_ids.contains(tc_id);
            }
            return false;
        }
        true
    });

    let removed = before - history.len();
    if removed > 0 {
        tracing::warn!(removed, "sanitized orphan tool results from history");
    }

    // Step 3: Collect tool_call_ids that actually have a tool result.
    let fulfilled_ids: std::collections::HashSet<String> = history
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Step 4: Trim trailing assistant messages whose tool_calls are all unfulfilled.
    // These are left behind when the process crashes during tool execution.
    // Mid-history unfulfilled calls are closed in step 5 instead of dropped so
    // later user messages stay valid against providers that reject tool→user.
    let mut trimmed = 0;
    while let Some(last) = history.last() {
        if last.role == "assistant" {
            if let Some(ref tcs) = last.tool_calls {
                if !tcs.is_empty() && tcs.iter().all(|tc| !fulfilled_ids.contains(&tc.id)) {
                    history.pop();
                    trimmed += 1;
                    continue;
                }
            }
        }
        break;
    }
    if trimmed > 0 {
        tracing::warn!(
            trimmed,
            "sanitized trailing assistant messages with unfulfilled tool_calls"
        );
    }

    // Step 5: Close interrupted tool rounds that are followed by a later user/
    // assistant message (e.g. crash/hot-reload mid-tools, then a new user turn).
    // Providers like Gemini reject functionResponse without a subsequent model
    // turn, and reject functionCall without a matching functionResponse.
    close_interrupted_tool_rounds(history);

    // Step 6: Merge adjacent same-role user/assistant messages. Consecutive
    // `user` rows break Gemini / some OpenAI-compatible endpoints; consecutive
    // assistants are also invalid on strict alternation providers.
    merge_adjacent_same_role(history);

    // Step 7: Provider invariant — history must contain at least one user
    // message. Force-drop / crash recovery can leave assistant+tool-only tails
    // (GLM 1214 / messages 参数非法). Synthesize a minimal user turn.
    ensure_at_least_one_user(history);
}

/// Ensure `history` has ≥1 user message so providers that require a user turn
/// (and our own compaction/summary markers) never see an empty-of-user prefix.
fn ensure_at_least_one_user(history: &mut Vec<ChatMessage>) {
    if history.iter().any(|m| m.role == "user") {
        return;
    }
    if history.is_empty() {
        history.push(ChatMessage::user_text(
            "[CONTEXT RECOVERY] prior turns were dropped; continue from the latest request.",
        ));
        tracing::warn!("history empty after sanitize; injected recovery user message");
        return;
    }
    // Prepend so existing assistant/tool tail stays a legal continuation.
    history.insert(
        0,
        ChatMessage::user_text(
            "[CONTEXT RECOVERY] earlier user turns were dropped to fit the context window; treat the following as prior work product and continue.",
        ),
    );
    tracing::warn!(
        remaining = history.len() - 1,
        "history had no user message after sanitize; prepended recovery user"
    );
}

/// Insert cancelled tool results / placeholder assistant messages so a later
/// user turn never sits directly after an open tool round.
fn close_interrupted_tool_rounds(history: &mut Vec<ChatMessage>) {
    let mut i = 0;
    while i < history.len() {
        if history[i].role != "assistant" {
            i += 1;
            continue;
        }
        let Some(calls) = history[i].tool_calls.clone() else {
            i += 1;
            continue;
        };
        if calls.is_empty() {
            i += 1;
            continue;
        }

        // Gather fulfilled tool ids immediately following this assistant.
        let mut j = i + 1;
        let mut fulfilled: std::collections::HashSet<String> = std::collections::HashSet::new();
        while j < history.len() && history[j].role == "tool" {
            if let Some(ref id) = history[j].tool_call_id {
                fulfilled.insert(id.clone());
            }
            j += 1;
        }

        let missing: Vec<_> = calls
            .iter()
            .filter(|tc| !fulfilled.contains(&tc.id))
            .cloned()
            .collect();

        // Only rewrite when this tool round is not the live tail — the live tail
        // is either recovered (run_recovery) or closed by abort.
        let followed_by_later_turn = j < history.len();
        if !followed_by_later_turn {
            break;
        }

        if !missing.is_empty() {
            let mut insert_at = j;
            for tc in &missing {
                let mut msg = ChatMessage::text("tool", "cancelled: turn interrupted");
                msg.tool_call_id = Some(tc.id.clone());
                msg.name = Some(tc.name.clone());
                msg.is_error = Some(true);
                history.insert(insert_at, msg);
                insert_at += 1;
            }
            j = insert_at;
            tracing::warn!(
                missing = missing.len(),
                "sanitized unfulfilled tool_calls before a later turn"
            );
        }

        // Google maps tool results to role=user; a real user message right after
        // tools becomes adjacent user roles → 400. Insert a closing model turn.
        if j < history.len() && history[j].role == "user" {
            history.insert(j, ChatMessage::assistant_text("（已中断）"));
            tracing::warn!("inserted placeholder assistant between tool results and user");
            i = j + 2;
            continue;
        }
        i = j;
    }
}

/// Merge consecutive `user`/`assistant` messages (not tool/system) so providers
/// that require strict role alternation accept the request.
fn merge_adjacent_same_role(history: &mut Vec<ChatMessage>) {
    if history.len() < 2 {
        return;
    }
    let mut out: Vec<ChatMessage> = Vec::with_capacity(history.len());
    let mut merged = 0usize;
    for msg in history.drain(..) {
        let can_merge = matches!(msg.role.as_str(), "user" | "assistant")
            && msg.tool_calls.as_ref().is_none_or(|tc| tc.is_empty());
        if let Some(last) = out.last_mut() {
            let last_can_merge = matches!(last.role.as_str(), "user" | "assistant")
                && last.tool_calls.as_ref().is_none_or(|tc| tc.is_empty());
            if can_merge && last_can_merge && last.role == msg.role {
                // Keep non-text parts from both; separate text chunks with a blank line.
                let last_has_text = last.parts.iter().any(|p| {
                    matches!(p, crate::providers::ContentPart::Text { text } if !text.is_empty())
                });
                let msg_has_text = msg.parts.iter().any(|p| {
                    matches!(p, crate::providers::ContentPart::Text { text } if !text.is_empty())
                });
                if last_has_text && msg_has_text {
                    last.parts.push(crate::providers::ContentPart::Text {
                        text: "\n\n".to_string(),
                    });
                }
                last.parts.extend(msg.parts);
                // Prefer keeping model/usage metadata from the later message when present.
                if msg.model.is_some() {
                    last.model = msg.model;
                }
                if msg.usage.is_some() {
                    last.usage = msg.usage;
                }
                merged += 1;
                continue;
            }
        }
        out.push(msg);
    }
    if merged > 0 {
        tracing::warn!(merged, "merged adjacent same-role messages before model send");
    }
    *history = out;
}

/// Same as `sanitize_history` but keeps IDs paired with their messages throughout,
/// so the returned IDs correctly correspond to the surviving messages.
///
/// `sanitize_history` uses `retain()` which may remove messages from arbitrary
/// positions (not just the tail), so slicing the IDs array after the fact gives
/// wrong IDs. This variant avoids that by filtering both vecs together.
pub(super) fn sanitize_paired(pairs: Vec<(i64, ChatMessage)>) -> Vec<(i64, ChatMessage)> {
    // Step 1: Remove orphan tool results.
    let known_tool_ids: std::collections::HashSet<String> = pairs
        .iter()
        .filter(|(_, m)| m.role == "assistant")
        .flat_map(|(_, m)| m.tool_calls.iter().flatten().map(|tc| tc.id.clone()))
        .collect();

    let before = pairs.len();
    let mut result: Vec<_> = pairs
        .into_iter()
        .filter(|(_, msg)| {
            if msg.role == "tool" {
                return msg
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| known_tool_ids.contains(id));
            }
            true
        })
        .collect();

    // Step 2: Collect fulfilled tool_call_ids (those with actual tool results).
    let fulfilled_ids: std::collections::HashSet<String> = result
        .iter()
        .filter(|(_, m)| m.role == "tool")
        .filter_map(|(_, m)| m.tool_call_id.clone())
        .collect();

    // Step 3: Trim trailing assistant messages with all-unfulfilled tool_calls.
    let mut trimmed = 0;
    while let Some((_, last)) = result.last() {
        if last.role == "assistant" {
            if let Some(ref tcs) = last.tool_calls {
                if !tcs.is_empty() && tcs.iter().all(|tc| !fulfilled_ids.contains(&tc.id)) {
                    result.pop();
                    trimmed += 1;
                    continue;
                }
            }
        }
        break;
    }
    if trimmed > 0 {
        tracing::warn!(
            trimmed,
            "sanitize_paired: trimmed trailing assistant messages with unfulfilled tool_calls"
        );
    }

    let removed = before - result.len();
    if removed > 0 {
        tracing::warn!(removed, "sanitized orphan tool results from history");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::sanitize_history;
    use crate::providers::capability_chat::ChatMessage;
    use crate::providers::ToolCall;

    fn asst_tools(ids: &[&str]) -> ChatMessage {
        let mut msg = ChatMessage::assistant_text("call");
        msg.tool_calls = Some(
            ids.iter()
                .map(|id| ToolCall {
                    id: (*id).to_string(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                })
                .collect(),
        );
        msg
    }

    fn tool(id: &str) -> ChatMessage {
        let mut msg = ChatMessage::text("tool", "ok");
        msg.tool_call_id = Some(id.into());
        msg.name = Some("shell".into());
        msg
    }

    #[test]
    fn merges_adjacent_user_messages() {
        let mut history = vec![
            ChatMessage::user_text("one"),
            ChatMessage::user_text("two"),
            ChatMessage::assistant_text("ok"),
        ];
        sanitize_history(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert!(history[0].text_content().contains("one"));
        assert!(history[0].text_content().contains("two"));
        assert_eq!(history[1].role, "assistant");
    }

    #[test]
    fn closes_tool_round_before_later_user() {
        let mut history = vec![
            ChatMessage::user_text("hi"),
            asst_tools(&["t1"]),
            tool("t1"),
            ChatMessage::user_text("again"),
        ];
        sanitize_history(&mut history);
        // tool → placeholder assistant → user (merged path keeps all three roles)
        assert!(history.iter().any(|m| m.role == "assistant" && m.tool_calls.is_none()));
        assert_eq!(history.last().map(|m| m.role.as_str()), Some("user"));
        // No adjacent user/user after sanitize.
        for w in history.windows(2) {
            assert!(
                !(w[0].role == "user" && w[1].role == "user"),
                "adjacent user messages remain"
            );
            // tool must not be immediately followed by user
            assert!(
                !(w[0].role == "tool" && w[1].role == "user"),
                "tool directly followed by user"
            );
        }
    }

    #[test]
    fn injects_user_when_history_has_only_assistant_tool_tail() {
        // xiaoliu force-drop half left no user → GLM 1214.
        let mut history = vec![asst_tools(&["t1"]), tool("t1"), ChatMessage::assistant_text("mid")];
        sanitize_history(&mut history);
        assert!(
            history.iter().any(|m| m.role == "user"),
            "must inject recovery user: {:?}",
            history.iter().map(|m| m.role.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(history[0].role, "user");
        assert!(history[0].text_content().contains("CONTEXT RECOVERY"));
    }

    #[test]
    fn injects_user_when_history_empty() {
        let mut history: Vec<ChatMessage> = vec![];
        sanitize_history(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "user");
    }

    #[test]
    fn fills_missing_tool_results_mid_history() {
        let mut history = vec![
            ChatMessage::user_text("hi"),
            asst_tools(&["t1", "t2"]),
            tool("t1"),
            ChatMessage::user_text("later"),
        ];
        sanitize_history(&mut history);
        let tool_ids: Vec<_> = history
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        assert!(tool_ids.contains(&"t1".to_string()));
        assert!(tool_ids.contains(&"t2".to_string()));
    }
}
