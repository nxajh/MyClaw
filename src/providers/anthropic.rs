//! Anthropic provider — implements ChatProvider only.
//!
//! Chat is delegated to `AnthropicMessagesClient` from the protocols layer.

use async_trait::async_trait;

use crate::providers::{BoxStream, ChatProvider, ChatRequest, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self { base_url: DEFAULT_BASE_URL.to_string(), api_key, user_agent: None }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, user_agent: None }
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