//! Xiaomi MiMo provider — dual-protocol: Anthropic or OpenAI.
//!
//! The protocol is determined by the user's config (`protocol = "anthropic"` or
//! `protocol = "openai"`). When Anthropic, uses mimo-v2.5-pro with MiMo-specific
//! thinking patches. When OpenAI, uses the standard OpenAI Chat Completions
//! client which supports text, image, video, and audio inputs.

use async_trait::async_trait;

use crate::providers::{BoxStream, ChatProvider, ChatRequest, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.xiaomimimo.com/anthropic";

#[derive(Clone)]
pub struct XiaomiProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
    openai: bool,
}

impl XiaomiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            user_agent: None,
            openai: false,
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            base_url,
            api_key,
            user_agent: None,
            openai: false,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    /// When set, requests are sent via OpenAI Chat Completions protocol.
    pub fn with_openai(mut self) -> Self {
        self.openai = true;
        self
    }

    /// Derive the OpenAI-compatible base URL from the Anthropic base URL.
    /// e.g. `https://api.xiaomimimo.com/anthropic` → `https://api.xiaomimimo.com`
    fn openai_base_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if let Some(pos) = base.rfind("/anthropic") {
            base[..pos].to_string()
        } else {
            base.to_string()
        }
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

        let has_thinking = content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"));
        let has_tool_use = content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"));

        if !has_thinking && has_tool_use {
            content.insert(0, serde_json::json!({"type": "thinking", "thinking": ""}));
        }
    }
}

#[async_trait]
impl ChatProvider for XiaomiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        if self.openai {
            // ── OpenAI path ──────────────────────────────────────────────
            use crate::providers::protocols::openai::chat_completions::OpenAiChatCompletionsClient;

            let openai_base = self.openai_base_url();
            tracing::debug!(
                base_url = %openai_base,
                model = %req.model,
                "XiaomiProvider: using OpenAI protocol"
            );

            let client = OpenAiChatCompletionsClient::new(
                self.api_key.clone(),
                openai_base,
            );
            let client = if let Some(ref ua) = self.user_agent {
                client.with_user_agent(ua.clone())
            } else {
                client
            };
            client.chat(req)
        } else {
            // ── Anthropic path (default) ─────────────────────────────────
            use crate::providers::protocols::anthropic::messages::AnthropicMessagesClient;

            let thinking_enabled = req.thinking.as_ref().is_some_and(|t| t.enabled);
            let mut body = crate::providers::protocols::anthropic::message_rendering::build_anthropic_body(&req);

            // MiMo-specific: MiMo ALWAYS requires a thinking block in every
            // assistant message that contains tool_use.
            patch_mimo_thinking(&mut body);

            let client =
                AnthropicMessagesClient::new(self.api_key.clone(), self.base_url.clone());
            let client = if let Some(ref ua) = self.user_agent {
                client.with_user_agent(ua.clone())
            } else {
                client
            };
            client.chat_with_body(body, thinking_enabled)
        }
    }
}
