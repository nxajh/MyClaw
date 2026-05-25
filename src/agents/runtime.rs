//! `AgentRuntime` — singleton bundle of runtime infrastructure shared
//! across all sessions.
//!
//! RFC v2 §三.A: separates *what the agent is* (`Agent { config }`) from
//! *what it has access to* (`AgentRuntime { tools, mcp, skills, ... }`).

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::agents::workspace::skills::SkillManager;
use crate::agents::AgentRegistry;
use crate::providers::ProviderRegistry;

use super::loop_breaker::{LoopBreaker, LoopBreakerConfig};
use super::session::PersistHook;
use super::tool_registry::ToolRegistry;
use super::context_engine::ContextEngine;
use super::tool_executor::ToolExecutor;
use super::resource_provider::ResourceProvider;
use super::attachment::AttachmentManager;
use super::prompt::SystemPromptBuilder;
use super::agent_impl::{AgentConfig, AgentLoop, AgentSession};
use super::session::Session;

/// Channel map type alias (avoids clippy type_complexity).
pub type ChannelMap = Arc<RwLock<std::collections::HashMap<(String, String), Arc<dyn crate::channels::Channel>>>>;

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
    pub channels: ChannelMap,
    /// MCP server instructions (server_name → instructions text).
    pub mcp_instructions: Vec<(String, String)>,
    /// Default agent config (prompt, context, model, tool limits).
    pub agent_config: AgentConfig,
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
            mcp_instructions: Vec::new(),
            agent_config: AgentConfig::default(),
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

    pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_sub_agent_configs(mut self, agents: AgentRegistry) -> Self {
        self.agents = agents;
        self
    }

    pub fn with_tool_timeout(mut self, secs: u64) -> Self {
        self.tool_timeout_secs = secs;
        self
    }

    pub fn with_agent_config(mut self, config: AgentConfig) -> Self {
        self.agent_config = config;
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

    pub fn with_mcp_instructions(mut self, instructions: Vec<(String, String)>) -> Self {
        self.mcp_instructions = instructions;
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

    pub fn mcp_instructions(&self) -> &[(String, String)] {
        &self.mcp_instructions
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.workspace_dir.join("skills")
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.workspace_dir.join("agents")
    }

    pub fn knowledge_dir(&self) -> PathBuf {
        self.knowledge_dir.clone()
    }

    pub fn compact_threshold(&self) -> f64 {
        self.agent_config.context.compact_threshold
    }

    pub fn workspace_dir(&self) -> PathBuf {
        self.workspace_dir.clone()
    }

    // ── Session creation ────────────────────────────────────────────────

    /// Build an `AgentSession` from this runtime + per-session config.
    ///
    /// Replaces the old `Agent(factory).loop_for_with_persist(session, hook)`.
    /// The `agent_config` captures per-agent settings (system prompt template,
    /// model override, tool limits, context policy).
    pub fn create_session(
        &self,
        session: Session,
        persist_hook: Option<Arc<dyn PersistHook>>,
    ) -> AgentSession {
        let ov = &session.session_override;
        let config = self.agent_config.with_override(ov);

        let prompt = {
            let skills = self.skills.read();
            let builder = SystemPromptBuilder::new(config.prompt_config.clone());
            builder.build(&skills)
        };

        let max_tool_calls = config.max_tool_calls;

        let resources = ResourceProvider::new(
            Arc::clone(&self.skills),
            self.agents.clone(),
            self.mcp_instructions.clone(),
            self.skills_dir(),
            self.agents_dir(),
            config.prompt_config.knowledge_dir.clone(),
            config.timezone_offset,
        );
        AgentSession { loop_: AgentLoop {
            registry: Arc::clone(&self.providers),
            system_prompt: prompt,
            attachments: AttachmentManager::new(),
            resources: Arc::clone(&resources),
            pending_image_urls: None,
            pending_image_base64: None,
            change_rx: None,
            context: ContextEngine::new(
                &config.context,
                Arc::clone(&self.providers),
                Arc::clone(&resources),
                Arc::clone(&self.tools),
            ),
            tool_executor: ToolExecutor::new(Arc::clone(&self.tools), config.tool_timeout_secs),
            config,
            session,
            loop_breaker: LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls,
                exact_repeat_threshold: self.loop_breaker_defaults.exact_repeat_threshold,
                ..LoopBreakerConfig::default()
            }),
            persist_hook,
            pending_retry_message: None,
        }}
    }
}
