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
use crate::api::message::Channel;

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

    /// Snapshot of in-flight turn tasks.
    pub fn active_count(self: &SharedTurnTracker) -> usize {
        self.active.load(Ordering::SeqCst)
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
                    "turn drain timed out — proceeding with in-flight turns (history may need recovery)"
                );
                return;
            }
            tracing::info!(
                active_turns = remaining,
                "waiting for in-flight turn tasks to finish"
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
#[derive(Clone)]
pub struct OrchestratorCtx {
    /// Live channels, keyed by (channel_type, account_id).
    pub channels: ChannelRegistry,
    /// SessionManager owns the SessionContext table (1:1 invariant).
    pub sessions: Arc<crate::agents::session::SessionManager>,
    /// AskRouter (RFC v2 §三.B): indexed by session.id, fulfilled by inbound
    /// messages ahead of process_turn. Shared with the daemon-built
    /// `AskUserTool`.
    pub ask: Arc<AskRouter>,
    /// Global user registry — replaces per-channel KnownSenders/RateLimiter.
    /// `inbound::dispatch` records every inbound message; slash commands query.
    pub known_users: Arc<crate::agents::KnownUsersRegistry>,
    /// P4 用户实体注册表（uid/email/username）——gate 判定、命令与工具
    /// 解析 user.id / email 共用。
    pub user_registry: Arc<crate::agents::UserRegistry>,
    /// AgentRuntime for the per-turn `Agent::run` path. Cloned into each turn.
    pub runtime: AgentRuntime,
    /// Delegation manager (shared with DelegateTaskTool via handler).
    pub delegator: Option<Arc<DelegationCoordinator>>,
    /// Shared scheduler for run-result tracking from cron tasks.
    pub scheduler: Option<crate::agents::SharedScheduler>,
    /// In-flight turn task counter for drain-on-hot-switch.
    pub turn_tracker: SharedTurnTracker,
    /// P2 (2026-08-13, RFC delegation-notice-queue §5): persistent delivery
    /// queue for delegation completion notices (at-least-once across
    /// restarts). `None` when the storage dir cannot be opened (degraded to
    /// P1 in-memory-only delivery) or in in-memory tests.
    pub completion_queue: Option<Arc<crate::agents::orchestrator::CompletionNoticeStore>>,
    /// RFC inbound-spool: persistent at-least-once spool for inbound channel
    /// messages (qqbot / telegram / wechat), written at `spawn_listener`
    /// before the message enters the event loop and marked `Done` after
    /// `inbound::dispatch` returns. `None` when the storage dir cannot be
    /// opened (degraded to in-memory-only delivery, fail-open) or in
    /// in-memory tests.
    pub inbound_spool: Option<Arc<crate::agents::orchestrator::InboundSpool>>,
    /// issue #131: shell process table, shared across all sessions. `None`
    /// in tests / bare CLI usage that never registers the shell tool.
    pub shell_registry: Option<crate::tools::shell::ShellRegistry>,
}

impl OrchestratorCtx {
    /// issue #131 decision 8 / issue #140: this session's currently-running
    /// tracked shell processes, newest first. Used only to build the
    /// read-only background-work status reminder injected into a user turn
    /// that interrupts pending async work — NOT for silenced/fold/drain
    /// gating.
    ///
    /// Those gates are computed synchronously in the same critical section
    /// as `record_terminal` (the 2026-08-10 E2E 恢复轮1 race fix), which
    /// rules out reading this `tokio::sync::RwLock`-backed registry live
    /// inside that section (would force an `.await` into a stretch that must
    /// stay yield-free). issue #140 covers shell there a different way
    /// instead — see `ShellTool::register_pending` /
    /// `SessionContext::add_pending_task`: the shell tool registers itself
    /// into the SAME `TurnSuspension.pending` list delegation uses, ahead of
    /// time, from inside its own tool call (already running synchronously
    /// within the turn that spawned it) — so `has_pending_async_work` covers
    /// shell for free, with no new read inside the race-sensitive section at
    /// all. This method stays a plain live registry read because the status
    /// reminder has no race to avoid — it's informational, never gates
    /// anything.
    pub async fn running_shell_processes(&self, session_id: &str) -> Vec<crate::tools::shell::ProcSummary> {
        let Some(registry) = &self.shell_registry else {
            return Vec::new();
        };
        let mut running: Vec<crate::tools::shell::ProcSummary> = registry
            .read()
            .await
            .values()
            .filter(|e| e.session_id() == session_id && e.is_running())
            .map(|e| e.summary())
            .collect();
        running.sort_by(|a, b| b.spawned_at_ms.cmp(&a.spawned_at_ms));
        running
    }

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

/// `OrchestratorHook` implementation for the orchestrator side of the
/// #151 Phase 3d SCC 解环: `scheduling_runtime` defines the trait
/// (consumer), the orchestrator implements it (producer), the daemon wires
/// it into `WebhookContext`. Thin delegation to the same functions the
/// removed direct calls used — no behavior change.
#[async_trait::async_trait]
impl crate::scheduling_runtime::scheduler::OrchestratorHook for OrchestratorCtx {
    async fn run_scheduled_turn(
        &self,
        session_key: &str,
        prompt: &str,
    ) -> anyhow::Result<String> {
        super::scheduled::run_scheduled_turn(self, session_key, prompt, None).await
    }

    fn outbound_channel(
        &self,
        channel_type: &str,
        account_id: &str,
    ) -> Option<Arc<dyn Channel>> {
        self.channels
            .get(&(channel_type.to_string(), account_id.to_string()))
    }
}

// P1 CI 回归守卫 (2026-08-13): `drain_delegation_notices` 被 `dispatch_turn` 的
// spawn 闭包 await, 闭包内逐级 await 的 future 必须 Send。死函数（非 cfg(test)）
// 让普通 `cargo check` 就能验证并给出字段级诊断。`unreachable!()` 只用于构造
// 类型（future 惰性, 不执行）。
// 2026-08-13 修复: 根因是 Send 证明环 —— drain 曾 await dispatch_turn, 而
// dispatch_turn 体内 spawn 的闭包又 await drain; 外部查各自 future 单独 Send
// 可通过（余归纳）, 但 spawn 现场无法闭合。现 drain 直接同步调用
// `dispatch_turn_spawn`（无 await 体）, 环已斩断。以下守卫验证斩环后的不变量:
// 两个 future 仍 Send + spawn 闭包/drain 跨 await 持有的类型 Send。
#[allow(dead_code, unreachable_code)]
fn _p1_drain_chain_send_guards() {
    fn require_send<F: std::future::Future + Send>(f: F) -> F {
        f
    }
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn never_ctx() -> &'static OrchestratorCtx {
        unreachable!()
    }
    fn never_key() -> &'static crate::agents::orchestrator::key::SessionKey {
        unreachable!()
    }
    fn never_msg() -> crate::api::message::ChannelInboundMessage {
        unreachable!()
    }
    fn never_str() -> &'static str {
        unreachable!()
    }

    drop(require_send(super::inbound::dispatch_turn(never_ctx(), never_key(), never_msg())));
    drop(require_send(super::delegation::drain_delegation_notices(never_ctx(), never_str())));
    assert_send::<crate::api::message::ChannelInboundMessage>();
    // spawn 闭包按 move 捕获 ctx、drain 跨 await 持有 key —— Sync 不够, 必须 Send
    assert_send::<OrchestratorCtx>();
    assert_send::<crate::agents::orchestrator::key::SessionKey>();
    // drain 跨 await 持有的状态
    assert_send::<crate::agents::session::Session>();
    assert_send::<std::sync::Arc<crate::agents::SessionContext>>();
    assert_sync::<std::sync::Arc<crate::agents::SessionContext>>();
    assert_send::<std::collections::VecDeque<crate::agents::DelegationNotice>>();
}
