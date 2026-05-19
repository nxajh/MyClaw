//! Xiaomi MiMo provider — implements ChatProvider via Anthropic-compatible API.
//!
//! Xiaomi MiMo uses the Anthropic Messages API protocol.
//! Endpoint: https://api.xiaomimimo.com/anthropic/v1/messages
//! Auth: Bearer token or api-key header.
//!
//! Key differences from Anthropic:
//! - Different base URL
//! - Usage includes `cache_read_input_tokens`
//! - Extra stop reason: `repetition_truncation`
//! - MiMo requires `reasoning_content` on every assistant tool_call message
//!   when thinking mode is enabled (empty thinking block inserted if missing).
//!
//! The SSE parsing is identical to Anthropic, so this provider delegates
//! to `AnthropicMessagesClient` from the protocols layer.

use async_trait::async_trait;

use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, StreamEvent,
};
use crate::providers::protocols::anthropic::message_rendering::build_anthropic_body;

const DEFAULT_BASE_URL: &str = "https://api.xiaomimimo.com/anthropic";

#[derive(Clone)]
pub struct XiaomiProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
}

impl XiaomiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            user_agent: None,
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            base_url,
            api_key,
            user_agent: None,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }
}

/// MiMo requires that every assistant message with tool_calls also contains a
/// `reasoning_content` (thinking) block when thinking mode is active. If the
/// model didn't produce one (or it was lost during compaction), we insert an
/// empty thinking block to avoid a 400 error from the MiMo API.
fn patch_mimo_thinking(body: &mut serde_json::Value) {
    let messages = match body.get_mut("messages") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => return,
    };

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let content = match msg.get_mut("content") {
            Some(serde_json::Value::Array(arr)) => arr,
            _ => continue,
        };

        let has_thinking = content.iter().any(|b| {
            b.get("type").and_then(|v| v.as_str()) == Some("thinking")
        });
        let has_tool_use = content.iter().any(|b| {
            b.get("type").and_then(|v| v.as_str()) == Some("tool_use")
        });

        if !has_thinking && has_tool_use {
            content.insert(0, serde_json::json!({"type": "thinking", "thinking": ""}));
        }
    }
}

#[async_trait]
impl ChatProvider for XiaomiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        use crate::providers::protocols::anthropic::messages::AnthropicMessagesClient;

        let thinking_enabled = req.thinking.as_ref().is_some_and(|t| t.enabled);
        let mut body = build_anthropic_body(&req);

        // MiMo-specific: ensure every assistant tool_call message has a thinking block.
        if thinking_enabled {
            patch_mimo_thinking(&mut body);
        }

        let client = AnthropicMessagesClient::new(self.api_key.clone(), self.base_url.clone());
        let client = if let Some(ref ua) = self.user_agent {
            client.with_user_agent(ua.clone())
        } else {
            client
        };
        client.chat_with_body(body, thinking_enabled)
    }
}
