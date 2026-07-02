//! DeepSeek provider — chat via OpenAI-compatible client.
//!
//! `deepseek_body_override` adds DeepSeek-specific thinking parameters and
//! reasoning_content echo for interleaved thinking with tool calls.
//!
//! Reference: https://api-docs.deepseek.com/guides/thinking_mode

use crate::providers::{ChatRequest, ContentPart};

/// DeepSeek body override.
///
/// 1. Echoes `reasoning_content` for assistant messages that contain tool_calls
///    (Interleaved Thinking — required for multi-turn tool-use).
/// 2. Adds `thinking: {"type":"enabled"}` when reasoning is on.
pub fn deepseek_body_override(
    mut body: serde_json::Value,
    req: &ChatRequest<'_>,
) -> serde_json::Value {
    use serde_json::json;

    // Inject reasoning_content into assistant messages with tool_calls.
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for (i, msg) in messages.iter_mut().enumerate() {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            // Only echo for messages that have tool_calls (Interleaved).
            if msg.get("tool_calls").is_none() {
                continue;
            }
            let orig = &req.messages[i];
            let reasoning: Vec<&str> = orig.parts.iter()
                .filter_map(|p| match p {
                    ContentPart::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            if !reasoning.is_empty() {
                msg["reasoning_content"] = json!(reasoning.join(""));
            }
        }
    }

    // Enable thinking mode when configured.
    if let Some(ref tc) = req.thinking {
        if tc.enabled {
            body["thinking"] = json!({"type": "enabled"});
        } else {
            body["thinking"] = json!({"type": "disabled"});
        }
    }

    body
}
