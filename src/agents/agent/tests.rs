//! Shared test fixtures for the `agent/` submodules (RFC §2: tests.rs
//! hosts only cross-file fixtures; per-symbol tests live beside their
//! implementation). Extracted verbatim from the former `agent.rs` tests
//! mod (batch 5).

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::agents::AgentRuntime;
use crate::agents::context_engine::ContextEngine;
use crate::agents::resource_provider::ResourceProvider;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::{AgentRegistry, LoopBreaker, LoopBreakerConfig};
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::Capability;
use crate::providers::{
    ChatModelConfig, ChatProvider, EmbeddingProvider, ImageGenerationProvider, ProviderRegistry,
    ProviderSummary, SearchFallbackEntry, SearchProvider, SttProvider, TtsProvider,
    VideoGenerationProvider,
};

pub(super) fn empty_config() -> SubAgentConfig {
    SubAgentConfig {
        name: "test".into(),
        system_prompt: String::new(),
        tools: Default::default(),
        skills: Default::default(),
        mcp: Default::default(),
        max_tool_calls: None,
        description: None,
        model: None,
        isolation: Default::default(),
        timeout: None,
    }
}

/// A `ProviderRegistry` that errors on everything — enough to satisfy
/// `AgentRuntime`'s type, never enough to run a real turn.
struct BailingRegistry;

#[rustfmt::skip]
impl ProviderRegistry for BailingRegistry {
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

/// An `AgentRuntime` whose provider registry always bails — the turn
/// under test must fail with "test stub" before touching any real LLM.
pub(super) fn bailing_runtime() -> AgentRuntime {
    let providers: Arc<dyn ProviderRegistry> = Arc::new(BailingRegistry);
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
