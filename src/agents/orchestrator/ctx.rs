//! `OrchestratorCtx` — the shared dependency bundle.
//!
//! Everything the orchestrator's handlers, interceptors, and spawned tasks
//! need to do their job, separated from the *runtime* (`Orchestrator`, which
//! owns the consume-once event receivers and listener handles). All fields are
//! cheap-to-clone `Arc`s, so tasks that must outlive a single turn hold an
//! `Arc<OrchestratorCtx>` rather than an `Arc<Orchestrator>`.
//!
//! The composition root (daemon.rs) and the webhook server both reach for the
//! same `Arc<OrchestratorCtx>` instead of going through per-field accessor
//! methods on `Orchestrator`.

use std::sync::Arc;

use dashmap::DashMap;

use crate::agents::{AgentRuntime, AskRouter, DelegationCoordinator, SessionContext};
use crate::channels::Channel;

/// Shared, cheaply-clonable dependencies used across the orchestrator.
pub struct OrchestratorCtx {
    /// Channels, keyed by (channel_type, account_id).
    pub channels: Arc<DashMap<(String, String), Arc<dyn Channel>>>,
    /// SessionManager owns the SessionContext table (1:1 invariant).
    pub session_manager: Arc<crate::agents::session::SessionManager>,
    /// AskRouter (RFC v2 §三.B): indexed by session.id, fulfilled by inbound
    /// messages ahead of process_turn. Shared with the daemon-built
    /// `AskUserTool`.
    pub ask_router: Arc<AskRouter>,
    /// AgentRuntime for the per-turn `Agent::run` path. Cloned into each turn.
    pub agent_runtime: AgentRuntime,
    /// Delegation manager (shared with DelegateTaskTool via handler).
    pub delegator: Option<Arc<DelegationCoordinator>>,
    /// Shared scheduler for run-result tracking from cron tasks.
    pub scheduler: Option<crate::agents::SharedScheduler>,
}

impl OrchestratorCtx {
    /// Get or create the `SessionContext` for a routing key.
    ///
    /// First call loads the Session from SessionManager (restoring from
    /// backend), wraps it in a `SessionContext`, and caches it; later calls
    /// return the same `Arc`.
    pub fn session_context_for(&self, sk: &str) -> Arc<SessionContext> {
        self.session_manager.get_or_create_context(sk)
    }

    /// Look up a channel by its (channel_type, account_id) pair.
    pub fn channel(&self, account: &(String, String)) -> Option<Arc<dyn Channel>> {
        self.channels.get(account).map(|r| r.clone())
    }
}
