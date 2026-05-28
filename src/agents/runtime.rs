//! `AgentRuntime` — singleton bundle of runtime infrastructure shared
//! across all sessions.
//!
//! RFC v2 §三.A: separates *what the agent is* (`Agent { config }`) from
//! *what it has access to* (`AgentRuntime { tools, mcp, skills, ... }`).
//! Today AgentLoop conflates both — every per-session AgentLoop holds its
//! own copy of tools/skills/mcp Arc<...> chains. Switching to one shared
//! AgentRuntime drops the per-session Arc churn and gives a single
//! reload-on-disk-change touchpoint.
//!
//! This module currently defines the type and a minimal builder. The
//! orchestrator and Agent.run still reach into the old AgentLoop fields;
//! C18 atomic rewrite will switch them onto AgentRuntime.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::agents::workspace::skills::SkillManager;
use crate::agents::AgentRegistry;
use crate::providers::ProviderRegistry;

use super::loop_breaker::LoopBreakerConfig;
use super::session::PersistHook;
use super::tool_registry::ToolRegistry;

/// Per-process runtime resources shared by every `Agent.run` invocation.
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
    /// Sub-agents available for `agent_delegate`.
    pub agents: AgentRegistry,
    /// Default loop-breaker policy. Per-turn instances are spawned in
    /// `Agent.run` and reset between turns.
    pub loop_breaker_defaults: LoopBreakerConfig,
    /// Default tool timeout (seconds) applied to non-special tools.
    pub tool_timeout_secs: u64,
    /// Optional persist hook — None for tests; Some for production.
    pub persist: Option<Arc<dyn PersistHook>>,
    /// Workspace root for resolving `${SKILL_DIR}` and file-tool relative paths.
    pub workspace_dir: PathBuf,
    /// Knowledge root (memory/ directory).
    pub knowledge_dir: PathBuf,
    /// Context-engine configuration (compact_threshold, retain_work_units).
    /// `Agent::run` reads this to build its per-turn ContextEngine — without
    /// it the engine falls back to its hard-coded defaults and the user's
    /// `[agent.context]` config is silently ignored.
    pub context: crate::config::agent::ContextConfig,
    /// Base prompt config (permission_mode default, run_mode default,
    /// workspace_dir, identity_header, native_tools, …). Per-turn
    /// SessionOverride is applied on top in process_turn.
    pub prompt_config: crate::agents::prompt::SystemPromptConfig,
    /// Pre-built system prompt for the "main" agent. Empty string means
    /// "rebuild via SystemPromptBuilder from skills + prompt_config".
    /// Per-turn callers prefer this cached value to avoid rebuilding on
    /// every message.
    pub system_prompt: String,
}

impl AgentRuntime {
    /// Minimal constructor — fills in defaults for everything optional.
    /// Production wiring uses `AgentRuntime::builder()`.
    pub fn new(
        providers: Arc<dyn ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        skills: Arc<RwLock<SkillManager>>,
        agents: AgentRegistry,
    ) -> Self {
        Self {
            providers,
            tools,
            skills,
            agents,
            loop_breaker_defaults: LoopBreakerConfig::default(),
            tool_timeout_secs: 180,
            persist: None,
            workspace_dir: PathBuf::new(),
            knowledge_dir: PathBuf::new(),
            context: crate::config::agent::ContextConfig::default(),
            prompt_config: crate::agents::prompt::SystemPromptConfig::default(),
            system_prompt: String::new(),
        }
    }

    pub fn with_context(mut self, ctx: crate::config::agent::ContextConfig) -> Self {
        self.context = ctx;
        self
    }

    pub fn with_prompt_config(mut self, cfg: crate::agents::prompt::SystemPromptConfig) -> Self {
        self.prompt_config = cfg;
        self
    }

    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    pub fn with_persist(mut self, persist: Arc<dyn PersistHook>) -> Self {
        self.persist = Some(persist);
        self
    }

    pub fn with_dirs(mut self, workspace: PathBuf, knowledge: PathBuf) -> Self {
        self.workspace_dir = workspace;
        self.knowledge_dir = knowledge;
        self
    }

    pub fn with_tool_timeout(mut self, secs: u64) -> Self {
        self.tool_timeout_secs = secs;
        self
    }

    pub fn with_loop_breaker(mut self, cfg: LoopBreakerConfig) -> Self {
        self.loop_breaker_defaults = cfg;
        self
    }
}
