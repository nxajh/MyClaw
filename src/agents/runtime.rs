//! `AgentRuntime` — singleton bundle of runtime infrastructure shared
//! across all sessions.
//!
//! RFC v2 §三.A: separates *what the agent is* (`Agent { config }`) from
//! *what it has access to* (`AgentRuntime { providers, tools, skills,
//! agents, context_engine, tool_executor, loop_breaker, defaults }`).
//! Executors live as `Arc<X>` singletons so per-turn `Agent::run` reads
//! one shared instance instead of rebuilding the executor stack each
//! message.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::agents::AgentRegistry;
use crate::agents::context_engine::ContextEngine;
use crate::agents::loop_breaker::LoopBreaker;
use crate::agents::mcp_manager::McpManager;
use crate::agents::prompt::SystemPromptConfig;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::user_registry::UserRegistry;
use crate::agents::workspace::skills::SkillManager;
use crate::config::agent::PermissionMode;
use crate::providers::ProviderRegistry;
use crate::tools::SearchProviderCooldown;

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
    /// Enable automatic TTS for replies.
    pub auto_tts: bool,
}

impl Default for RuntimeDefaults {
    fn default() -> Self {
        Self {
            permission_mode: PermissionMode::Default,
            prompt: SystemPromptConfig::default(),
            auto_tts: false,
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
    /// Defaults — exactly `{ permission_mode, prompt }` per the target
    /// shape; see RFC v2 §三.A.
    pub defaults: RuntimeDefaults,
    /// MCP server manager (Option because MCP servers are opt-in).
    /// Read by the `/mcp` slash command to report connection state.
    pub mcp_manager: Option<Arc<McpManager>>,
    /// Search-provider rate-limit tracker shared with `WebSearchTool`
    /// (the tool writes timestamps on rate-limit; `/status` reads them
    /// to render ⏱️ markers next to cooled-down providers).
    pub search_cooldown: Option<Arc<SearchProviderCooldown>>,
    /// Per-session task boards (P1-B1). Resolved from the current session id
    /// at compaction time so the injected plan matches the session's own
    /// `{sessions_root}/{uuid}/tasks.json` board.
    pub task_boards: Option<Arc<crate::tools::TaskBoards>>,
    /// Root directory of session storage (`{workspace}/sessions`).
    /// Used by the exec-marker mechanism so recovery can detect tools
    /// that were executing when the daemon was killed (e.g. `myclaw update`).
    /// `None` in tests / CLI mode — marker logic is skipped.
    pub sessions_dir: Option<PathBuf>,
    /// P4 第二波：User registry for `<ref>` output rendering (streaming + Done +
    /// fallback send). `None` in tests / CLI mode — rendering degrades to the
    /// `@u/uid` fallback (render_refs accepts `Option`-less registry; callers
    /// skip rendering entirely when this is `None`).
    pub user_registry: Option<Arc<UserRegistry>>,
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
            mcp_manager: None,
            search_cooldown: None,
            task_boards: None,
            sessions_dir: None,
            user_registry: None,
        }
    }

    pub fn with_defaults(mut self, defaults: RuntimeDefaults) -> Self {
        self.defaults = defaults;
        self
    }

    pub fn with_mcp_manager(mut self, mcp: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(mcp);
        self
    }

    pub fn with_search_cooldown(mut self, cooldown: Arc<SearchProviderCooldown>) -> Self {
        self.search_cooldown = Some(cooldown);
        self
    }

    pub fn with_task_boards(mut self, boards: Arc<crate::tools::TaskBoards>) -> Self {
        self.task_boards = Some(boards);
        self
    }

    pub fn with_sessions_dir(mut self, dir: PathBuf) -> Self {
        self.sessions_dir = Some(dir);
        self
    }

    /// P4 第二波：install the shared UserRegistry so agent replies can render
    /// `<ref id="…"/>` → `@昵称(u/uid)` on streaming/Done/fallback paths.
    pub fn with_user_registry(mut self, registry: Arc<UserRegistry>) -> Self {
        self.user_registry = Some(registry);
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
