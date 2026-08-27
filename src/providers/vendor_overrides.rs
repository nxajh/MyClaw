//! Vendor-specific provider overrides and thin vendor shells.
//!
//! Merged from `anthropic.rs` / `deepseek.rs` / `qwen.rs` / `kimi.rs`
//! (#151 Phase 10, pure relocation — no behavior change).

use async_trait::async_trait;

use crate::providers::{BoxStream, ChatProvider, ChatRequest, ContentPart, StreamEvent};

// ── Anthropic (was anthropic.rs) ─────────────────────────────────────────────

const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            base_url: ANTHROPIC_DEFAULT_BASE_URL.to_string(),
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

#[async_trait]
impl ChatProvider for AnthropicProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        use crate::providers::protocols::anthropic::messages::AnthropicMessagesClient;
        let client = AnthropicMessagesClient::new(self.api_key.clone(), self.base_url.clone());
        let client = if let Some(ref ua) = self.user_agent {
            client.with_user_agent(ua.clone())
        } else {
            client
        };
        client.chat(req)
    }
}

// ── DeepSeek (was deepseek.rs) ───────────────────────────────────────────────

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
            let reasoning: Vec<&str> = orig
                .parts
                .iter()
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

// ── Qwen (was qwen.rs) ───────────────────────────────────────────────────────

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

// ── Kimi (was kimi.rs) ───────────────────────────────────────────────────────

const KIMI_DEFAULT_BASE_URL: &str = "https://api.moonshot.cn";

#[derive(Clone)]
pub struct KimiProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
}

impl KimiProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, KIMI_DEFAULT_BASE_URL.to_string())
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

#[async_trait]
impl ChatProvider for KimiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        use crate::providers::protocols::openai::chat_completions::OpenAiChatCompletionsClient;
        let client = OpenAiChatCompletionsClient::new(self.api_key.clone(), self.base_url.clone());
        let client = if let Some(ref ua) = self.user_agent {
            client.with_user_agent(ua.clone())
        } else {
            client
        };
        client.chat(req)
    }
}
