//! `AgentRuntime` — singleton bundle of runtime infrastructure shared
//! across all sessions.
//!
//! RFC v2 §三.A: separates *what the agent is* (`Agent { config }`) from
//! *what it has access to* (`AgentRuntime { tools, mcp, skills, ... }`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::agents::workspace::skills::SkillManager;
use crate::agents::AgentRegistry;
use crate::providers::ProviderRegistry;

use super::loop_breaker::LoopBreakerConfig;
use super::session::PersistHook;
use super::tool_registry::ToolRegistry;

/// Per-process runtime resources shared by every `Agent.run` invocation.
#[derive(Clone)]
pub struct AgentRuntime {
    /// LLM provider registry.
    pub providers: Arc<dyn ProviderRegistry>,
    /// All registered tools (built-in + MCP wrappers + skill tools).
    pub tools: Arc<ToolRegistry>,
    /// Skill metadata for system-prompt injection and the `skill_view` tool.
    pub skills: Arc<RwLock<SkillManager>>,
    /// Sub-agents available for `agent_delegate`.
    pub agents: AgentRegistry,
    /// Default loop-breaker policy.
    pub loop_breaker_defaults: LoopBreakerConfig,
    /// Default tool timeout (seconds).
    pub tool_timeout_secs: u64,
    /// Optional persist hook.
    pub persist: Option<Arc<dyn PersistHook>>,
    /// Workspace root.
    pub workspace_dir: PathBuf,
    /// Knowledge root (memory/ directory).
    pub knowledge_dir: PathBuf,
    /// E30: AskRouter for F35 AskUserTool integration.
    pub ask_router: Option<Arc<crate::agents::ask_router::AskRouter>>,
    /// F35: Channels map for ask_user delivery.
    pub channels: Arc<RwLock<std::collections::HashMap<(String, String), Arc<dyn crate::channels::Channel>>>>,
}

impl AgentRuntime {
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
            ask_router: None,
            channels: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
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

    pub fn with_ask_router(mut self, router: Arc<crate::agents::ask_router::AskRouter>) -> Self {
        self.ask_router = Some(router);
        self
    }

    // ── Accessors ───────────────────────────────────────────────────────

    pub fn registry(&self) -> &Arc<dyn ProviderRegistry> {
        &self.providers
    }

    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    pub fn skills(&self) -> &Arc<RwLock<SkillManager>> {
        &self.skills
    }

    pub fn sub_agent_configs(&self) -> AgentRegistry {
        self.agents.clone()
    }

    pub fn mcp_instructions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.workspace_dir.join("skills")
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.workspace_dir.join("agents")
    }
}
