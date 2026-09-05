//! Shared test fixtures for the `agent/` submodules (RFC §2: tests.rs
//! hosts only cross-file fixtures; per-symbol tests live beside their
//! implementation). Extracted verbatim from the former `agent.rs` tests
//! mod (batch 5).

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::agents::AgentRuntime;
use crate::agents::compaction_engine::CompactionEngine;
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
pub(crate) fn bailing_runtime() -> AgentRuntime {
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
    let context_engine = Arc::new(CompactionEngine::new(
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

// ── Skeleton-level tests (moved from `mod.rs` batch 6; RFC §2) ─────────────
// run/run_recovery end-to-end via bailing_runtime; symbol-level tests live
// beside their implementation; shared fixtures above.

use super::tool_filter::filter_turn_scoped_tools;
use super::Agent;
use crate::agents::session::Session;
use crate::agents::turn::TurnContext;
use crate::config::agent::{PermissionMode, RunMode};
use crate::providers::{Tool, ToolResult};

/// Regression guard for the v4 recovery refactor: `Agent::run` must
/// pre-check `run_recovery`, and `run_recovery`'s Cases B/C must fall
/// through to `run_inner` — NOT back into `run` (that would re-enter
/// `run_recovery` and recurse until the stack overflows).
///
/// Case C (trailing user message, no LLM response) exercises the full
/// `run → run_recovery → run_inner` chain; the stub registry makes the
/// first provider lookup fail with "test stub", which is exactly what
/// we assert. A regression to `run_recovery` calling `run()` would
/// abort the test with a stack overflow instead of returning Err.
#[tokio::test]
async fn run_prechecks_recovery_without_recursing() {
    let mut session = Session::new("sess-1".into());
    session.add_user("pending question".into());
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let mut session = session.lock_owned().await;
    let agent = Agent::new(empty_config());
    let runtime = bailing_runtime();
    let turn_ctx = TurnContext {
        system_prompt: "",
        model_id: None,
        thinking: None,
        permission_mode: PermissionMode::Default,
        run_mode: RunMode::Interactive,
    };
    let err = match agent.run(&mut session, turn_ctx, &runtime).await {
        Ok(_) => panic!("expected recovery to run the LLM and hit the stub registry"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("test stub"),
        "expected stub registry error, got: {err}"
    );
}

struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        self.0
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools.iter().map(|tool| tool.name().to_string()).collect()
}

#[test]
fn turn_tool_allowlist_narrows_to_intersection() {
    // Some(["shell"]) → only "shell" survives (intersection with
    // whatever was already in the list).
    let mut session = Session::new("s".into());
    session.turn_tool_allowlist = Some(vec!["shell".into()]);
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(NamedTool("shell")),
        Arc::new(NamedTool("file_read")),
        Arc::new(NamedTool("calculator")),
    ];
    filter_turn_scoped_tools(&mut tools, &session);
    assert_eq!(tool_names(&tools), vec!["shell"]);
}

#[test]
fn turn_tool_allowlist_empty_forbids_all() {
    // Some([]) → explicitly disable all tools.
    let mut session = Session::new("s".into());
    session.turn_tool_allowlist = Some(vec![]);
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(NamedTool("shell")),
        Arc::new(NamedTool("file_read")),
    ];
    filter_turn_scoped_tools(&mut tools, &session);
    assert!(tools.is_empty());
}

#[test]
fn turn_tool_allowlist_none_preserves_existing() {
    // None → no extra filtering beyond the normal scoped filter.
    let mut session = Session::new("s".into());
    session.turn_tool_allowlist = None;
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(NamedTool("shell")),
        Arc::new(NamedTool("calculator")),
    ];
    filter_turn_scoped_tools(&mut tools, &session);
    assert_eq!(tool_names(&tools), vec!["shell", "calculator"]);
}

#[test]
fn agent_holds_config() {
    let cfg = empty_config();
    let agent = Agent::new(cfg);
    assert_eq!(agent.config.name, "test");
}

#[test]
fn session_persist_field_default_none() {
    let session = Session::new("s".into());
    assert!(session.persist.is_none());
    assert!(session.channels.is_none());
    assert!(session.channel_account.is_none());
    assert!(!session.turn_headless);
    assert!(session.resolve_channel().is_none());
}
