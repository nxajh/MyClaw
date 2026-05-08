//! MiniMax provider — delegates to Anthropic-compatible API.
//!
//! MiniMax officially recommends using their Anthropic-compatible endpoint
//! (`https://api.minimaxi.com/anthropic`).  This module is a thin wrapper
//! around [`AnthropicProvider`], which already implements the full Anthropic
//! SSE protocol (tools, thinking, streaming, tool_result).

use async_trait::async_trait;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/anthropic";

#[derive(Clone)]
pub struct MiniMaxProvider {
    inner: AnthropicProvider,
}

impl MiniMaxProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { inner: AnthropicProvider::with_base_url(api_key, base_url) }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.inner = self.inner.with_user_agent(user_agent);
        self
    }
}

#[async_trait]
impl ChatProvider for MiniMaxProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        self.inner.chat(req)
    }
}
