//! Breakpoint detection — session recovery after interrupted turns.

use std::collections::HashMap;

use crate::providers::capability_chat::ChatMessage;

// ── Breakpoint detection (session recovery) ───────────────────────────────────

/// Describes a tool call that was initiated but never received a result
/// (e.g. the process was killed during tool execution).
#[derive(Debug, Clone)]
pub struct BreakpointItem {
    pub tool_call_id: String,
    pub tool_name: String,
    /// JSON-encoded arguments string.
    pub arguments: String,
}

/// Analyze session history and return tool calls that lack corresponding results.
///
/// Walks the entire message list, tracking which `tool_call_id`s were issued by
/// assistant messages and which received a `tool` result.  Any pending (unfulfilled)
/// tool calls represent "breakpoints" — places where execution was interrupted.
pub fn identify_breakpoint(messages: &[ChatMessage]) -> Vec<BreakpointItem> {
    let mut pending_tools: HashMap<String, (&str, &str)> = HashMap::new();

    for msg in messages {
        match msg.role.as_str() {
            "assistant" => {
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        pending_tools.insert(call.id.clone(), (&call.name, &call.arguments));
                    }
                }
            }
            "tool" => {
                if let Some(id) = &msg.tool_call_id {
                    pending_tools.remove(id.as_str());
                }
            }
            _ => {}
        }
    }

    pending_tools
        .into_iter()
        .map(|(id, (name, arguments))| BreakpointItem {
            tool_call_id: id,
            tool_name: name.to_string(),
            arguments: arguments.to_string(),
        })
        .collect()
}

/// Phase 2d (issue #256): the sole-orphan replay gate.
///
/// Returns `Some(tool_call_id)` only when the history contains exactly one
/// unresolved tool_call (`identify_breakpoint`), it is a `sessions_yield`,
/// and `pending_yield` names that same call — i.e. a *deliberately deferred*
/// yield that survived a restart, safe to replay lazily. Mixed orphans (a
/// `sessions_yield` alongside any other unresolved call) are a genuine
/// crash: `None` — recovery must take Case A, never replay.
pub fn sole_sessions_yield_orphan(
    messages: &[ChatMessage],
    pending_yield: Option<&crate::agents::session::PendingYield>,
) -> Option<String> {
    let breakpoints = identify_breakpoint(messages);
    if breakpoints.len() != 1 || breakpoints[0].tool_name != "sessions_yield" {
        return None;
    }
    let call_id = breakpoints[0].tool_call_id.clone();
    pending_yield
        .filter(|p| p.tool_call_id == call_id)
        .map(|_| call_id)
}

/// Check whether the message history ends with an incomplete assistant turn
/// (assistant emitted tool_calls but some/all are missing tool results).
///
/// This is a lighter check than [`identify_breakpoint`] — it only examines the
/// tail of the conversation rather than scanning the full history.
pub fn detect_incomplete_turn(messages: &[ChatMessage]) -> bool {
    messages
        .last()
        .map(|m| m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty()))
        .unwrap_or(false)
}
