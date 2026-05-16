//! Kimi (Moonshot) provider — Kimi-specific protocol.
//!
//! Key differences from generic OpenAI:
//! - `reasoning_content` is a top-level message field (not a content part)
//! - K2.5/K2.6 support `thinking` parameter for enabling/disabling reasoning
//! - `content` must not be empty; assistant messages with only tool_calls use null
//!
//! The SSE parsing is OpenAI-compatible, so this provider delegates
//! to `OpenAiChatCompletionsClient` from the protocols layer.
//! Kimi-specific body rendering is preserved for the thinking/reasoning fields.

use async_trait::async_trait;

use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, StreamEvent,
};

const DEFAULT_BASE_URL: &str = "https://api.moonshot.cn";

#[derive(Clone)]
pub struct KimiProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
}

impl KimiProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
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
