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

use super::key::SessionKey;
use crate::agents::{AgentRuntime, AskRouter, DelegationCoordinator, SessionContext};
use crate::channels::Channel;

/// The set of live channels, keyed by `(channel_type, account_id)`.
///
/// A thin newtype over the underlying map so lookups go through a typed seam
/// (`get` / `get_by_key`) instead of raw `DashMap` access scattered across the
/// codebase. Cheap to clone (the map is behind an `Arc`).
#[derive(Clone, Default)]
pub struct ChannelRegistry {
    inner: Arc<DashMap<(String, String), Arc<dyn Channel>>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, account: (String, String), channel: Arc<dyn Channel>) {
        self.inner.insert(account, channel);
    }

    /// Look up a channel by its `(channel_type, account_id)` pair.
    pub fn get(&self, account: &(String, String)) -> Option<Arc<dyn Channel>> {
        self.inner.get(account).map(|r| r.clone())
    }

    /// Look up the channel that owns `key`'s session.
    pub fn get_by_key(&self, key: &SessionKey) -> Option<Arc<dyn Channel>> {
        self.get(&key.account_key())
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Shared, cheaply-clonable dependencies used across the orchestrator.
pub struct OrchestratorCtx {
    /// Live channels, keyed by (channel_type, account_id).
    pub channels: ChannelRegistry,
    /// SessionManager owns the SessionContext table (1:1 invariant).
    pub sessions: Arc<crate::agents::session::SessionManager>,
    /// AskRouter (RFC v2 §三.B): indexed by session.id, fulfilled by inbound
    /// messages ahead of process_turn. Shared with the daemon-built
    /// `AskUserTool`.
    pub ask: Arc<AskRouter>,
    /// AgentRuntime for the per-turn `Agent::run` path. Cloned into each turn.
    pub runtime: AgentRuntime,
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
        self.sessions.get_or_create_context(sk)
    }

    /// Look up a channel by its (channel_type, account_id) pair.
    pub fn channel(&self, account: &(String, String)) -> Option<Arc<dyn Channel>> {
        self.channels.get(account)
    }
}
