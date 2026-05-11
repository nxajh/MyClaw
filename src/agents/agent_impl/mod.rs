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

use crate::providers::{ThinkingConfig};
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

pub(crate) mod types;
mod run;
mod tools;
mod compaction;
mod images;

pub(crate) use types::{estimate_message_tokens, TokenTracker};

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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            max_history: 200,
            prompt_config: SystemPromptConfig::default(),
            context: ContextConfig::default(),
            stream_chunk_timeout_secs: 90,
            max_output_bytes: 100 * 1024, // 100KB default
            loop_breaker_threshold: 3,
            tool_timeout_secs: 180,
        }
    }
}

impl AgentConfig {
    /// Return a copy of this config with the given session override applied.
    /// All override fields are `Option` — `None` means "keep the base value".
    pub fn with_override(&self, ov: &super::session_manager::SessionOverride) -> Self {
        let mut cfg = self.clone();
        if let Some(ref autonomy) = ov.autonomy {
            cfg.prompt_config.autonomy = *autonomy;
        }
        if let Some(t) = ov.compact_threshold { cfg.context.compact_threshold = t; }
        if let Some(r) = ov.retain_work_units { cfg.context.retain_work_units = r; }
        if let Some(mtc) = ov.max_tool_calls   { cfg.max_tool_calls = mtc; }
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
    /// Optional model override for sub-agents (e.g. summarizer uses a cheaper model).
    model_override: Option<String>,
    /// MCP server instructions: Vec<(server_name, instructions)>
    mcp_instructions: Vec<(String, String)>,
    /// Sub-agent configs (hot-reloadable).
    sub_agent_configs: Arc<RwLock<Vec<SubAgentConfig>>>,
    /// Workspace dirs for hot-reload scanning.
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

    /// Access the service registry (for slash commands).
    pub fn registry(&self) -> &Arc<dyn ServiceRegistry> {
        &self.registry
    }

    /// Access the tool registry (for slash commands).
    pub fn tools(&self) -> &Arc<super::tool_registry::ToolRegistry> {
        &self.tools
    }

    /// Access the skill manager (for slash commands).
    pub fn skills(&self) -> &Arc<RwLock<SkillManager>> {
        &self.skills
    }

    /// Access sub-agent configs (for slash commands and reload).
    pub fn sub_agent_configs(&self) -> &Arc<RwLock<Vec<SubAgentConfig>>> {
        &self.sub_agent_configs
    }

    /// Access workspace dirs.
    pub fn workspace_dir(&self) -> &str {
        &self.config.prompt_config.workspace_dir
    }

    /// Compact threshold ratio from agent config.
    pub fn compact_threshold(&self) -> f64 {
        self.config.context.compact_threshold
    }

    /// Set the system prompt directly (overrides builder).
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    /// Set a model override (used by sub-agents to route to specific models).
    pub fn with_model(mut self, model: String) -> Self {
        self.model_override = Some(model);
        self
    }

    /// Set MCP server instructions.
    pub fn with_mcp_instructions(mut self, instructions: Vec<(String, String)>) -> Self {
        self.mcp_instructions = instructions;
        self
    }

    /// Set sub-agent configs.
    pub fn with_sub_agent_configs(mut self, configs: Arc<RwLock<Vec<SubAgentConfig>>>) -> Self {
        self.sub_agent_configs = configs;
        self
    }

    /// Set workspace dirs for hot-reload scanning.
    pub fn with_workspace_dirs(mut self, skills_dir: PathBuf, agents_dir: PathBuf) -> Self {
        self.skills_dir = skills_dir;
        self.agents_dir = agents_dir;
        self
    }

    /// Create an AgentLoop for the given session.
    /// The system prompt is built from SystemPromptConfig + SkillManager.
    pub fn loop_for(&self, session: Session) -> AgentLoop {
        self.loop_for_with_persist(session, None)
    }

    /// Create an AgentLoop for the given session with an optional persist hook.
    pub fn loop_for_with_persist(
        &self,
        session: Session,
        persist_hook: Option<Arc<dyn PersistHook>>,
    ) -> AgentLoop {
        let ov = &session.session_override;

        // Merge base config with session overrides in one call.
        let config = self.config.with_override(ov);

        let prompt = if !self.system_prompt.is_empty() {
            self.system_prompt.clone()
        } else {
            let skills = self.skills.read();
            let builder = SystemPromptBuilder::new(config.prompt_config.clone());
            builder.build(&skills)
        };

        // Session override takes priority over Agent-level model override.
        let model_override = ov.model.clone().or_else(|| self.model_override.clone());
        let thinking_override = ov.to_thinking_config();
        let max_tool_calls = config.max_tool_calls;

        AgentLoop {
            registry: Arc::clone(&self.registry),
            tools: Arc::clone(&self.tools),
            config,
            session,
            system_prompt: prompt,
            ask_user_handler: None,
            delegate_handler: None,
            loop_breaker: LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls,
                exact_repeat_threshold: self.config.loop_breaker_threshold,
                ..LoopBreakerConfig::default()
            }),
            pending_image_urls: None,
            pending_image_base64: None,
            token_tracker: TokenTracker::default(),
            persist_hook,
            sub_delegator: None,
            model_override,
            thinking_override,
            attachments: AttachmentManager::new(),
            mcp_instructions: self.mcp_instructions.clone(),
            skills: Arc::clone(&self.skills),
            sub_agent_configs: Arc::clone(&self.sub_agent_configs),
            skills_dir: self.skills_dir.clone(),
            agents_dir: self.agents_dir.clone(),
            change_rx: None,
            pending_retry_message: None,
        }
    }
}

/// Per-session agent loop handle. Execute `run(user_message)` to process a message.
pub struct AgentLoop {
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) config: AgentConfig,
    pub(crate) session: Session,
    /// Template for the system prompt.
    pub(crate) system_prompt: String,
    /// Optional callback for ask_user tool.
    pub(crate) ask_user_handler: Option<AskUserHandler>,
    /// Optional callback for async agent_delegate.
    pub(crate) delegate_handler: Option<DelegateHandler>,
    /// Loop breaker — detects repetitive tool-call patterns.
    pub(crate) loop_breaker: LoopBreaker,
    /// Pending image URLs from the current user message (attached per-model in chat_loop).
    pub(crate) pending_image_urls: Option<Vec<String>>,
    /// Pending base64 image data from the current user message.
    pub(crate) pending_image_base64: Option<Vec<String>>,
    /// Token usage tracker for context window management.
    pub(crate) token_tracker: TokenTracker,
    /// Optional hook for persisting messages to the backend.
    pub(crate) persist_hook: Option<Arc<dyn PersistHook>>,
    /// Optional sub-agent delegator for compaction (shared with Orchestrator).
    pub(crate) sub_delegator: Option<Arc<super::sub_agent::SubAgentDelegator>>,
    /// Optional model override — forces a specific model instead of registry default.
    pub(crate) model_override: Option<String>,
    /// Optional thinking override from session override (None = use model config).
    pub(crate) thinking_override: Option<ThinkingConfig>,
    // ── Attachment (增量注入) ──
    /// Attachment manager — 追踪已通知 LLM 的 skills/agents/MCP 列表。
    pub(crate) attachments: AttachmentManager,
    /// MCP server instructions (startup 时一次性获取)。
    pub(crate) mcp_instructions: Vec<(String, String)>,
    /// Shared skill manager (RwLock for hot-reload).
    pub(crate) skills: Arc<RwLock<SkillManager>>,
    /// Sub-agent configs (RwLock for hot-reload).
    pub(crate) sub_agent_configs: Arc<RwLock<Vec<SubAgentConfig>>>,
    /// Workspace dirs for hot-reload scanning.
    pub(crate) skills_dir: PathBuf,
    pub(crate) agents_dir: PathBuf,
    /// File change receiver (None for sub-agents).
    pub(crate) change_rx: Option<watch::Receiver<super::watcher::ChangeSet>>,
    /// User message from a failed turn, awaiting user decision (retry or abort).
    /// Set by `run_turn_core` when the LLM returns an empty response after all retries.
    /// Consumed by the Orchestrator when the user clicks "retry" or "abort".
    pub(crate) pending_retry_message: Option<String>,
}

impl AgentLoop {
    /// Set the ask_user handler (called by Orchestrator to wire the channel).
    pub fn with_ask_user_handler(mut self, handler: AskUserHandler) -> Self {
        self.ask_user_handler = Some(handler);
        self
    }

    /// Get a reference to the current session.
    pub fn session(&self) -> &super::session_manager::Session {
        &self.session
    }

    /// Get the current estimated total tokens.
    pub fn token_total(&self) -> u64 {
        self.token_tracker.total_tokens()
    }

    /// Get a breakdown of the last API call's token usage.
    pub fn last_usage(&self) -> (u64, u64, u64) {
        (
            self.token_tracker.last_input(),
            self.token_tracker.last_cached(),
            self.token_tracker.last_output(),
        )
    }

    /// Get the compact threshold ratio from config.
    pub fn compact_threshold(&self) -> f64 {
        self.config.context.compact_threshold
    }

    /// Get the current session override.
    pub fn session_override(&self) -> &SessionOverride {
        &self.session.session_override
    }

    /// Set the delegate handler (called by Orchestrator to wire async delegation).
    pub fn with_delegate_handler(mut self, handler: DelegateHandler) -> Self {
        self.delegate_handler = Some(handler);
        self
    }

    /// Set the sub-agent delegator (used for compaction summarization).
    pub fn with_sub_delegator(mut self, delegator: Arc<super::sub_agent::SubAgentDelegator>) -> Self {
        self.sub_delegator = Some(delegator);
        self
    }

    /// Set the file change receiver (for hot-reload).
    pub fn with_change_rx(mut self, rx: watch::Receiver<super::watcher::ChangeSet>) -> Self {
        self.change_rx = Some(rx);
        self
    }

    /// Access the attachment manager (for /reload command).
    pub fn attachments(&mut self) -> &mut AttachmentManager {
        &mut self.attachments
    }

    /// Store a user message for retry after an empty response.
    pub fn set_pending_retry(&mut self, msg: String) {
        self.pending_retry_message = Some(msg);
    }

    /// Take the pending retry message (consumes it).
    pub fn take_pending_retry(&mut self) -> Option<String> {
        self.pending_retry_message.take()
    }

    /// Check if there's a pending retry message.
    pub fn has_pending_retry(&self) -> bool {
        self.pending_retry_message.is_some()
    }
}
