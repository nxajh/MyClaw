//! Qwen (Alibaba DashScope / Bailian) provider — chat via OpenAI-compatible client.
//!
//! `qwen_body_override` echoes `reasoning_content` for assistant messages
//! (required for multi-turn coherence with qwen3 thinking models) and
//! controls the `enable_thinking` parameter.
//!
//! Reference: https://help.aliyun.com/zh/model-studio/qwen-thinking-mode

use crate::providers::{ChatRequest, ContentPart};

/// Qwen body override.
///
/// 1. Echoes `reasoning_content` into assistant messages that have Thinking
///    parts, so the model can maintain reasoning context across turns.
/// 2. Sets `enable_thinking` based on the request's thinking config.
///    When thinking is enabled (default for qwen3 models), the API returns
///    reasoning in a separate `reasoning_content` field.
pub fn qwen_body_override(
    mut body: serde_json::Value,
    req: &ChatRequest<'_>,
) -> serde_json::Value {
    // Inject reasoning_content into assistant messages from Thinking parts.
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for (i, msg) in messages.iter_mut().enumerate() {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            let orig = &req.messages[i];
            let reasoning: Vec<&str> = orig
                .parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            if !reasoning.is_empty() {
                msg["reasoning_content"] = serde_json::json!(reasoning.join(""));
            }
        }
    }

    // Control thinking mode. Default: enabled (qwen3 models think by default).
    match &req.thinking {
        Some(tc) => {
            body["enable_thinking"] = serde_json::json!(tc.enabled);
        }
        None => {
            // No explicit thinking config — let the API default apply.
        }
    }

    body
}
