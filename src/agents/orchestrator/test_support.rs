//! Test-only fixtures for constructing an [`OrchestratorCtx`] with stubbed
//! infrastructure, so the inbound interceptors can be unit-tested in isolation.
//!
//! The interceptors under test (ask-reply / callback / crash-recovery) never
//! touch the LLM path, so the `AgentRuntime` here is wired with a no-op
//! `ProviderRegistry` and empty registries — enough to satisfy the type, not to
//! run a turn.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use parking_lot::RwLock;

use super::ctx::{ChannelRegistry, OrchestratorCtx, TurnTracker};
use crate::agents::context_engine::ContextEngine;
use crate::agents::resource_provider::ResourceProvider;
use crate::agents::session::SessionManager;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::{AgentRegistry, AgentRuntime, AskRouter, LoopBreaker, LoopBreakerConfig};
use crate::channels::{Channel, ChannelInboundMessage, ChannelOutboundMessage, OutboundSendResult};
use crate::providers::{
    Capability, ChatModelConfig, ChatProvider, EmbeddingProvider, ImageGenerationProvider,
    ProviderRegistry, ProviderSummary, SearchFallbackEntry, SearchProvider, SttProvider,
    TtsProvider, VideoGenerationProvider,
};
use tokio::sync::mpsc;

/// A `Channel` that records every `ChannelOutboundMessage` it is handed.
pub(crate) struct MockChannel {
    pub sent: Arc<Mutex<Vec<ChannelOutboundMessage>>>,
}

impl MockChannel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Arc::new(Mutex::new(Vec::new())),
        })
    }
    /// The fallback text of every message sent so far.
    pub(crate) fn texts(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.content.text.clone())
            .collect()
    }
}

#[async_trait]
impl Channel for MockChannel {
    fn name(&self) -> &str {
        "mock"
    }
    async fn send_message(
        &self,
        msg: &ChannelOutboundMessage,
    ) -> anyhow::Result<OutboundSendResult> {
        self.sent.lock().unwrap().push(msg.clone());
        Ok(OutboundSendResult::empty())
    }
    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelInboundMessage>> {
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
    }
    async fn health_check(&self) -> bool {
        true
    }
}

/// A `ProviderRegistry` that errors on everything — the interceptors under test
/// never call it.
struct NullRegistry;

#[rustfmt::skip]
impl ProviderRegistry for NullRegistry {
    fn get_chat_provider(&self, _c: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> { anyhow::bail!("test stub") }
    fn get_chat_provider_with_hint(&self, _c: Capability, _h: Option<&str>) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> { anyhow::bail!("test stub") }
    fn get_chat_fallback_chain(&self, _c: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>> { anyhow::bail!("test stub") }
    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)> { anyhow::bail!("test stub") }
    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)> { anyhow::bail!("test stub") }
    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)> { anyhow::bail!("test stub") }
    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)> { anyhow::bail!("test stub") }
    fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)> { anyhow::bail!("test stub") }
    fn get_search_fallback_chain(&self) -> anyhow::Result<Vec<SearchFallbackEntry>> { anyhow::bail!("test stub") }
    fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)> { anyhow::bail!("test stub") }
    fn get_chat_model_config(&self, _m: &str) -> anyhow::Result<&ChatModelConfig> { anyhow::bail!("test stub") }
    fn get_chat_provider_by_model(&self, _m: &str) -> Option<(Arc<dyn ChatProvider>, String)> { None }
    fn get_chat_provider_id_by_model(&self, _m: &str) -> Option<String> { None }
    fn get_chat_media_policy(&self, _m: &str) -> Option<crate::providers::MediaPolicy> { None }
    fn get_chat_routing_models(&self) -> Vec<String> { Vec::new() }
    fn get_all_provider_summaries(&self) -> Vec<ProviderSummary> { Vec::new() }
}

fn test_runtime() -> AgentRuntime {
    let providers: Arc<dyn ProviderRegistry> = Arc::new(NullRegistry);
    let tools = Arc::new(crate::agents::ToolRegistry::new());
    let skills = Arc::new(RwLock::new(crate::agents::SkillManager::new()));
    let agents = Arc::new(AgentRegistry::default());
    let resources = ResourceProvider::new(
        Arc::clone(&skills),
        Arc::clone(&agents),
        Vec::new(),
        std::path::PathBuf::new(),
        std::path::PathBuf::new(),
        String::new(),
        0,
    );
    let context_engine = Arc::new(ContextEngine::new(
        &crate::config::agent::ContextConfig::default(),
        Arc::clone(&providers),
        resources,
        Arc::clone(&tools),
    ));
    let tool_executor = Arc::new(ToolExecutor::new(30));
    let loop_breaker = Arc::new(LoopBreaker::new(LoopBreakerConfig::default()));
    AgentRuntime::new(
        providers,
        tools,
        skills,
        agents,
        context_engine,
        tool_executor,
        loop_breaker,
    )
}

/// Build an `OrchestratorCtx` over an in-memory SessionManager, a fresh
/// AskRouter, and the given channels (keyed by `(channel_type, account_id)`).
pub(crate) fn test_ctx(channels: Vec<((String, String), Arc<dyn Channel>)>) -> OrchestratorCtx {
    let registry = ChannelRegistry::new();
    for (k, ch) in channels {
        registry.insert(k, ch);
    }
    OrchestratorCtx {
        channels: registry,
        sessions: Arc::new(SessionManager::default()),
        ask: Arc::new(AskRouter::new()),
        known_users: Arc::new(crate::agents::KnownUsersRegistry::in_memory()),
        user_registry: Arc::new(crate::agents::UserRegistry::in_memory()),
        runtime: test_runtime(),
        delegator: None,
        scheduler: None,
        turn_tracker: Arc::new(TurnTracker::new()),
    }
}

/// A minimal inbound `ChannelInboundMessage` with the given sender and content.
pub(crate) fn inbound_msg(sender: &str, content: &str) -> ChannelInboundMessage {
    ChannelInboundMessage {
        id: "test-msg".to_string(),
        sender: crate::channels::MessageSender::new(sender.to_string()),
        receiver: crate::channels::MessageReceiver::new(sender.to_string()),
        content: crate::channels::ChannelMessageContent::text(content.to_string()),
        timestamp: 0,
        interruption_scope_id: None,
        silenced_override: None,
        progress_text: None,
    }
}
