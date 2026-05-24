//! Agent execution — the RFC v2 `Agent` + `AgentSession` implementation.
//!
//! C17: `Agent { config }` — stateless identity.
//! C18: `Agent::run()` — one turn, using AgentSession for mutable state.
//! H45: AgentLoop becomes a private implementation detail.
//!
//! Architecture:
//! - `Agent` — stateless config (SubAgentConfig), cheap to clone.
//! - `AgentSession` — per-session mutable state (session, policy, loop_breaker, etc).
//! - `AgentRuntime` — shared process-wide resources (providers, tools, skills).
//! - `Agent::run()` — executes one turn using all three.

use std::sync::Arc;

use tokio::sync::watch;

use crate::config::sub_agent::SubAgentConfig;
use crate::agents::session::Session;
use crate::agents::prompt::SystemPromptBuilder;
use crate::agents::attachment::AttachmentManager;

use super::runtime::AgentRuntime;
use super::agent_impl::{AgentConfig, AgentLoop, AgentSession, AskUserHandler, DelegateHandler};

/// RFC v2 Agent — stateless identity = config only.
#[derive(Debug, Clone)]
pub struct Agent {
    pub config: SubAgentConfig,
}

impl Agent {
    pub fn new(config: SubAgentConfig) -> Self {
        Self { config }
    }

    /// Create an AgentSession for this agent + session combination.
    ///
    /// Equivalent to the old `Agent(factory).loop_for(session)`.
    /// The caller owns the AgentSession and passes it to `run()` on each turn.
    pub fn create_session(
        &self,
        session: Session,
        runtime: &AgentRuntime,
        persist_hook: Option<Arc<dyn PersistHook>>,
    ) -> AgentSession {
        let agent_config = self.build_agent_config(runtime);
        let system_prompt = self.build_system_prompt(runtime, &agent_config);

        let resources = ResourceProvider::new(
            Arc::clone(runtime.skills()),
            runtime.sub_agent_configs(),
            runtime.mcp_instructions().to_vec(),
            runtime.skills_dir(),
            runtime.agents_dir(),
            runtime.knowledge_dir.to_string_lossy().to_string(),
            agent_config.timezone_offset,
        );
        let request_builder = RequestBuilder::new(system_prompt, Arc::clone(&resources));

        let loop_ = AgentLoop {
            registry: Arc::clone(runtime.registry()),
            compactor: CompactionExecutor::new(
                Arc::clone(runtime.registry()),
                Arc::clone(&resources),
                Arc::clone(runtime.tools()),
            ),
            tool_executor: ToolExecutor::new(Arc::clone(runtime.tools()), agent_config.tool_timeout_secs),
            config: agent_config,
            session,
            request_builder,
            loop_breaker: LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls: self.config.max_tool_calls.unwrap_or(50),
                exact_repeat_threshold: runtime.loop_breaker_defaults.exact_repeat_threshold,
                ..LoopBreakerConfig::default()
            }),
            policy: CompactionPolicy::from_context_config(&crate::config::agent::ContextConfig::default()),
            persist_hook,
            pending_retry_message: None,
        };

        AgentSession { loop_ }
    }

    fn build_agent_config(&self, runtime: &AgentRuntime) -> AgentConfig {
        let mut config = AgentConfig::default();
        if let Some(max) = self.config.max_tool_calls {
            config.max_tool_calls = max;
        }
        if let Some(ref model) = self.config.model {
            config.model_override = Some(model.clone());
        }
        config.tool_timeout_secs = runtime.tool_timeout_secs;
        config
    }

    fn build_system_prompt(&self, runtime: &AgentRuntime, agent_config: &AgentConfig) -> String {
        if !self.config.system_prompt.is_empty() {
            self.config.system_prompt.clone()
        } else {
            let skills = runtime.skills().read();
            let builder = SystemPromptBuilder::new(agent_config.prompt_config.clone());
            builder.build(&skills)
        }
    }

    /// Build tool specs filtered by this agent's config.
    pub fn filtered_tool_specs(
        &self,
        runtime: &AgentRuntime,
    ) -> Vec<crate::providers::capability_chat::ToolSpec> {
        runtime.tools()
            .all_tools()
            .iter()
            .filter(|t| {
                let source = t.source();
                match &source {
                    crate::providers::capability_tool::ToolSource::Builtin => {
                        self.config.allows_tool(t.name())
                    }
                    crate::providers::capability_tool::ToolSource::Skill { name } => {
                        self.config.allows_skill(name)
                    }
                    crate::providers::capability_tool::ToolSource::Mcp { server } => {
                        self.config.allows_mcp(server)
                    }
                }
            })
            .map(|t| {
                let spec = t.spec();
                crate::providers::capability_chat::ToolSpec {
                    name: spec.name,
                    description: Some(spec.description),
                    input_schema: spec.parameters,
                }
            })
            .collect()
    }
}

/// Per-session mutable state wrapper (defined in agent_impl, methods here).
///
/// See [`super::agent_impl::AgentSession`] for the struct definition.
/// Methods are split across `agent_impl/mod.rs` (builder methods) and here
/// (delegation methods that don't need the old factory).
impl AgentSession {
    /// Execute a single turn (non-streaming).
    pub async fn run(
        &mut self,
        user_message: &str,
        image_urls: Option<Vec<String>>,
        image_base64: Option<Vec<String>>,
    ) -> anyhow::Result<String> {
        self.loop_.run(user_message, image_urls, image_base64).await
    }

    /// Execute a single turn (streaming events to a channel).
    pub async fn run_streamed(
        &mut self,
        user_message: &str,
        image_urls: Option<Vec<String>>,
        image_base64: Option<Vec<String>>,
        event_tx: tokio::sync::mpsc::Sender<super::TurnEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        self.loop_.run_streamed(user_message, image_urls, image_base64, event_tx, cancel).await
    }

    /// Access the underlying session (read-only).
    pub fn session(&self) -> &Session {
        &self.loop_.session
    }

    /// Access the underlying session (mutable).
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.loop_.session
    }

    /// Token tracking.
    pub fn token_total(&self) -> u64 { self.loop_.token_total() }
    pub fn last_usage(&self) -> (u64, u64, u64) { self.loop_.last_usage() }
    pub fn compact_threshold(&self) -> f64 { self.loop_.compact_threshold() }

    /// Session override.
    pub fn session_override(&self) -> &crate::agents::session::SessionOverride {
        self.loop_.session_override()
    }

    /// Pending retry for empty response handling.
    pub fn set_pending_retry(&mut self, msg: String) { self.loop_.set_pending_retry(msg); }
    pub fn take_pending_retry(&mut self) -> Option<String> { self.loop_.take_pending_retry() }
    pub fn has_pending_retry(&self) -> bool { self.loop_.has_pending_retry() }

    /// Attachments.
    pub fn attachments(&mut self) -> &mut AttachmentManager { self.loop_.attachments() }

    /// Wire ask_user handler.
    pub fn with_ask_user_handler(mut self, handler: AskUserHandler) -> Self {
        self.loop_ = self.loop_.with_ask_user_handler(handler);
        self
    }

    /// Wire delegate handler.
    pub fn with_delegate_handler(mut self, handler: DelegateHandler) -> Self {
        self.loop_ = self.loop_.with_delegate_handler(handler);
        self
    }

    /// Wire sub-delegator.
    pub fn with_sub_delegator(mut self, delegator: Arc<DelegationCoordinator>) -> Self {
        self.loop_ = self.loop_.with_sub_delegator(delegator);
        self
    }

    /// Wire change receiver for hot-reload.
    pub fn with_change_rx(mut self, rx: watch::Receiver<crate::agents::watcher::ChangeSet>) -> Self {
        self.loop_ = self.loop_.with_change_rx(rx);
        self
    }

    /// Set model override for this session.
    pub fn set_model_override(&mut self, model: Option<String>) {
        self.loop_.config.model_override = model;
    }

    /// Apply a session override (from slash commands) to the live loop.
    pub fn apply_session_override(&mut self, ov: crate::agents::session::SessionOverride) {
        self.loop_.apply_session_override(ov);
    }

    /// Recover incomplete turn from previous session.
    pub async fn recover_interrupted_turn(&mut self) -> anyhow::Result<Option<String>> {
        self.loop_.recover_interrupted_turn().await
    }

    /// Force compaction of the session history.
    pub async fn compact_now(&mut self, model_id: &str) -> anyhow::Result<()> {
        self.loop_.compact_now(model_id).await
    }

    /// Access the inner AgentLoop for backward compatibility.
    pub fn as_loop(&self) -> &AgentLoop { &self.loop_ }
    pub fn as_loop_mut(&mut self) -> &mut AgentLoop { &mut self.loop_ }
}
