//! Agent — shared factory for AgentLoop instances.
//!
//! Agent holds shared resources (registry, skills, config) and creates
//! per-session AgentLoop handles.
//!
//! DDD: Agent depends on `dyn ServiceRegistry` (Domain trait), not on
//! `Registry` (Infrastructure concrete type). This keeps the Application
//! layer decoupled from Infrastructure.

use std::sync::Arc;
use std::path::PathBuf;

use parking_lot::RwLock;
use tokio::sync::watch;

use crate::providers::ServiceRegistry;
use crate::config::agent::ContextConfig;
use crate::agents::session_manager::SessionOverride;

use super::skills::SkillManager;
use super::tool_registry::ToolRegistry;

/// Callback for ask_user tool: (session_key, question) → user_answer.
///
/// The handler sends the question through the channel and waits for the
/// user's next message, which is delivered via a oneshot channel managed
/// by the Orchestrator.
pub type AskUserHandler = Arc<
    dyn Fn(String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>
        + Send
        + Sync,
>;

/// Callback for async delegation: (agent_name, task) → task_id.
///
/// The handler spawns the sub-agent in a background tokio task and returns
/// the task_id immediately. When the sub-agent completes, the Orchestrator
/// receives a DelegationEvent and wakes the main agent.
pub type DelegateHandler = Arc<
    dyn Fn(String, String) -> anyhow::Result<String> + Send + Sync,
>;

use super::loop_breaker::{LoopBreaker, LoopBreakerConfig};
use super::session_manager::{Session, PersistHook};
use crate::agents::prompt::{SystemPromptBuilder, SystemPromptConfig};
use crate::agents::attachment::AttachmentManager;
use crate::config::sub_agent::SubAgentConfig;
use super::tool_executor::DefaultToolExecutor;
use super::compaction_executor::CompactionExecutor;

pub(crate) mod types;
mod run;
mod tools;
mod compaction;
mod images;

pub(crate) use types::estimate_message_tokens;
use super::compaction_policy::CompactionPolicy;
use super::resource_provider::ResourceProvider;
use super::request_builder::RequestBuilder;

/// AgentConfig controls loop breaker thresholds and tool call limits.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard cap on tool calls per turn. 0 = unlimited.
    pub max_tool_calls: usize,
    /// Maximum history messages to keep in memory. 0 = unlimited.
    pub max_history: usize,
    /// System prompt builder config.
    pub prompt_config: SystemPromptConfig,
    /// Context window management settings.
    pub context: ContextConfig,
    /// Stream chunk timeout in seconds — max time to wait for next chunk.
    pub stream_chunk_timeout_secs: u64,
    /// Max output bytes before forcing stream stop (derived from max_output_tokens).
    pub max_output_bytes: usize,
    /// Loop breaker exact-repeat threshold: N identical consecutive calls → break.
    pub loop_breaker_threshold: usize,
    /// Per-tool execution timeout in seconds (0 = no timeout).
    /// Does not apply to ask_user or agent_delegate (those have their own timeouts).
    pub tool_timeout_secs: u64,
    /// Model override for this session (session override > Agent-level default).
    pub model_override: Option<String>,
    /// Thinking/reasoning config override for this session.
    pub thinking_override: Option<crate::providers::ThinkingConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            max_history: 200,
            prompt_config: SystemPromptConfig::default(),
            context: ContextConfig::default(),
            stream_chunk_timeout_secs: 90,
            max_output_bytes: 100 * 1024,
            loop_breaker_threshold: 3,
            tool_timeout_secs: 180,
            model_override: None,
            thinking_override: None,
        }
    }
}

impl AgentConfig {
    pub fn with_override(&self, ov: &super::session_manager::SessionOverride) -> Self {
        let mut cfg = self.clone();
        if let Some(ref autonomy) = ov.autonomy {
            cfg.prompt_config.autonomy = *autonomy;
        }
        if let Some(t) = ov.compact_threshold { cfg.context.compact_threshold = t; }
        if let Some(r) = ov.retain_work_units { cfg.context.retain_work_units = r; }
        if let Some(mtc) = ov.max_tool_calls   { cfg.max_tool_calls = mtc; }
        cfg.model_override = ov.model.clone();
        cfg.thinking_override = ov.to_thinking_config();
        cfg
    }

}

/// Agent is the shared factory — call `.loop_for(session)` to get an AgentLoop.
#[derive(Clone)]
pub struct Agent {
    registry: Arc<dyn ServiceRegistry>,
    tools: Arc<ToolRegistry>,
    skills: Arc<RwLock<SkillManager>>,
    config: AgentConfig,
    system_prompt: String,
    model_override: Option<String>,
    mcp_instructions: Vec<(String, String)>,
    sub_agent_configs: Arc<RwLock<Vec<SubAgentConfig>>>,
    skills_dir: PathBuf,
    agents_dir: PathBuf,
}

impl Agent {
    pub fn new(
        registry: Arc<dyn ServiceRegistry>,
        tools: Arc<ToolRegistry>,
        skills: Arc<RwLock<SkillManager>>,
        config: AgentConfig,
    ) -> Self {
        Self {
            registry,
            tools,
            skills,
            config,
            system_prompt: String::new(),
            model_override: None,
            mcp_instructions: Vec::new(),
            sub_agent_configs: Arc::new(RwLock::new(Vec::new())),
            skills_dir: PathBuf::new(),
            agents_dir: PathBuf::new(),
        }
    }

    pub fn registry(&self) -> &Arc<dyn ServiceRegistry> { &self.registry }
    pub fn tools(&self) -> &Arc<super::tool_registry::ToolRegistry> { &self.tools }
    pub fn skills(&self) -> &Arc<RwLock<SkillManager>> { &self.skills }

    pub fn sub_agent_configs(&self) -> &Arc<RwLock<Vec<SubAgentConfig>>> { &self.sub_agent_configs }
    pub fn workspace_dir(&self) -> &str { &self.config.prompt_config.workspace_dir }
    pub fn compact_threshold(&self) -> f64 { self.config.context.compact_threshold }

    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    pub fn with_model(mut self, model: String) -> Self {
        self.model_override = Some(model);
        self
    }

    pub fn with_mcp_instructions(mut self, instructions: Vec<(String, String)>) -> Self {
        self.mcp_instructions = instructions;
        self
    }

    pub fn with_sub_agent_configs(mut self, configs: Arc<RwLock<Vec<SubAgentConfig>>>) -> Self {
        self.sub_agent_configs = configs;
        self
    }

    pub fn with_workspace_dirs(mut self, skills_dir: PathBuf, agents_dir: PathBuf) -> Self {
        self.skills_dir = skills_dir;
        self.agents_dir = agents_dir;
        self
    }

    pub fn loop_for(&self, session: Session) -> AgentLoop {
        self.loop_for_with_persist(session, None)
    }

    pub fn loop_for_with_persist(
        &self,
        session: Session,
        persist_hook: Option<Arc<dyn PersistHook>>,
    ) -> AgentLoop {
        let ov = &session.session_override;
        let config = self.config.with_override(ov);

        let prompt = if !self.system_prompt.is_empty() {
            self.system_prompt.clone()
        } else {
            let skills = self.skills.read();
            let builder = SystemPromptBuilder::new(config.prompt_config.clone());
            builder.build(&skills)
        };

        // Apply Agent-level model_override as fallback if session didn't specify one.
        let mut config = config;
        if config.model_override.is_none() {
            config.model_override = self.model_override.clone();
        }
        let max_tool_calls = config.max_tool_calls;
        let policy = CompactionPolicy::from_context_config(&config.context);

        let resources = ResourceProvider::new(
            Arc::clone(&self.skills),
            Arc::clone(&self.sub_agent_configs),
            self.mcp_instructions.clone(),
            self.skills_dir.clone(),
            self.agents_dir.clone(),
            config.prompt_config.knowledge_dir.clone(),
            config.prompt_config.timezone_offset,
        );
        let request_builder = RequestBuilder::new(prompt, Arc::clone(&resources));

        AgentLoop {
            registry: Arc::clone(&self.registry),
            compactor: CompactionExecutor::new(
                Arc::clone(&self.registry),
                Arc::clone(&resources),
                Arc::clone(&self.tools),
                config.stream_chunk_timeout_secs,
            ),
            tool_executor: DefaultToolExecutor::new(Arc::clone(&self.tools), config.tool_timeout_secs),
            config,
            session,
            request_builder,
            loop_breaker: LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls,
                exact_repeat_threshold: self.config.loop_breaker_threshold,
                ..LoopBreakerConfig::default()
            }),
            policy,
            persist_hook,
            pending_retry_message: None,
        }
    }
}

/// Per-session agent loop handle. Execute `run(user_message)` to process a message.
pub struct AgentLoop {
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    pub(crate) config: AgentConfig,
    pub(crate) session: Session,
    // ── Message building + attachments + images + hot-reload ──
    pub(crate) request_builder: RequestBuilder,
    // ── Token tracking + compaction strategy ──
    pub(crate) policy: CompactionPolicy,
    // ── Tool execution ──
    pub(crate) tool_executor: DefaultToolExecutor,
    // ── Compaction summarizer ──
    pub(crate) compactor: CompactionExecutor,
    // ── Infrastructure ──
    pub(crate) loop_breaker: LoopBreaker,
    pub(crate) persist_hook: Option<Arc<dyn PersistHook>>,
    pub(crate) pending_retry_message: Option<String>,
}

impl AgentLoop {
    pub fn with_ask_user_handler(mut self, handler: AskUserHandler) -> Self {
        self.tool_executor.ask_user_handler = Some(handler);
        self
    }

    pub fn session(&self) -> &super::session_manager::Session {
        &self.session
    }

    pub fn token_total(&self) -> u64 {
        self.policy.token_total()
    }

    pub fn last_usage(&self) -> (u64, u64, u64) {
        self.policy.last_usage()
    }

    pub fn compact_threshold(&self) -> f64 {
        self.config.context.compact_threshold
    }

    pub fn session_override(&self) -> &SessionOverride {
        &self.session.session_override
    }

    pub fn with_delegate_handler(mut self, handler: DelegateHandler) -> Self {
        self.tool_executor.delegate_handler = Some(handler);
        self
    }

    pub fn with_sub_delegator(mut self, delegator: Arc<super::sub_agent::SubAgentDelegator>) -> Self {
        self.tool_executor.sub_delegator = Some(delegator);
        self
    }

    pub fn with_change_rx(mut self, rx: watch::Receiver<super::watcher::ChangeSet>) -> Self {
        self.request_builder.set_change_rx(rx);
        self
    }

    /// Access the attachment manager (for /reload command).
    pub fn attachments(&mut self) -> &mut AttachmentManager {
        &mut self.request_builder.attachments
    }

    pub fn set_pending_retry(&mut self, msg: String) {
        self.pending_retry_message = Some(msg);
    }

    pub fn take_pending_retry(&mut self) -> Option<String> {
        self.pending_retry_message.take()
    }

    pub fn has_pending_retry(&self) -> bool {
        self.pending_retry_message.is_some()
    }

    pub async fn recover_interrupted_turn(&mut self) -> anyhow::Result<Option<String>> {
        self.recover_incomplete_turn(&crate::agents::agent_impl::types::StreamMode::Collect).await
    }
}
