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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;

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

/// Tracks the number of in-flight turn tasks so the orchestrator can drain
/// them before a hot switch (fork+execv). Without draining, a turn executing
/// tools when SIGUSR1 arrives gets killed mid-persist, leaving orphan
/// tool_calls that startup recovery blindly replays → crash loop.
pub struct TurnTracker {
    active: AtomicUsize,
    notify: Notify,
}

/// Shared handle; cheaply clonable.
pub type SharedTurnTracker = Arc<TurnTracker>;

impl TurnTracker {
    pub fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    /// Increment the active counter and return a guard that decrements on drop.
    /// Call **inside** the spawned task body so the counter is only released
    /// when the task truly finishes (or panics).
    pub fn track(self: &SharedTurnTracker) -> TurnGuard {
        self.active.fetch_add(1, Ordering::SeqCst);
        TurnGuard {
            tracker: Arc::clone(self),
        }
    }

    /// Wait until no turn tasks remain active, or `timeout` elapses (total,
    /// not per-iteration).
    pub async fn drain(self: &SharedTurnTracker, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = self.active.load(Ordering::SeqCst);
            if remaining == 0 {
                tracing::debug!("turn tracker: drained, no active turns");
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    active_turns = remaining,
                    timeout_secs = timeout.as_secs(),
                    "turn drain timed out — proceeding to hot switch with in-flight turns"
                );
                return;
            }
            tracing::info!(
                active_turns = remaining,
                "waiting for in-flight turn tasks to finish before hot switch"
            );
            let _ = tokio::time::timeout_at(deadline, self.notify.notified()).await;
        }
    }
}

impl Default for TurnTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard: decrements the active counter on drop and notifies the drainer.
pub struct TurnGuard {
    tracker: SharedTurnTracker,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let prev = self.tracker.active.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.tracker.notify.notify_one();
        }
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
    /// In-flight turn task counter for drain-on-hot-switch.
    pub turn_tracker: SharedTurnTracker,
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
