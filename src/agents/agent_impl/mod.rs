//! AgentLoop + AgentSession — per-session turn execution engine.
//!
//! AgentLoop holds per-session mutable state; AgentSession wraps it as the
//! public API surface. Session creation is done via AgentRuntime.

use std::sync::Arc;

use tokio::sync::watch;

use crate::providers::ProviderRegistry;
use crate::config::agent::ContextConfig;

use super::session::SessionOverride;

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

use super::loop_breaker::LoopBreaker;
use super::session::{Session, PersistHook};
use crate::agents::prompt::SystemPromptConfig;
use crate::agents::attachment::AttachmentManager;
use super::tool_executor::ToolExecutor;
use super::context_engine::ContextEngine;

pub(crate) mod types;
mod run;
mod turn;
mod chat_loop;
mod tools;
mod compaction;
mod images;

use super::request_builder::RequestBuilder;

/// AgentConfig controls loop breaker thresholds and tool call limits.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard cap on tool calls per turn. 0 = unlimited.
    pub max_tool_calls: usize,
    /// System prompt builder config.
    pub prompt_config: SystemPromptConfig,
    /// Context window management settings.
    pub context: ContextConfig,
    /// Loop breaker exact-repeat threshold: N identical consecutive calls → break.
    pub loop_breaker_threshold: usize,
    /// Per-tool execution timeout in seconds (0 = no timeout).
    /// Does not apply to ask_user or agent_delegate (those have their own timeouts).
    pub tool_timeout_secs: u64,
    /// Model override for this session (session override > Agent-level default).
    pub model_override: Option<String>,
    /// Thinking/reasoning config override for this session.
    pub thinking_override: Option<crate::providers::ThinkingConfig>,
    /// Timezone offset in hours — passed to ResourceProvider for date injection.
    pub timezone_offset: i32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            prompt_config: SystemPromptConfig::default(),
            context: ContextConfig::default(),
            loop_breaker_threshold: 3,
            tool_timeout_secs: 180,
            model_override: None,
            thinking_override: None,
            timezone_offset: 8,
        }
    }
}

impl AgentConfig {
    pub fn with_override(&self, ov: &super::session::SessionOverride) -> Self {
        let mut cfg = self.clone();
        if let Some(ref permission_mode) = ov.permission_mode {
            cfg.prompt_config.permission_mode = *permission_mode;
        }
        if let Some(run_mode) = ov.run_mode {
            cfg.prompt_config.run_mode = run_mode;
        }
        if let Some(t) = ov.compact_threshold { cfg.context.compact_threshold = t; }
        if let Some(r) = ov.retain_work_units { cfg.context.retain_work_units = r; }
        if let Some(mtc) = ov.max_tool_calls   { cfg.max_tool_calls = mtc; }
        cfg.model_override = ov.model.clone();
        cfg.thinking_override = ov.to_thinking_config();
        cfg
    }

}

/// Per-session agent loop handle. Execute `run(user_message)` to process a message.
pub struct AgentLoop {
    pub(crate) registry: Arc<dyn ProviderRegistry>,
    pub(crate) config: AgentConfig,
    pub(crate) session: Session,
    // ── Message building + attachments + images + hot-reload ──
    pub(crate) request_builder: RequestBuilder,
    // ── Token tracking + compaction policy + execution (C21) ──
    pub(crate) context: ContextEngine,
    // ── Tool execution ──
    pub(crate) tool_executor: ToolExecutor,
    // ── Infrastructure ──
    pub(crate) loop_breaker: LoopBreaker,
    pub(crate) persist_hook: Option<Arc<dyn PersistHook>>,
    pub(crate) pending_retry_message: Option<String>,
}

/// Per-session mutable state wrapper.
///
/// Thin wrapper around `AgentLoop` that provides the public API surface.
/// All methods delegate to the inner `AgentLoop`.
pub struct AgentSession {
    pub(crate) loop_: AgentLoop,
}

impl AgentLoop {
    pub fn with_ask_user_handler(mut self, handler: AskUserHandler) -> Self {
        self.tool_executor.ask_user_handler = Some(handler);
        self
    }

    pub fn session(&self) -> &super::session::Session {
        &self.session
    }

    pub fn token_total(&self) -> u64 {
        self.context.token_total()
    }

    pub fn last_usage(&self) -> (u64, u64, u64) {
        self.context.last_usage()
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

    pub fn with_sub_delegator(mut self, delegator: Arc<super::sub_agent::DelegationCoordinator>) -> Self {
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

    pub fn session_mut(&mut self) -> &mut super::session::Session {
        &mut self.session
    }

    pub fn set_model_override(&mut self, model: Option<String>) {
        self.config.model_override = model;
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
        // Clear the flag unconditionally: calling this means we are actively
        // handling whatever incomplete state existed, so the orchestrator must
        // not treat the session as incomplete again after we return.
        self.session.incomplete_turn = false;
        self.recover_incomplete_turn(&crate::agents::agent_impl::types::StreamMode::Collect).await
    }
}
