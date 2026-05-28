//! `AgentRuntime` — singleton bundle of runtime infrastructure shared
//! across all sessions.
//!
//! RFC v2 §三.A: separates *what the agent is* (`Agent { config }`) from
//! *what it has access to* (`AgentRuntime { providers, tools, skills,
//! agents, context_engine, tool_executor, loop_breaker, defaults }`).
//! Executors live as `Arc<X>` singletons so per-turn `Agent::run` reads
//! one shared instance instead of rebuilding the executor stack each
//! message.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::agents::context_engine::ContextEngine;
use crate::agents::loop_breaker::LoopBreaker;
use crate::agents::prompt::SystemPromptConfig;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::workspace::skills::SkillManager;
use crate::agents::AgentRegistry;
use crate::config::agent::PermissionMode;
use crate::providers::ProviderRegistry;

use super::session::PersistHook;
use super::tool_registry::ToolRegistry;

/// Default runtime values applied to every turn unless overridden by
/// `SessionOverride`. Matches the target architecture's
/// `RuntimeDefaults { permission_mode, prompt }` shape exactly.
#[derive(Clone)]
pub struct RuntimeDefaults {
    /// Default permission mode (overridden per turn by SessionOverride).
    pub permission_mode: PermissionMode,
    /// Base prompt config — workspace/knowledge dirs (as strings, used
    /// by SystemPromptBuilder + file-tool path resolution), identity
    /// header, native_tools, …
    pub prompt: SystemPromptConfig,
}

impl Default for RuntimeDefaults {
    fn default() -> Self {
        Self {
            permission_mode: PermissionMode::Default,
            prompt: SystemPromptConfig::default(),
        }
    }
}

/// Per-process runtime resources shared by every `Agent::run` invocation.
///
/// All fields are `Arc<...>` or `Clone` so passing `AgentRuntime` by value
/// (or by `&self`) is cheap and Send-safe.
#[derive(Clone)]
pub struct AgentRuntime {
    /// LLM provider registry — used to resolve `model_id` to a ChatProvider.
    pub providers: Arc<dyn ProviderRegistry>,
    /// All registered tools (built-in + MCP wrappers + skill tools).
    /// `Agent.run` filters this through `AgentConfig.tools` per turn.
    pub tools: Arc<ToolRegistry>,
    /// Skill metadata for system-prompt injection and the `skill_view` tool.
    pub skills: Arc<RwLock<SkillManager>>,
    /// Sub-agents available for `agent_delegate`. Shared `Arc` so the
    /// AgentRegistry held by SessionManager and AgentRuntime is the
    /// same live table.
    pub agents: Arc<AgentRegistry>,
    /// Compaction policy + summarizer. Shared singleton.
    pub context_engine: Arc<ContextEngine>,
    /// Tool executor (timeout + dispatch). Shared singleton.
    pub tool_executor: Arc<ToolExecutor>,
    /// Loop-breaker policy. Hands out per-turn `LoopBreakerCounter`s
    /// via `new_counter()`.
    pub loop_breaker: Arc<LoopBreaker>,
    /// Defaults (permission_mode, prompt config, cached system prompt).
    pub defaults: RuntimeDefaults,
    /// Optional session-persistence hook — None for tests; Some for
    /// production daemons. Kept here so `SessionContext::process_turn`
    /// can wire `Session.persist` at turn start.
    pub persist: Option<Arc<dyn PersistHook>>,
}

impl AgentRuntime {
    /// Build with all executors and a default `RuntimeDefaults`. Use
    /// the `with_*` builders to install non-default values.
    pub fn new(
        providers: Arc<dyn ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        skills: Arc<RwLock<SkillManager>>,
        agents: Arc<AgentRegistry>,
        context_engine: Arc<ContextEngine>,
        tool_executor: Arc<ToolExecutor>,
        loop_breaker: Arc<LoopBreaker>,
    ) -> Self {
        Self {
            providers,
            tools,
            skills,
            agents,
            context_engine,
            tool_executor,
            loop_breaker,
            defaults: RuntimeDefaults::default(),
            persist: None,
        }
    }

    pub fn with_defaults(mut self, defaults: RuntimeDefaults) -> Self {
        self.defaults = defaults;
        self
    }

    pub fn with_persist(mut self, persist: Arc<dyn PersistHook>) -> Self {
        self.persist = Some(persist);
        self
    }

    /// Build the system prompt for a turn. Applies the supplied
    /// `prompt_config` (with any per-session overrides already merged
    /// in) against the live SkillManager. Per the RFC v2 target,
    /// AgentRuntime no longer caches the assembled prompt — it's
    /// constructed fresh each turn so session-override changes are
    /// honored immediately.
    pub fn build_system_prompt(&self, prompt_config: &SystemPromptConfig) -> String {
        let skills = self.skills.read();
        crate::agents::SystemPromptBuilder::new(prompt_config.clone()).build(&skills)
    }
}
