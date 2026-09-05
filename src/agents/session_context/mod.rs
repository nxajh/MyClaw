//! `SessionContext` — bundles a `Session` with everything the per-turn
//! pipeline needs.
//!
//! RFC v2 §三.A: SessionContext is the boundary that owns per-session
//! mutable state (attachments, pending retry, turn lock) and drives
//! `process_turn`. `Agent::run` takes `&mut Session` plus a `TurnContext`
//! (already-resolved decisions); SessionContext is what the Orchestrator
//! holds in its session table and what `process_turn` operates on per turn.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::agents::session::{PersistHook, Session};

pub(crate) mod helpers;
pub(crate) mod suspension;
pub(crate) mod tts;

use crate::agents::turn::{PreviewState, SubResult, SubStatus, TurnResult, TurnSuspension};
use crate::agents::{Agent, AgentRuntime, TurnContext, UserProfile};
use crate::api::message::{Channel, ChannelInboundMessage};
pub(crate) use helpers::{age_session_media, history_looks_incomplete, is_bare_continue};
pub use suspension::TerminalRecord;
pub(crate) use suspension::{decide_silenced, semantic_stop_reason};
pub(crate) use tts::prepare_text_for_tts;

/// Per-session bundle held by the SessionManager's session-context table.
///
/// Fields:
/// - `session`: owns the conversation state (Session — history, override, …)
/// - `agent`: Agent bound to this session (resolved from `Session.agent_name`
///   at construction time; reused across every turn so per-session
///   dispatch doesn't re-look-up the SubAgentConfig)
/// - `pending_retry`: user message saved when last turn ended abnormally;
///   surfaced as a "retry?" prompt next time the user types
/// - `turn_lock`: tokio Mutex held for the duration of `process_turn`,
///   ensuring two messages on the same session do not race the LLM
/// - `user_profile`: snapshot of `workspace/users/{user_id}/profile.toml` at
///   session load time. Used by SystemPromptBuilder.build_with_profile so
///   the LLM sees a stable "## User" section. Refreshed via
///   `reload_user_profile` when the profile file changes mid-session.
pub struct SessionContext {
    /// Mutable session state. Wrapped in Mutex so the turn lock and the
    /// Session itself share the same critical section.
    pub session: Arc<Mutex<Session>>,
    /// Hex session id, copied at construction so the coordinator can locate
    /// this context by session id without locking `session` (which is held
    /// for the whole turn). Used by 方案 C pending registration.
    pub session_id: String,
    /// Agent bound to this session at creation time. Built from
    /// `Session.agent_name` via `SessionManager.build_agent_for_session`.
    pub agent: Arc<Agent>,
    /// User message saved when the previous turn ended with an empty LLM
    /// response or interrupted streaming. Cleared once retried.
    pub pending_retry: Arc<Mutex<Option<String>>>,
    /// Serializes `process_turn` per session. Distinct from `session`'s
    /// Mutex because some readers want to peek at session state without
    /// blocking on an in-flight turn.
    pub turn_lock: Arc<Mutex<()>>,
    /// 方案 C (docs/turn-suspension-rfc.md): non-None while the parent turn
    /// is suspended on async delegations. `std::sync::Mutex` (no await in
    /// the critical section): registration happens on the sync
    /// `spawn_delegate_async` path inside `Agent::run`'s tool execution,
    /// while `process_turn` holds `session`'s tokio Mutex.
    pub turn_suspension: std::sync::Mutex<Option<TurnSuspension>>,
    /// 方案 C (RFC §5): persist hook for `sessions/<sid>/suspension.json`,
    /// cloned from `Session.persist` at construction. Suspension writes
    /// (`add_pending_task` / `add_progress` / `record_terminal` /
    /// `clear_suspension_if_collected`) happen while `session`'s tokio
    /// Mutex is held, so they cannot reach `session.persist` — this copy
    /// keeps the durable state writable on those paths.
    pub suspension_persist: Option<Arc<dyn PersistHook>>,
    /// Loaded UserProfile snapshot taken at SessionContext creation.
    /// Immutable for the lifetime of the context — per RFC §三.A reload
    /// semantics drop the SessionContext and let `SessionManager`
    /// rematerialize it from a fresh profile read.
    pub user_profile: Arc<UserProfile>,
    /// 单 preview (2026-08-12): delegation-notice turns that have been
    /// dispatched but not yet finished — incremented synchronously at
    /// dispatch time (`route_notice` active branch / `process_non_active`,
    /// same sync section as `record_terminal`), decremented when the notice
    /// turn exits `process_turn` (RAII guard, every path). Feeds the
    /// suspension-sequence end determination: `pending` may be EMPTY (the
    /// wake burst already collected every terminal) while notices are still
    /// queued behind `turn_lock` — the suspension (and its cross-turn
    /// preview) must survive until the LAST notice turn finishes. Runtime-
    /// only, not persisted (a daemon restart re-enters via `suspension.json`).
    pub notice_turns_in_flight: AtomicUsize,
    /// P1 (2026-08-13, RFC delegation-notice-queue §4): delegation completion
    /// notices enqueued while `turn_lock` is busy. Enqueued by `route_notice`
    /// (wake path), drained by `drain_delegation_notices` — dispatch-time
    /// idle (wake sees `try_lock` succeed) or turn-end (dispatch_turn tail).
    /// FIFO; deduped by notice id within a drain pass. Runtime-only: NOT
    /// persisted (P2 adds the persistent delivery queue).
    pub delegation_notice_queue: std::sync::Mutex<std::collections::VecDeque<DelegationNotice>>,
    /// Per-turn cancel token. Recreated each turn by `process_turn`.
    /// `/stop` fires this to cancel the in-flight turn without locking `session`.
    pub turn_cancel: std::sync::Mutex<tokio_util::sync::CancellationToken>,
    /// issue #224: sub_session_ids `record_terminal` has recorded recently,
    /// kept for a bounded time window so a repeat call for the same id
    /// after `turn_suspension` clears (routine — happens as soon as every
    /// pending task is collected) still gets caught as a duplicate.
    /// `record_terminal`'s own in-`TurnSuspension` `results` dedup only
    /// works while suspension state still exists; once cleared, the
    /// suspension-based check is bypassed entirely (`NoSuspension`), and
    /// that path is a deliberate fall-through for the "session reloaded
    /// while running" case (#129) — so it cannot simply be made stricter.
    /// Runtime-only (not persisted — a restart's dedup is #220/#223's
    /// concern, not this one) and time-bounded rather than unbounded, to
    /// avoid the same unbounded-growth class of issue #214/#222 already
    /// hit elsewhere in a long-lived session.
    pub(crate) recently_recorded_terminals: std::sync::Mutex<Vec<(String, std::time::Instant)>>,
    /// issue #238: events (sub-agent/shell completions and, eventually, user
    /// interjections) queued for whenever this session next has an
    /// outstanding `sessions_yield` tool_call to deliver them to. Drained in
    /// one batch by `try_fill_pending_yield`, never one-at-a-time — see the
    /// #238 discussion for why streaming consumption was rejected (token
    /// blowup, intermediate-state hallucination risk). issue #240: persisted
    /// to `sessions/<sid>/pending_yield_events.json` via the same
    /// `PersistHook` clone used for `turn_suspension` (`suspension_persist`)
    /// — every mutation (`enqueue_pending_yield_event`,
    /// `take_pending_yield_events`) re-persists the queue, and it is
    /// restored at `SessionContext` construction, so a daemon restart no
    /// longer drops events that arrived before the outstanding
    /// `sessions_yield` was filled.
    pub pending_yield_events: std::sync::Mutex<std::collections::VecDeque<PendingYieldEvent>>,
    /// Phase 2b (issue #256): the live, in-process continuation for a
    /// `sessions_yield` currently parked in `run_and_deliver`'s
    /// `park_for_yield` — `None` whenever nothing is actually parked right
    /// now (no yield outstanding, or an outstanding one survived a daemon
    /// restart with nothing live to wake). A single slot: registering a new
    /// waiter overwrites (and thus drops/cancels) whatever was there,
    /// which is exactly the desired behavior when a new `sessions_yield`
    /// call or a user interjection supersedes an older pending one — the
    /// old parked task's `rx.await` resolves to `Err` and it gives up
    /// without touching history a second time. Runtime-only, never
    /// persisted: a restart has nothing live to reconnect to, so recovery
    /// falls back to the pre-existing `pending_yield_events` +
    /// `try_fill_pending_yield` + `resume_after_yield` path unchanged.
    pub(crate) yield_waiter: std::sync::Mutex<Option<YieldWaiter>>,
}

/// Phase 2b (issue #256): see `SessionContext::yield_waiter`.
pub(crate) struct YieldWaiter {
    /// The tool_call_id this waiter is parked on — must match
    /// `Session::pending_yield`'s id for a delivery to be accepted as
    /// "still ours" (a supersede/interjection may have moved on).
    pub tool_call_id: String,
    /// Sent exactly once: the combined content to deliver as this
    /// tool_call's result. Dropping the sender without sending (which
    /// happens automatically when a new waiter overwrites this slot)
    /// signals cancellation — the parked task's `rx.await` resolves to
    /// `Err` and it gives up.
    pub tx: tokio::sync::oneshot::Sender<String>,
}

/// issue #238: see `SessionContext::pending_yield_events`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingYieldEvent {
    pub content: String,
}

/// issue #238: render the accumulated queue as ONE combined tool_result —
/// batched, not delivered one at a time (the #238 discussion rejected
/// streaming consumption: token blowup from N separate wake round-trips,
/// and intermediate-state hallucination risk from the model not knowing
/// more results are already on their way). English: this feeds the model
/// as an ordinary tool_result, not the user directly — same convention as
/// `latest_entry_summary` (#237); the model paraphrases to the user in
/// their own language on its own next turn either way.
fn format_pending_yield_events(events: &[PendingYieldEvent]) -> String {
    if events.len() == 1 {
        return events[0].content.clone();
    }
    let mut out = String::from("Since you yielded, the following happened:\n");
    for e in events {
        out.push_str("- ");
        out.push_str(&e.content);
        out.push('\n');
    }
    out
}

/// issue #238: spawn `resume_after_yield` as an independent task — used by
/// `try_fill_pending_yield`'s cold/fallback path (no live waiter to signal,
/// e.g. after a daemon restart). Extracted into a plain (non-async)
/// function so its `tokio::spawn` call isn't inlined into an async body.
/// `try_fill_pending_yield` → `resume_after_yield` → `run_and_deliver` is a
/// genuine call cycle across async fns on the same impl block, and Rust
/// cannot resolve their opaque `impl Future` return types when any edge of
/// that cycle is a direct (non-boxed) async-fn call — routing the spawn
/// through a plain function breaks the cycle at the type level without
/// changing behavior (the runtime call graph is identical either way).
fn spawn_resume_after_yield(sctx: Arc<SessionContext>, runtime: AgentRuntime) {
    tokio::spawn(async move {
        if let Err(e) = sctx.resume_after_yield(runtime).await {
            tracing::error!(
                session_id = %sctx.session_id,
                err = %e,
                "issue #238: resume_after_yield failed"
            );
        }
    });
}

/// issue #224: how long a `record_terminal`'d sub_session_id is remembered
/// for duplicate detection after `turn_suspension` clears. Matches the 1h
/// convention already established by `STALE_LOST_NOTICE_AFTER_SECS`
/// (#216/#222) elsewhere in this series.
const RECENTLY_RECORDED_TERMINAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);

/// P1 (2026-08-13, RFC delegation-notice-queue §4): a delegation completion
/// notice enqueued while the session's `turn_lock` is busy (a user turn or
/// another notice turn is running). Enqueued by `route_notice`, drained by
/// `drain_delegation_notices` (dispatch-time idle / turn-end). Runtime-only:
/// NOT persisted — P2 adds the persistent delivery queue.
#[derive(Debug, Clone)]
pub struct DelegationNotice {
    /// Unique synthetic message id — dedup key within a drain pass (a
    /// terminal event has one notice; Message events have unique ids).
    pub id: String,
    /// Rendered notice text (terminal summary or sub-agent message).
    pub content: String,
    /// Wake-time silence intent snapshot (see `silenced_override` doc in
    /// `ChannelInboundMessage`).
    pub silenced_override: Option<bool>,
}

/// 单 preview (2026-08-12): RAII guard decrementing the per-session
/// in-flight notice-turn counter when a delegation-notice turn exits
/// `process_turn` — on EVERY path (normal end, early return, error). The
/// counter is incremented at dispatch time (sync with `record_terminal`), so
/// a notice queued behind `turn_lock` still counts toward the suspension-
/// sequence end determination: `pending` may be empty (the wake burst already
/// collected every terminal) while notices are still queued — clearing the
/// suspension then would drop the cross-turn preview and make each queued
/// notice open its own message (the multi-message spam bug, 2026-08-12).
struct NoticeTurnGuard {
    sctx: Arc<SessionContext>,
    active: bool,
    done: bool,
}

impl NoticeTurnGuard {
    fn new(sctx: &Arc<SessionContext>, is_notice: bool) -> Self {
        Self {
            sctx: sctx.clone(),
            active: is_notice,
            done: false,
        }
    }

    /// Decrement now (main turn path) — must run BEFORE
    /// `clear_suspension_if_collected` so the end-of-sequence check no longer
    /// counts the current turn. Idempotent; `Drop` covers the early-return /
    /// error paths.
    fn finish(&mut self) {
        if self.active && !self.done {
            self.sctx.finish_notice_turn();
            self.done = true;
        }
    }
}

impl Drop for NoticeTurnGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

impl SessionContext {
    pub fn new(session: Session, agent: Arc<Agent>) -> Self {
        // Clone the persist hook BEFORE moving `session` into the Mutex —
        // suspension writes happen while that tokio Mutex is held and cannot
        // reach `session.persist` directly (RFC §5).
        let suspension_persist = session.persist.clone();
        let ctx = Self {
            session_id: session.id.clone(),
            session: Arc::new(Mutex::new(session)),
            agent,
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
            turn_suspension: std::sync::Mutex::new(None),
            suspension_persist,
            notice_turns_in_flight: AtomicUsize::new(0),
            delegation_notice_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            turn_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            user_profile: Arc::new(UserProfile::default()),
            recently_recorded_terminals: std::sync::Mutex::new(Vec::new()),
            pending_yield_events: std::sync::Mutex::new(std::collections::VecDeque::new()),
            yield_waiter: std::sync::Mutex::new(None),
        };
        ctx.restore_suspension();
        ctx.restore_pending_yield_events();
        ctx
    }

    /// Build with a pre-loaded user profile (the path
    /// `SessionManager::get_or_create_context_with` takes).
    pub fn with_profile(session: Session, agent: Arc<Agent>, profile: Arc<UserProfile>) -> Self {
        let suspension_persist = session.persist.clone();
        let ctx = Self {
            session_id: session.id.clone(),
            session: Arc::new(Mutex::new(session)),
            agent,
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
            turn_suspension: std::sync::Mutex::new(None),
            suspension_persist,
            notice_turns_in_flight: AtomicUsize::new(0),
            delegation_notice_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            turn_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            user_profile: profile,
            recently_recorded_terminals: std::sync::Mutex::new(Vec::new()),
            pending_yield_events: std::sync::Mutex::new(std::collections::VecDeque::new()),
            yield_waiter: std::sync::Mutex::new(None),
        };
        ctx.restore_suspension();
        ctx.restore_pending_yield_events();
        ctx
    }

    /// Shared restore skeleton for a per-session persisted JSON field, used
    /// by both `restore_suspension` and `restore_pending_yield_events` (the
    /// two share an identical shape: no hook installed → no-op, nothing
    /// persisted yet → no-op, corrupt JSON → warn and ignore, fail open
    /// rather than blocking session load). `load` fetches the raw JSON via
    /// the installed hook; `apply` installs the parsed value into the
    /// caller's own mutex-guarded field and does its own success logging
    /// (the two callers use different field names — `pending` vs `count` —
    /// so that part stays caller-specific rather than being forced generic).
    /// `field` names the file for the corrupt-JSON warning.
    fn restore_persisted_field<T, L, A>(&self, field: &'static str, load: L, apply: A)
    where
        T: serde::de::DeserializeOwned,
        L: FnOnce(&dyn PersistHook, &str) -> Option<String>,
        A: FnOnce(T),
    {
        let Some(hook) = &self.suspension_persist else {
            return;
        };
        let Some(json) = load(hook.as_ref(), &self.session_id) else {
            return;
        };
        if json.trim().is_empty() {
            return;
        }
        match serde_json::from_str::<T>(&json) {
            Ok(value) => apply(value),
            Err(e) => {
                tracing::warn!(
                    session = %self.session_id,
                    field,
                    err = %e,
                    "persisted field corrupt; ignoring"
                );
            }
        }
    }

    /// Write-side counterpart of `restore_persisted_field`, shared by
    /// `persist_suspension` and `persist_pending_yield_events`. `value` is
    /// the current in-memory state — `None` (or, for the queue, an empty
    /// collection mapped to `None` by the caller) persists as `""`, the
    /// file backend's convention for "delete the file". No-op without a
    /// persist hook.
    fn persist_persisted_field<T, S>(&self, value: Option<&T>, save: S)
    where
        T: serde::Serialize,
        S: FnOnce(&dyn PersistHook, &str, &str),
    {
        let Some(hook) = &self.suspension_persist else {
            return;
        };
        let json = match value {
            Some(v) => match serde_json::to_string(v) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(session = %self.session_id, err = %e, "serialize persisted field failed");
                    return;
                }
            },
            None => String::new(),
        };
        save(hook.as_ref(), &self.session_id, &json);
    }

    /// 方案 C (RFC §5): hydrate `turn_suspension` from
    /// `sessions/<sid>/suspension.json` at construction (daemon-restart
    /// recovery). Corrupt JSON is warned about and ignored — the session
    /// just starts loud like an unsuspended one.
    fn restore_suspension(&self) {
        self.restore_persisted_field::<TurnSuspension, _, _>(
            "suspension.json",
            |hook, sid| hook.load_suspension(sid),
            |s| {
                let pending = s.pending.len();
                *self
                    .turn_suspension
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(s);
                tracing::info!(
                    session = %self.session_id,
                    pending = pending,
                    "restored suspended turn from disk"
                );
            },
        );
    }

    /// 方案 C (RFC §5): write the current `turn_suspension` to
    /// `sessions/<sid>/suspension.json`; `None` → empty string (file
    /// deleted). No-op without a persist hook. Called after every mutation
    /// point so a crash/restart never loses collected progress.
    fn persist_suspension(&self) {
        let guard = self
            .turn_suspension
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.persist_persisted_field(guard.as_ref(), |hook, sid, json| {
            hook.save_suspension(sid, json)
        });
    }

    /// issue #240: hydrate `pending_yield_events` from
    /// `sessions/<sid>/pending_yield_events.json` at construction
    /// (daemon-restart recovery), mirroring `restore_suspension`. Corrupt
    /// JSON is warned about and ignored — the session just starts with an
    /// empty queue.
    fn restore_pending_yield_events(&self) {
        self.restore_persisted_field::<Vec<PendingYieldEvent>, _, _>(
            "pending_yield_events.json",
            |hook, sid| hook.load_pending_yield_events(sid),
            |events| {
                let count = events.len();
                *self
                    .pending_yield_events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = events.into_iter().collect();
                tracing::info!(
                    session = %self.session_id,
                    count = count,
                    "restored pending yield events from disk"
                );
            },
        );
    }

    /// issue #240: write the current `pending_yield_events` queue to
    /// `sessions/<sid>/pending_yield_events.json`; an empty queue clears the
    /// file. No-op without a persist hook. Called after every mutation
    /// (`enqueue_pending_yield_event`, `take_pending_yield_events`) so a
    /// crash/restart never loses events queued before the outstanding
    /// `sessions_yield` was filled.
    fn persist_pending_yield_events(&self) {
        let events: Vec<PendingYieldEvent> = self
            .pending_yield_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        let arg = if events.is_empty() { None } else { Some(&events) };
        self.persist_persisted_field(arg, |hook, sid, json| {
            hook.save_pending_yield_events(sid, json)
        });
    }

    /// Snapshot the session for read-only consumers (e.g., /status commands).
    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }

    /// Stash rendered per-turn injections (RFC §3.5/§4.3: user-level mailbox
    /// messages + pending friend requests). Called by `dispatch_turn` right
    /// before `process_turn`; `Agent::run` injects them into the first LLM
    /// request and clears the field.
    pub async fn stash_turn_injections(&self, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        let mut session = self.session.lock().await;
        session.turn_injections.extend(texts);
    }

    /// 方案 C: register a pending async work item against this session's
    /// suspension — `sub_session_id` is a misnomer kept for the delegation
    /// caller (`spawn_delegate_async`); it's really just an opaque unique id
    /// (std Mutex, no await). issue #140: `ShellTool::register_pending` calls
    /// this same method with a shell `process_id` instead, which is what
    /// makes `has_pending_async_work` (and everything gated on it —
    /// silenced/fold/drain/`set_preview`) automatically cover shell
    /// background work too, without either caller knowing about the other.
    pub fn add_pending_task(&self, sub_session_id: String) {
        {
            let mut guard = self
                .turn_suspension
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(s) => s.add_pending(sub_session_id),
                None => *guard = Some(TurnSuspension::new(sub_session_id)),
            }
        }
        self.persist_suspension();
    }

    /// 方案 C / issue #140: true while the turn is suspended on uncollected
    /// async work — sub-agent delegations and/or armed shell background
    /// processes, both registered into the same `TurnSuspension.pending` via
    /// `add_pending_task`.
    pub fn has_pending_async_work(&self) -> bool {
        self.turn_suspension
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| !s.pending.is_empty())
            .unwrap_or(false)
    }

    /// 单 preview (2026-08-12): mark a delegation-notice turn as in-flight —
    /// called at dispatch time (sync section right after `record_terminal`,
    /// no await in between), so a notice queued behind `turn_lock` still
    /// counts toward the suspension-sequence end determination even after
    /// `pending` emptied (wake burst). See `notice_turns_in_flight` doc.
    pub fn bump_notice_turn(&self) {
        self.notice_turns_in_flight.fetch_add(1, Ordering::SeqCst);
    }

    /// 单 preview (2026-08-12): mark a delegation-notice turn as finished.
    /// Saturating — a notice that reached `process_turn` without a matching
    /// dispatch-time bump (e.g. a direct test call) must not underflow.
    pub fn finish_notice_turn(&self) {
        let _ = self
            .notice_turns_in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// 单 preview (2026-08-12): true while any delegation-notice turn is
    /// queued or running (see `notice_turns_in_flight` doc).
    pub fn has_notice_turns_in_flight(&self) -> bool {
        self.notice_turns_in_flight.load(Ordering::SeqCst) > 0
            || !self
                .delegation_notice_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    /// P1 (2026-08-13, RFC delegation-notice-queue §4): enqueue a delegation
    /// completion notice for later drain. Called from `route_notice` while
    /// `turn_lock` is busy (or as the first step of an immediate drain).
    pub fn enqueue_delegation_notice(&self, notice: DelegationNotice) {
        self.delegation_notice_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(notice);
    }

    /// P1 (2026-08-13, RFC delegation-notice-queue §4): take the whole queue
    /// (FIFO order) for one drain pass. Notices enqueued DURING the drain
    /// stay for the next pass — a single drain is bounded.
    pub fn take_delegation_notices(&self) -> Vec<DelegationNotice> {
        std::mem::take(
            &mut *self
                .delegation_notice_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
        .into_iter()
        .collect()
    }

    /// issue #238: enqueue an event for whenever this session next has a
    /// pending `sessions_yield` to deliver it to. Safe to call regardless of
    /// whether a yield is currently outstanding — `try_fill_pending_yield`
    /// is what actually consumes the queue.
    pub fn enqueue_pending_yield_event(&self, event: PendingYieldEvent) {
        self.pending_yield_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(event);
        self.persist_pending_yield_events();
    }

    /// issue #238: take the whole queue (FIFO order) for one fill. Events
    /// enqueued DURING the fill stay for the next one. issue #240: the
    /// queue is empty on disk either way once this returns (whether the
    /// caller consumes the events or restores `pending_yield` untouched),
    /// so persisting unconditionally here keeps the file in sync.
    pub fn take_pending_yield_events(&self) -> Vec<PendingYieldEvent> {
        let taken: Vec<PendingYieldEvent> = std::mem::take(
            &mut *self
                .pending_yield_events
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
        .into_iter()
        .collect();
        self.persist_pending_yield_events();
        taken
    }

    /// issue #238: attempt to fill this session's pending `sessions_yield`
    /// tool_call with whatever has accumulated in the event queue, and if
    /// it does, spawn the resume turn. Safe to call from anywhere — both
    /// directions (an event is enqueued while a yield may already be
    /// pending; a yield appears — explicit or implicit — while events may
    /// already be queued) funnel through here, and taking `pending_yield`
    /// under the session lock means only one caller can ever actually fill
    /// it.
    ///
    /// No-op when there's no pending yield yet (nothing to fill), or when
    /// the queue is empty (nothing to fill it WITH) — in the latter case
    /// `pending_yield` is put back so a later call can still find it.
    ///
    /// Phase 2b (issue #256): when a `sessions_yield` is currently live-
    /// parked in `park_for_yield` (a waiter is registered matching this
    /// tool_call_id), delivery goes through the waiter instead — signal it
    /// directly and let the already-running task write the result itself
    /// (single writer), rather than writing to history here and spawning a
    /// fresh task. `pending_yield` is put back in that case too: the
    /// waking task re-validates and clears it itself.
    pub async fn try_fill_pending_yield(self: &Arc<Self>, runtime: AgentRuntime) {
        let should_resume = {
            let mut session = self.session.lock().await;
            let Some(pending) = session.pending_yield.take() else {
                return;
            };
            let has_live_waiter = self
                .yield_waiter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|w| w.tool_call_id == pending.tool_call_id);
            if has_live_waiter {
                let events = self.take_pending_yield_events();
                if events.is_empty() {
                    session.pending_yield = Some(pending);
                    return;
                }
                let waiter = self
                    .yield_waiter
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                session.pending_yield = Some(pending);
                if let Some(waiter) = waiter {
                    let content = format_pending_yield_events(&events);
                    let _ = waiter.tx.send(content);
                } else {
                    // Raced with the waiter being taken/cancelled between
                    // the check above and here — fall back to persisting
                    // the events for whoever looks next (restart-safe
                    // path); nothing was consumed.
                    for event in events {
                        self.enqueue_pending_yield_event(event);
                    }
                }
                return;
            }
            let events = self.take_pending_yield_events();
            if events.is_empty() {
                session.pending_yield = Some(pending);
                return;
            }
            let content = format_pending_yield_events(&events);
            session.add_tool_result(pending.tool_call_id, "sessions_yield", content, false);
            session.persist_last();
            true
        };
        if should_resume {
            spawn_resume_after_yield(Arc::clone(self), runtime);
        }
    }

    /// Phase 2b (issue #256): park in place awaiting the real event for the
    /// `sessions_yield` tool_call named by `session.pending_yield` — called
    /// from `run_and_deliver` immediately after `Agent::run` returns with a
    /// pending yield outstanding. Takes both guards by value so it can drop
    /// them for the actual wait and reacquire fresh ones once (or if) an
    /// event arrives; returns them back to the caller either way.
    ///
    /// Two outcomes, distinguished by the returned `bool`:
    /// - `true` — the tool_call's result was written directly to history
    ///   (either immediately, from an already-queued event, or after
    ///   waking from the park) and `pending_yield` cleared. The caller
    ///   should loop back into `Agent::run` to continue the turn, exactly
    ///   like `resume_after_yield`'s own turns.
    /// - `false` — nothing more to do here: either the queue was empty and
    ///   the wait was cancelled (a supersede/interjection resolved this
    ///   tool_call from a different task while we were parked), or the
    ///   wait was cancelled outright. The caller should return its current
    ///   `TurnResult` as-is without re-delivering anything.
    async fn park_for_yield(
        self: &Arc<Self>,
        mut session: OwnedMutexGuard<Session>,
        mut turn_guard: OwnedMutexGuard<()>,
        tool_call_id: String,
    ) -> (OwnedMutexGuard<Session>, OwnedMutexGuard<()>, bool) {
        // Fast path: something may already be queued (issue #238's original
        // race — the event arrived before the yield did). No park needed.
        let queued = self.take_pending_yield_events();
        if !queued.is_empty() {
            let content = format_pending_yield_events(&queued);
            session.pending_yield = None;
            session.add_tool_result(tool_call_id, "sessions_yield", content, false);
            session.persist_last();
            return (session, turn_guard, true);
        }

        // Register the waiter BEFORE dropping either lock — atomic with
        // `pending_yield` already being set (by our caller, under the same
        // continuous hold), so there is no window where a notice can find
        // pending_yield set but no waiter to deliver to.
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.yield_waiter.lock().unwrap_or_else(|e| e.into_inner()) = Some(YieldWaiter {
            tool_call_id: tool_call_id.clone(),
            tx,
        });

        drop(turn_guard);
        drop(session);

        let outcome = rx.await;

        turn_guard = Arc::clone(&self.turn_lock).lock_owned().await;
        session = Arc::clone(&self.session).lock_owned().await;

        let content = match outcome {
            Ok(content) => content,
            // Sender dropped without sending: cancelled — a new waiter
            // (supersede) overwrote our slot, or #240/interjection closed
            // this tool_call out from a different task while we waited.
            Err(_) => return (session, turn_guard, false),
        };

        // Re-validate: still ours? A cancellation can race the send itself
        // (both are possible right up until we reacquire the lock), so
        // check identity again rather than trusting `Ok` alone.
        let still_ours = session
            .pending_yield
            .as_ref()
            .is_some_and(|p| p.tool_call_id == tool_call_id);
        if !still_ours {
            // The content was already successfully handed to us (`Ok`
            // above) before a supersede (a new sessions_yield, or a #248
            // interjection close) cleared `pending_yield` out from under
            // this tool_call — the waiter slot was already empty by then,
            // so nothing else knows about this content. Re-queue it rather
            // than silently dropping it: whatever superseded us (or its
            // own next sessions_yield) will pick it up via the same
            // fast-path queue check this function starts with.
            self.enqueue_pending_yield_event(PendingYieldEvent { content });
            return (session, turn_guard, false);
        }
        session.pending_yield = None;
        session.add_tool_result(tool_call_id, "sessions_yield", content, false);
        session.persist_last();
        (session, turn_guard, true)
    }

    /// P1 (2026-08-13, RFC delegation-notice-queue §4): true while any
    /// delegation notice waits for a drain. Feeds the dispatch_turn tail
    /// (turn-end drain trigger) and the wake idle check.
    pub fn has_queued_delegation_notices(&self) -> bool {
        !self
            .delegation_notice_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// 方案 C: snapshot of the suspension state (P0-2 consumes terminal
    /// events against it; P1-1 persists it).
    pub fn suspension_snapshot(&self) -> Option<TurnSuspension> {
        self.turn_suspension
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 方案 C: accumulate a suppressed progress report (RFC §2.3 — never
    /// injected into the parent context, suspended or not). No-op when the
    /// session is not suspended.
    pub fn add_progress(&self, sub_session_id: &str, text: &str) {
        {
            let mut guard = self
                .turn_suspension
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(s) = guard.as_mut() {
                s.progress_by_sub_session
                    .entry(sub_session_id.to_string())
                    .or_default()
                    .push(text.to_string());
            }
        }
        self.persist_suspension();
    }

    /// 方案 C: collect a terminal event into the suspension — move the task
    /// out of `pending`, fold its suppressed progress reports into the new
    /// `SubResult`, append to `results` in completion order. Returns
    /// [`TerminalRecord::Recorded`] on first collection (callers send the
    /// notice), [`TerminalRecord::Duplicate`] on an idempotent hit (skip the
    /// notice), or [`TerminalRecord::NoSuspension`] when no suspension is
    /// active. `source` tags the caller (issue #224 probe telemetry) so the
    /// duplicate-hit warning can attribute which path attempted the re-route.
    pub fn record_terminal(
        &self,
        sub_session_id: String,
        status: SubStatus,
        content: String,
        sent_message_count: u64,
        source: &'static str,
    ) -> TerminalRecord {
        // issue #252: an id currently sitting in `pending` is a legitimate,
        // live task (freshly delegated, or re-armed by `agent_resume` —
        // issue #251) — its terminal event must always be allowed to
        // migrate pending→results, regardless of whether an EARLIER,
        // different terminal for the same id (e.g. the original timeout
        // that #251's resume is recovering from) fell inside the same
        // stale-repeat-call window below. Skipping that window check here
        // is what lets the resume's own completion/cancellation actually
        // clear `pending` instead of leaving a ghost entry that
        // `render_background_work_reminder` reports forever.
        let currently_pending = {
            let guard = self
                .turn_suspension
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .is_some_and(|s| s.pending.iter().any(|t| t == &sub_session_id))
        };

        if !currently_pending {
            // issue #224: this bounded, suspension-independent check runs
            // because the check below (inside the `Some(s)` branch) only
            // protects while `turn_suspension` still exists — as soon as
            // every pending task is collected, `clear_suspension_if_collected`
            // drops it back to `None`, and a call for an id already recorded
            // minutes (or seconds) earlier would otherwise hit the
            // `guard.as_mut() == None` branch below and return
            // `NoSuspension` — which `route_shell_completion` deliberately
            // treats as "route it anyway" (#129: every notify-armed process
            // gets a notice), turning a repeat call into a repeat delivery.
            // Pruned lazily to the window.
            let mut recent = self
                .recently_recorded_terminals
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            recent.retain(|(_, recorded_at)| {
                now.duration_since(*recorded_at) <= RECENTLY_RECORDED_TERMINAL_WINDOW
            });
            if recent.iter().any(|(id, _)| id == &sub_session_id) {
                // issue #224 probe: log (warn-level) when the bounded dedup
                // actually fires in a running process, to observe whether
                // duplicate routing (re-routes) really occurs in production.
                tracing::warn!(
                    sub_session_id = %sub_session_id,
                    recent_count = recent.len(),
                    source,
                    "record_terminal: duplicate hit on recently_recorded_terminals (possible re-route, issue #224 probe)"
                );
                return TerminalRecord::Duplicate;
            }
        }

        let snapshot = {
            let mut guard = self
                .turn_suspension
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let s = match guard.as_mut() {
                Some(s) => s,
                None => return TerminalRecord::NoSuspension,
            };
            // Idempotent: if the sub-session was already collected (not in
            // pending and already in results), return the current
            // snapshot without adding a duplicate result entry. This
            // guards against double-delivery when both recover_suspension
            // and the sub-agent recovery loop handle the same task.
            if !s.pending.iter().any(|t| t == &sub_session_id)
                && s.results.iter().any(|r| r.sub_session_id == sub_session_id)
            {
                return TerminalRecord::Duplicate;
            }
            s.pending.retain(|t| t != &sub_session_id);
            let progress = s
                .progress_by_sub_session
                .remove(&sub_session_id)
                .unwrap_or_default();
            s.results.push(SubResult {
                sub_session_id: sub_session_id.clone(),
                status,
                content,
                sent_message_count,
                progress,
            });
            guard
                .clone()
                .expect("record_terminal: suspension exists after NoSuspension guard")
        };
        // Persist after the guard drops — persist_suspension re-locks the
        // same std Mutex (not reentrant).
        self.persist_suspension();
        self.recently_recorded_terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((sub_session_id, std::time::Instant::now()));
        TerminalRecord::Recorded(Box::new(snapshot))
    }

    /// 方案 C (RFC §3.4): clear the suspension once every pending task has
    /// been collected (no-op when there is no suspension or tasks remain).
    /// Called after each turn's `has_pending` computation — a turn whose
    /// run re-delegated keeps its (repopulated) suspension; the final
    /// resume turn ends with `pending` empty and drops the state so the
    /// next turn is loud again.
    pub fn clear_suspension_if_collected(&self) {
        {
            let mut guard = self
                .turn_suspension
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(s) = guard.as_ref() {
                // 单 preview (2026-08-12): `pending` empty alone is NOT the end
                // of the suspension sequence — the wake burst may have collected
                // every terminal while delegation-notice turns are still queued
                // behind `turn_lock` (counted in `notice_turns_in_flight`).
                // Clearing here would drop the cross-turn preview and let each
                // queued notice open its own message (multi-message spam bug).
                // P1 (2026-08-13): `has_notice_turns_in_flight` now also covers
                // the enqueued-but-undrained queue — clear only when no notice
                // turns remain AND nothing waits for a drain.
                if s.pending.is_empty() && !self.has_notice_turns_in_flight() {
                    *guard = None;
                }
            }
        }
        self.persist_suspension();
    }

    /// 单 preview (2026-08-12): persist the streaming preview identity for
    /// cross-turn takeover — the origin turn and each silenced resume turn
    /// write the preview message id + current body into the suspension; the
    /// next delegation-notice turn folds it (edit-in-place append, 保留历史
    /// 行追加). No-op when the session is not suspended.
    pub fn set_preview(&self, reply_target: String, fold: crate::api::turn_stream::FoldCandidate) {
        {
            let mut guard = self
                .turn_suspension
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(s) = guard.as_mut() {
                s.preview = Some(PreviewState {
                    reply_target,
                    msg_id: fold.msg_id,
                    text: fold.text,
                    // 单 preview (2026-08-12): cumulative counters + wall-clock
                    // start ride along so the FINAL summary line reflects the
                    // WHOLE message ("summary 没有累计", user-confirmed).
                    thinking_steps: fold.thinking_steps,
                    tool_count: fold.tool_count,
                    commentary_notes: fold.commentary_notes,
                    started_at_unix_secs: fold.started_at_unix_secs,
                });
            }
        }
        self.persist_suspension();
    }

    /// Run one turn end-to-end: acquire the turn lock, replay the
    /// inbound message, resolve `TurnContext` from the session override,
    /// invoke `Agent.run`, and return the result. The caller is
    /// responsible for dispatching the response — user-message paths
    /// send via `channel.send`; scheduled paths route through the
    /// scheduler's `send_to_target_internal`.
    ///
    /// RFC channel-role-split §1.3: `channel` is a **pure delivery
    /// handle** for this turn engine's own output — streaming fold,
    /// fallback send, TTS, cancel/error notices. It does NOT mark
    /// interactivity: the "is a human present" marker travels on
    /// `inbound_msg.run_mode` (→ `session.turn_headless`), and
    /// per-turn tools resolve their channel via
    /// `Session::resolve_channel()` (registry lookup), not from this
    /// parameter. Callers: user/wake/recovery turns pass `Some`,
    /// scheduled turns pass `None`.
    ///
    /// On `Ok(TurnResult)`, the caller may inspect `text` /
    /// `pending_retry`. The latter is also stashed on
    /// `SessionContext.pending_retry` here so the next inbound for the
    /// same session can offer a retry prompt.
    pub async fn process_turn(
        self: &Arc<Self>,
        inbound_msg: ChannelInboundMessage,
        channel: Option<Arc<dyn Channel>>,
        runtime: AgentRuntime,
    ) -> anyhow::Result<TurnResult> {
        let turn_guard = Arc::clone(&self.turn_lock).lock_owned().await;
        let mut session = Arc::clone(&self.session).lock_owned().await;

        let content = crate::str_utils::neutralize_spoofing(&inbound_msg.content.text);
        let reply_target = inbound_msg.receiver.id.clone();
        // 方案 C (§3.3, race fix): delegation notices carry the wake-time
        // silence intent; capture before `inbound_msg` is moved. User-message
        // turns leave it None → live snapshot decides below.
        let silenced_intent = inbound_msg.silenced_override;
        // RFC channel-role-split §1.1: capture the turn-scoped
        // interactivity marker BEFORE `inbound_msg` is moved. User /
        // recovery / delegation-wake messages default to Interactive;
        // cron/heartbeat synthesized messages carry Background.
        let turn_run_mode = inbound_msg.run_mode;
        // 单 preview (2026-08-12): RAII guard — a delegation-notice turn
        // (silenced_intent = Some) decrements `notice_turns_in_flight` on
        // EVERY exit path (normal / early return / error). `finish()` is
        // called explicitly on the main path BEFORE
        // `clear_suspension_if_collected` so the end-of-sequence check no
        // longer counts the current turn; `Drop` covers the rest.
        let notice_guard = NoticeTurnGuard::new(self, silenced_intent.is_some());

        // Persist inbound files to session-local storage so their lifetime
        // matches the session.  Read the body stream (via
        // ChannelFileBody::open) and write it under <session_dir>/files/.
        // Falls back to the adapter's temp-file path_hint when the backend
        // cannot persist (e.g. MemoryBackend in tests).
        let session_id = session.id.clone();
        let media_parts: Vec<crate::providers::ContentPart> = {
            use crate::providers::ContentPart;
            let mut parts = Vec::new();
            for file in &inbound_msg.content.files {
                let path = match &session.persist {
                    Some(hook) => {
                        let mut reader = file.body.open().await?;
                        let mut data = Vec::new();
                        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut data).await?;
                        match hook.save_file(
                            &session_id,
                            Some(&file.meta.file_name),
                            &data,
                            file.meta.mime_type.as_deref(),
                        ) {
                            Some(saved) => saved.path,
                            None => match file.body.path_hint() {
                                Some(hint) => hint.to_string(),
                                None => {
                                    tracing::warn!(
                                        file = %file.meta.file_name,
                                        "inbound file persist failed and no path_hint; skipping"
                                    );
                                    continue;
                                }
                            },
                        }
                    }
                    None => match file.body.path_hint() {
                        Some(hint) => hint.to_string(),
                        None => {
                            tracing::warn!(
                                file = %file.meta.file_name,
                                "no persist hook and no path_hint; skipping"
                            );
                            continue;
                        }
                    },
                };
                parts.push(ContentPart::File {
                    path,
                    mime_type: file.meta.mime_type.clone(),
                    name: Some(file.meta.file_name.clone()),
                    size_bytes: file.meta.size_bytes,
                });
            }
            parts
        };

        // Session.persist was wired at SessionContext creation by
        // SessionManager; capture a clone so the post-turn `add_user`
        // persistence call sees the same hook.
        let persist_hook = session.persist.clone();
        // Snapshot before clearing — bare-"继续" soft-block uses this.
        let was_incomplete_turn = session.incomplete_turn;
        // A new turn owns the incomplete-turn state from here on.
        session.incomplete_turn = false;
        // Record inbound and persist last_message safely under turn_lock.
        session.record_inbound(inbound_msg.clone());
        if let Some(hook) = &persist_hook {
            if let Some(ref msg) = session.last_message {
                hook.save_last_message(&session.id, msg);
            }
        }
        let channel_for_send = channel.clone();
        // 方案 C (RFC §3.3): silent iff the session is suspended with pending
        // tasks — for delegation notices the intent is captured at wake time
        // (`silenced_intent`, see `decide_silenced`; a queued notice may run
        // after later terminals cleared `pending`), for user messages from
        // the live snapshot at turn start. Origin turns (suspension created
        // mid-run) and the final resume turn (last terminal already recorded
        // → pending empty) are loud; intermediate resume turns are silenced:
        // their model output is streamed as commentary (💬) and, on suspended
        // turns, NOT re-delivered as a standalone message (RFC §3.3 七次修正
        // — no tool-trailing final answer exists, so fallback would duplicate
        // the commentary; non-streaming channels still get the fallback send).
        // The turn does NOT end. Also disables `ask_user` for this turn.
        let silenced = decide_silenced(silenced_intent, self.suspension_snapshot());
        session.turn_silenced = silenced;
        // RFC channel-role-split §1.1: the headless marker is turn-scoped,
        // derived from the inbound message (never from a session-level
        // override — Inject crons must not poison the user's session).
        // Cleared at turn end like `turn_silenced`.
        session.turn_headless = matches!(turn_run_mode, crate::config::agent::RunMode::Background);
        // RFC §7.6: install per-turn streaming handle BEFORE Agent::run.
        // Channels that don't support streaming return None; the
        // fallback send block below covers them (delivery == Pending always
        // delivers, even on suspended turns). Silenced (intermediate resume)
        // turns stream exactly like ordinary turns — the model's output is
        // shown as commentary and the turn does NOT end; later terminal
        // events resume it until the final loud summary.
        // 单 preview (2026-08-12): the whole async-delegation flow is ONE
        // evolving message. A delegation-notice turn (silenced_intent = Some)
        // TAKES OVER the suspension's live preview message (fold → edit the
        // same message, append lines, 保留历史行追加); origin/user turns start
        // fresh. Silenced resume turns additionally defer collapse so their
        // intermediate output appends as 💬 lines; the final resume turn
        // collapses the preview into a summary (boundary ②, user-confirmed
        // 2026-08-12).
        let fold = if silenced_intent.is_some() {
            self.suspension_snapshot().and_then(|s| s.preview).map(|p| {
                crate::api::turn_stream::FoldCandidate {
                    msg_id: p.msg_id,
                    text: p.text,
                    // 单 preview (2026-08-12): cumulative counters + wall-clock
                    // start ride along so the FINAL summary line reflects the
                    // WHOLE message ("summary 没有累计", user-confirmed).
                    thinking_steps: p.thinking_steps,
                    tool_count: p.tool_count,
                    commentary_notes: p.commentary_notes,
                    started_at_unix_secs: p.started_at_unix_secs,
                }
            })
        } else {
            None
        };
        // 单 preview (2026-08-12): the FINAL loud notice turn (fold takeover,
        // NOT silenced) takes over the origin's preview and collapses it into
        // the one-line summary on `Done` (`final_takeover`); the final answer
        // is then delivered by the `send_message` fallback as a SEPARATE
        // message — user-confirmed shape: 2 messages (summary + answer).
        // Ordinary turns and silenced resume turns are unaffected.
        let fold_takeover = fold.is_some();
        session.turn_stream = channel
            .as_ref()
            .and_then(|ch| ch.create_stream_folding(&reply_target, fold));
        if silenced {
            if let Some(stream) = session.turn_stream.as_mut() {
                stream.defer_collapse();
            }
        }
        if fold_takeover && !silenced {
            if let Some(stream) = session.turn_stream.as_mut() {
                stream.final_takeover();
            }
        }

        // Create a fresh cancellation token for this turn. A clone goes to
        // SessionContext (for `/stop`) and another to the session (for
        // Agent::run to check at the top of each loop iteration).
        let cancel_token = tokio_util::sync::CancellationToken::new();
        *self.turn_cancel.lock().unwrap() = cancel_token.clone();
        session.cancel_token = Some(cancel_token);

        let session_override = session.session_override.clone();
        let mut prompt_config = runtime.defaults.prompt.clone();
        if let Some(pm) = session_override.permission_mode {
            prompt_config.permission_mode = pm;
        }
        // RFC channel-role-split §1.1: run_mode is turn-scoped and travels
        // on the inbound message (Interactive for user/wake/recovery turns,
        // Background for cron/heartbeat). No longer read from
        // session_override — Inject crons must not poison the user's
        // subsequent interactive turns.
        prompt_config.run_mode = turn_run_mode;

        // Sub-agent delegations set `system_prompt_override` to bypass
        // the full SystemPromptBuilder (sub-agents want a minimal
        // AGENT.md-derived identity prompt, not the full behavioral
        // ruleset).
        let system_prompt = match &session_override.system_prompt_override {
            Some(custom) => custom.clone(),
            None => runtime.build_system_prompt(&prompt_config),
        };

        // issue #248: a plain-text user interjection while a `sessions_yield`
        // is still pending closes that yield's tool_result instead of
        // wrapping the interjection in a new `[user]` message — the natural
        // protocol position for "the wait was interrupted", and it sidesteps
        // the tool_call → user adjacency problem `session_override.rs`'s
        // placeholder patches. `add_tool_result` is text-only, so a message
        // carrying media falls through to the ordinary path below unchanged
        // (known simplification, same spirit as `resume_after_yield`'s own
        // disclosed gaps).
        if media_parts.is_empty() {
            if let Some(pending) = session.pending_yield.take() {
                // Phase 2b (issue #256): if a live park was still waiting on
                // this exact tool_call, cancel it now rather than leaving it
                // to be silently overwritten (and only then dropped) by
                // some later waiter registration — dropping the sender here
                // resolves the parked task's `rx.await` to `Err` right
                // away, so it gives up promptly instead of leaking until
                // this session's next yield.
                {
                    let mut waiter_guard =
                        self.yield_waiter.lock().unwrap_or_else(|e| e.into_inner());
                    if waiter_guard
                        .as_ref()
                        .is_some_and(|w| w.tool_call_id == pending.tool_call_id)
                    {
                        *waiter_guard = None;
                    }
                }

                let closing_content =
                    format!("The user interrupted the wait and said: {}", content);
                session.add_tool_result(
                    pending.tool_call_id,
                    "sessions_yield",
                    closing_content,
                    false,
                );
                session.persist_last();

                let thinking = session_override.to_thinking_config();
                let model_id = session_override.model.as_deref();
                let turn_ctx = TurnContext {
                    system_prompt: &system_prompt,
                    model_id,
                    thinking: thinking.as_ref(),
                    permission_mode: prompt_config.permission_mode,
                    run_mode: prompt_config.run_mode,
                };

                return self
                    .run_and_deliver(
                        session,
                        turn_guard,
                        turn_ctx,
                        &runtime,
                        channel_for_send,
                        reply_target,
                        silenced,
                        notice_guard,
                        persist_hook,
                    )
                    .await;
            }
        }

        // RFC §三.A line 312-323: process_turn computes the attachment
        // delta (skills/agents/MCP/memory/date/autonomy) against the
        // history's announced state and prepends a <system-reminder> to
        // the user content before recording the turn.
        let reminder = {
            let skills_snap = runtime.skills.read();
            // Clone history to avoid borrow conflict with attachments.
            let history_clone = session.history.clone();
            // RFC #101 P1: pass the FQID (users/{uuid}), not the routing key.
            let owner = session.owner_fqid.clone();
            session
                .attachments
                .diff_skills(&skills_snap, &history_clone, Some(owner.as_str()));
            let agent_list: Vec<(String, String)> = runtime
                .agents
                .values_cloned()
                .into_iter()
                .map(|a| {
                    (
                        a.config.name.clone(),
                        a.config.description.clone().unwrap_or_default(),
                    )
                })
                .collect();
            session.attachments.diff_agents(&agent_list, &history_clone);
            // Date injection respects the configured [prompt] timezone_offset
            // (sourced from the shared ResourceProvider via CompactionEngine).
            session
                .attachments
                .diff_date(runtime.context_engine.timezone_offset(), &history_clone);
            session
                .attachments
                .diff_autonomy(&prompt_config.permission_mode, &history_clone);
            // Daily throttled draft-skill backlog reminder (issue #89,
            // layer ②) — best-effort, never blocks the turn.
            // #101 P2: per-layer accounting — the user-layer backlog
            // belongs to this session's owner; the agent-layer backlog
            // surfaces only in the operator's sessions
            // (runtime.defaults.operator = [system] operator
            // normalized to a bare uuid at daemon assembly).
            let is_operator = !session.owner_fqid.is_empty()
                && runtime
                    .defaults
                    .operator
                    .as_deref()
                    .is_some_and(|op| crate::ids::bare_dir_name(&session.owner_fqid) == op);
            if let Some(backlog) = crate::agents::skill_draft_reminder::check_and_arm(
                std::path::Path::new(&runtime.defaults.prompt.base_dir),
                runtime.context_engine.timezone_offset(),
                &session.owner_fqid,
                is_operator,
            ) {
                session
                    .attachments
                    .push_skill_draft_reminder(backlog.user_layer, backlog.agent_layer);
            }
            // Inject user/feedback memory index as system-reminder.
            // P4 (RFC #101 §6): merged view — the agent layer plus THIS
            // owner's user layer. The previous full single-pool scan let any
            // user's inject=always entries leak into every conversation.
            let memory_root = &runtime.defaults.prompt.memory_root;
            if !memory_root.is_empty() {
                let memory_dir = std::path::Path::new(memory_root);
                match crate::memory::scan_merged_for_user(memory_dir, &session.owner_fqid) {
                    Ok(files) => {
                        let memory_entries: Vec<crate::memory::IndexEntry> = files
                            .iter()
                            .map(crate::memory::IndexEntry::from)
                            .collect();
                        session
                            .attachments
                            .diff_memory(&memory_entries, &history_clone);
                    }
                    // Never fake an empty layer — log and skip this
                    // turn's injection entirely.
                    Err(e) => tracing::error!(
                        error = %e,
                        "memory layer scan failed; skipping memory index injection this turn"
                    ),
                }
            }
            let text = session.attachments.build_text(&skills_snap);
            session.attachments.clear_pending();

            text
        };

        // P3: bare "继续"/continue with no media and no clear open work →
        // soft guidance only (do not call the model). Incomplete-turn /
        // pending_retry / trailing incomplete history still fall through.
        if media_parts.is_empty() && is_bare_continue(&content) {
            let has_open = was_incomplete_turn
                || self.pending_retry.lock().await.is_some()
                || history_looks_incomplete(&session.history);
            if !has_open {
                let msg = crate::agents::user_messages::MSG_BARE_CONTINUE.to_string();
                // Tear down stream without running the agent.
                let turn_stream = session.turn_stream.take();
                session.turn_headless = false;
                if let Some(s) = turn_stream {
                    s.abort().await;
                }
                if let Some(ch) = channel_for_send {
                    let receiver = {
                        let mut r = crate::api::message::MessageReceiver::new(reply_target.clone());
                        if let Some(ref last_msg) = session.last_message {
                            r.reply_to_message_id = Some(
                                last_msg
                                    .receiver
                                    .reply_to_message_id
                                    .clone()
                                    .unwrap_or_else(|| last_msg.id.clone()),
                            );
                            r.thread_id = last_msg.receiver.thread_id.clone();
                        }
                        r
                    };
                    let message = crate::api::message::ChannelOutboundMessage {
                        receiver,
                        content: crate::api::message::ChannelMessageContent::text(msg.clone()),
                        options: Default::default(),
                    };
                    let _ = ch.send_message(&message).await;
                }
                return Ok(TurnResult {
                    text: msg,
                    stop_reason: crate::providers::StopReason::EndTurn,
                    pending_retry: None,
                    has_pending: false,
                });
            }
        }

        // P3: pure-image (or media-only) turns get an explicit model-side hint
        // so the agent does not invent tasks from compaction noise.
        let content_for_model = if !media_parts.is_empty() && content.trim().is_empty() {
            crate::agents::user_messages::MSG_IMAGE_ONLY_HINT.to_string()
        } else {
            content
        };

        let user_content = match reminder {
            Some(rem) => format!("{}\n\n{}", rem, content_for_model),
            None => content_for_model,
        };
        if media_parts.is_empty() {
            session.add_user(user_content);
        } else {
            session.add_user_with_media(user_content, media_parts);
        }
        if let Some(ref hook) = persist_hook {
            if let Some(last) = session.history.last().cloned() {
                if let Some(id) = hook.persist_message(&session.id, &last) {
                    if let Some(slot) = session.message_ids.last_mut() {
                        *slot = id;
                    }
                }
            }
        }

        let thinking = session_override.to_thinking_config();
        let model_id = session_override.model.as_deref();
        let turn_ctx = TurnContext {
            system_prompt: &system_prompt,
            model_id,
            thinking: thinking.as_ref(),
            permission_mode: prompt_config.permission_mode,
            run_mode: prompt_config.run_mode,
        };

        self.run_and_deliver(
            session,
            turn_guard,
            turn_ctx,
            &runtime,
            channel_for_send,
            reply_target,
            silenced,
            notice_guard,
            persist_hook,
        )
        .await
    }

    /// issue #238: extracted from `process_turn` — everything from here on is
    /// generic "run the LLM/tool loop and deliver the result" logic that
    /// doesn't care whether the turn was triggered by a new inbound message or
    /// a resumed `sessions_yield`. `resume_after_yield` (added alongside the
    /// rest of #238) is the second caller: it skips straight to this after
    /// appending the filled tool_result directly to history, with
    /// `silenced=false` and a no-op `NoticeTurnGuard` — a yield-resume is now
    /// an ordinary turn, not part of the old suspension/silenced bookkeeping.
    #[allow(clippy::too_many_arguments)]
    async fn run_and_deliver(
        self: &Arc<Self>,
        mut session: OwnedMutexGuard<Session>,
        // Phase 2a (issue #256): owned (not borrowed) so this frame — the
        // only one that will ever need to — can `drop` it and reacquire a
        // fresh one around the Phase 2b sessions_yield park. `&mut
        // OwnedMutexGuard<_>` cannot do that (nothing to move it out into;
        // `OwnedMutexGuard` has no `Default` for `mem::take`, and
        // `try_lock_owned` would deadlock against the guard's own hold).
        mut turn_guard: OwnedMutexGuard<()>,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
        channel_for_send: Option<Arc<dyn Channel>>,
        reply_target: String,
        silenced: bool,
        mut notice_guard: NoticeTurnGuard,
        persist_hook: Option<Arc<dyn PersistHook>>,
    ) -> anyhow::Result<TurnResult> {
        // Phase 2b (issue #256): `silenced`/`notice_guard` apply only to
        // the FIRST `Agent::run` call — the caller's own turn semantics. A
        // loop iteration reached by parking-then-waking is always an
        // ordinary (non-silenced) continuation, exactly like
        // `resume_after_yield`'s own turns always are.
        let mut silenced = silenced;
        loop {
            // Notify channel that processing has started (typing indicator,
            // etc.) — suppressed on silenced resume turns (RFC §3.3).
            if !silenced {
                if let Some(ref ch) = channel_for_send {
                    ch.on_status(&reply_target, crate::ProcessingStatus::Thinking)
                        .await;
                }
            }

            let result = self.agent.run(&mut session, turn_ctx.clone(), runtime).await;
            // Per-turn turn_stream is transient. Consume the stream first
            // (RFC §7.6): finish on success delivers FinalDelivered; abort on
            // error cancels the WS transport.
            let turn_stream = session.turn_stream.take();
            // RFC channel-role-split §1.1: the headless marker is turn-scoped —
            // always cleared at turn end so the next turn starts Interactive
            // unless its own message says otherwise.
            session.turn_headless = false;
            session.cancel_token = None;

            match (result, turn_stream) {
            (Ok(mut turn_result), stream) => {
                // 单 preview (2026-08-12): `has_pending` = "this turn belongs
                // to the suspension sequence" = origin turn (Agent::run set it
                // via `async_delegation_spawned`) || resume turn (silenced).
                // Command/user turns that run while delegations are pending
                // are NOT part of the sequence (has_pending=false → normal
                // delivery). Replaces the old unconditional overwrite with
                // `has_pending_async_work()`, which wrongly marked such
                // turns suspended and gated their delivery.
                turn_result.has_pending = turn_result.has_pending || silenced;
                // 单 preview (2026-08-12): this notice turn is now finished —
                // decrement BEFORE the end-of-sequence check so the final
                // resume turn can clear its own suspension (otherwise it would
                // still see its own in-flight count = 1 and never clear).
                notice_guard.finish();
                // 方案 C (RFC §3.4): pending 归零 → 最终轮,挂起状态消费完毕,
                // 清除之(下一轮恢复响亮)。级联轮(本 turn 再次派发)pending
                // 非空 → 保留,继续挂起。
                self.clear_suspension_if_collected();
                // 方案 C (fix v2): a silenced resume turn whose model output
                // ended with EndTurn is semantically converted to Continue —
                // pending delegations remain, the turn must NOT end (only the
                // final loud summary does). Also patch the persisted history
                // entry: `llm_usage` writes `format!("{:?}", stop_reason)` so
                // history.jsonl would show "EndTurn" even though the turn
                // continues — rewrite the last assistant message's
                // usage.stop_reason to "Continue" so the observable record
                // matches the semantics.
                let semantic = semantic_stop_reason(
                    silenced,
                    turn_result.has_pending,
                    turn_result.stop_reason,
                );
                if semantic != turn_result.stop_reason {
                    turn_result.stop_reason = semantic;
                    // Take the backend ID before borrowing `history` mutably.
                    let persisted_id = session.message_ids.last().copied().unwrap_or(0);
                    let session_id = session.id.clone();
                    if let Some(last) = session.history.last_mut() {
                        if last.role == "assistant" {
                            if let Some(usage) = last.usage.as_mut() {
                                usage.stop_reason = Some("Continue".to_string());
                            }
                            // fix v2.1: rewrite the persisted history row too
                            // — previously only the in-memory entry was patched,
                            // so history.jsonl kept showing "EndTurn" even though
                            // the turn continued (observable record mismatch).
                            if persisted_id > 0 {
                                if let Some(hook) = persist_hook.as_deref() {
                                    hook.update_message(&session_id, persisted_id, last);
                                }
                            }
                        }
                    }
                    tracing::info!(
                        session = %session.id,
                        "silenced resume turn mapped EndTurn → Continue (pending delegations remain)"
                    );
                }
                // Notify channel that processing completed successfully
                // — suppressed on silenced resume turns (RFC §3.3).
                if !silenced {
                    if let Some(ref ch) = channel_for_send {
                        ch.on_status(&reply_target, crate::ProcessingStatus::Done)
                            .await;
                    }
                }
                // ── Media aging ────────────────────────────────────────────
                // After the model has seen the media in this turn, replace
                // inline File parts in history with compact text markers so
                // subsequent turns don't re-upload large payloads (the root
                // cause of 300s API timeouts with big videos).
                //
                // Skip when the turn ended with ContextOverflow — the model
                // never successfully processed the request, so we want the
                // media to remain inline for the retry / compaction path.
                if turn_result.stop_reason != crate::providers::StopReason::ContextOverflow {
                    age_session_media(&mut session, persist_hook.as_deref());
                }

                if let Some(ref retry_msg) = turn_result.pending_retry {
                    *self.pending_retry.lock().await = Some(retry_msg.clone());
                }
                // RFC §7.6: consume the stream first; its `finish()` reports
                // how far the streaming path actually got. Fall back to
                // `send_message` whenever delivery did NOT reach
                // `FinalDelivered` — covers three cases uniformly:
                //   1. No stream at all (non-streaming channel) → Pending
                //   2. Stream existed but transport failed mid-turn → Visible
                //      or Pending (push_or_drop in Agent::run set turn_stream
                //      to None on Err; finish() returns Pending here)
                //   3. Stream existed and acked everything → FinalDelivered
                //      → skip fallback (avoids the double-display bug)
                // Silenced (intermediate resume) turns fall through the same
                // stream-consumption path — their model output is streamed as
                // commentary (progress note, not a turn end).
                //
                // Suspended turns (silenced resume wheels OR the origin turn
                // that spawned async delegations with `has_pending`) differ
                // from ordinary turns: there is no tool-trailing final answer,
                // so `turn_result.text` is just the tool-leading commentary —
                // on streaming channels it was ALREADY shown as a 💬 line in
                // the live preview. Re-sending it as a standalone message
                // duplicates the commentary (double display). Gate the
                // fallback by whether streaming actually displayed the output:
                //   - delivery == Pending (non-streaming channel: qqbot/wechat/
                //     client) → still deliver: that's the only way the user
                //     sees intermediate progress (RFC §3.3 no-stream fallback).
                //   - delivery == Visible (streaming displayed it, e.g.
                //     Telegram) on a suspended turn → skip: commentary-only
                //     output must not be re-sent as a plain text message.
                //   - final wheel (!silenced && !has_pending) → deliver as an
                //     ordinary turn (full summary).
                // 单 preview (2026-08-12): capture the fold candidate BEFORE
                // `finish()` consumes the stream. A suspended turn (origin or
                // intermediate resume) writes the preview identity back into
                // the suspension so the next delegation-notice turn can take
                // over the same message (cross-turn fold); the final resume
                // turn (suspension cleared) has nowhere to write → skip.
                let fold = stream.as_ref().and_then(|s| s.fold_candidate());
                let delivery = match stream {
                    Some(s) => s.finish().await,
                    None => crate::api::turn_stream::StreamDelivery::Pending,
                };
                let suspended_turn = silenced || turn_result.has_pending;
                if suspended_turn {
                    if let Some(f) = fold {
                        self.set_preview(reply_target.clone(), f);
                    }
                }
                if delivery != crate::api::turn_stream::StreamDelivery::FinalDelivered
                    && (delivery == crate::api::turn_stream::StreamDelivery::Pending
                        || !suspended_turn)
                {
                    if let Some(ref ch) = channel_for_send {
                        if !turn_result.text.trim().is_empty() {
                            let receiver = {
                                let mut r =
                                    crate::api::message::MessageReceiver::new(reply_target.clone());
                                if let Some(ref last_msg) = session.last_message {
                                    r.reply_to_message_id = Some(
                                        last_msg
                                            .receiver
                                            .reply_to_message_id
                                            .clone()
                                            .unwrap_or_else(|| last_msg.id.clone()),
                                    );
                                    r.thread_id = last_msg.receiver.thread_id.clone();
                                }
                                r
                            };
                            let message = crate::api::message::ChannelOutboundMessage {
                                receiver,
                                content: crate::api::message::ChannelMessageContent::text(
                                    turn_result.text.clone(),
                                ),
                                options: Default::default(),
                            };
                            match ch.send_message(&message).await {
                                Ok(_res) => {}
                                Err(e) => {
                                    tracing::error!(
                                        session = %session.id,
                                        err = %e,
                                        "process_turn: fallback send failed"
                                    );
                                }
                            }
                        }
                    }
                }
                // ── Auto TTS ──────────────────────────────────────────────
                // If auto_tts is enabled and a TTS provider is available,
                // synthesize the reply text to audio and send as a voice message.
                // Gate: global `[agent] auto_tts` master switch AND the
                // per-account channel `tts` flag (default off).
                if runtime.defaults.auto_tts
                    && channel_for_send.as_ref().is_some_and(|ch| ch.tts_enabled())
                    && !silenced
                    && !turn_result.has_pending
                    && !turn_result.text.trim().is_empty()
                {
                    if let Some(ref ch) = channel_for_send {
                        if let Ok((tts_provider, tts_model)) = runtime.providers.get_tts_provider()
                        {
                            let text_for_tts = prepare_text_for_tts(turn_result.text.trim());
                            // Skip very long texts or texts that became empty after stripping.
                            if !text_for_tts.is_empty() && text_for_tts.chars().count() <= 2000 {
                                let req = crate::providers::TtsRequest {
                                    model: tts_model,
                                    input: text_for_tts.to_string(),
                                    voice: crate::providers::TtsVoice::Id(String::new()),
                                    response_format: None,
                                    speed: None,
                                };
                                match tts_provider.synthesize(req) {
                                    Ok(audio_resp) => {
                                        let temp_path = std::env::temp_dir().join(format!(
                                            "myclaw-tts-{}.mp3",
                                            uuid::Uuid::new_v4()
                                        ));
                                        if std::fs::write(&temp_path, &audio_resp.audio.bytes)
                                            .is_ok()
                                        {
                                            let receiver = {
                                                let mut r =
                                                    crate::api::message::MessageReceiver::new(
                                                        reply_target.clone(),
                                                    );
                                                if let Some(ref last_msg) = session.last_message {
                                                    r.reply_to_message_id = Some(
                                                        last_msg
                                                            .receiver
                                                            .reply_to_message_id
                                                            .clone()
                                                            .unwrap_or_else(|| last_msg.id.clone()),
                                                    );
                                                    r.thread_id =
                                                        last_msg.receiver.thread_id.clone();
                                                }
                                                r
                                            };
                                            let voice_file = crate::api::message::ChannelFile {
                                                meta: crate::api::message::ChannelFileMeta {
                                                    file_name: format!(
                                                        "voice-{}.mp3",
                                                        uuid::Uuid::new_v4()
                                                    ),
                                                    mime_type: Some(
                                                        audio_resp.audio.mime_type.clone(),
                                                    ),
                                                    size_bytes: Some(
                                                        audio_resp.audio.bytes.len() as u64
                                                    ),
                                                    source_url: None,
                                                },
                                                body: std::sync::Arc::new(
                                                    crate::api::message::LocalFileBody::new(
                                                        temp_path.to_string_lossy().to_string(),
                                                    ),
                                                ),
                                            };
                                            let message =
                                                crate::api::message::ChannelOutboundMessage {
                                                    receiver,
                                                    content:
                                                        crate::api::message::ChannelMessageContent {
                                                            text: String::new(),
                                                            files: vec![voice_file],
                                                            buttons: vec![],
                                                        },
                                                    options: Default::default(),
                                                };
                                            if let Err(e) = ch.send_message(&message).await {
                                                tracing::warn!(
                                                    session = %session.id,
                                                    err = %e,
                                                    "auto_tts: voice send failed"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            session = %session.id,
                                            err = %e,
                                            "auto_tts: synthesis failed"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                // issue #238: a turn that leaves async work outstanding
                // (has_pending) but never called `sessions_yield` itself —
                // natural EndTurn, per that tool's own doc comment, is an
                // equally valid way to leave work running — still needs a
                // pending tool_call to hang the eventual result on.
                // Synthesize one so this path and the explicit-yield path
                // converge on the exact same delivery mechanism.
                if turn_result.has_pending && session.pending_yield.is_none() {
                    session.insert_implicit_yield();
                }

                // This turn may have left a pending `sessions_yield` behind
                // (explicit, from tool_phase.rs, or just synthesized above).
                //
                // Phase 2b (issue #256): park right here awaiting the real
                // event — `park_for_yield` handles the fast path (something
                // already queued, issue #238's original race) and the
                // actual wait (drop both guards, await, reacquire)
                // uniformly. `true` means the tool_result is now in
                // history and this loop should continue the SAME turn with
                // a fresh `Agent::run` call, exactly like
                // `resume_after_yield`'s own turns; `false` means either
                // there was nothing pending, or delivery was cancelled
                // (superseded by a different task while parked) — either
                // way, nothing more to do here.
                if session.pending_yield.is_some() {
                    let tool_call_id = session
                        .pending_yield
                        .as_ref()
                        .expect("just checked is_some")
                        .tool_call_id
                        .clone();
                    let (s, tg, filled) =
                        self.park_for_yield(session, turn_guard, tool_call_id).await;
                    session = s;
                    turn_guard = tg;
                    if filled {
                        silenced = false;
                        notice_guard = NoticeTurnGuard::new(self, false);
                        continue;
                    }
                }

                return Ok(turn_result);
            }
            (Err(e), stream) => {
                if let Some(s) = stream {
                    s.abort().await;
                }
                // Notify channel of error status only — the user-facing
                // error notice itself is the caller's job (issue #113: this
                // used to also send a generic MSG_TURN_FAILED text here,
                // duplicating the classified, more specific notice the
                // active-session dispatch path already sends on Err; a
                // caller with a channel but no notice of its own — e.g.
                // spooled-message replay — sends its own via
                // `user_facing_error_message`).
                if let Some(ref ch) = channel_for_send {
                    ch.on_status(&reply_target, crate::ProcessingStatus::Error)
                        .await;
                }
                let channel_name = channel_for_send
                    .as_ref()
                    .map(|ch| ch.name())
                    .unwrap_or("none");
                tracing::error!(
                    session = %session.id,
                    channel = %channel_name,
                    err = %e,
                    "Agent turn failed"
                );
                return Err(e);
            }
        }
        }
    }

    /// issue #238: resume a session after `try_fill_pending_yield` has
    /// already appended the filled tool_result directly to history — there
    /// is no new inbound message here, this is a continuation of the same
    /// still-open tool round, not a new conversational turn. Skips
    /// `process_turn`'s message-specific setup entirely (attachment
    /// diffing, bare-continue detection, `add_user`) and goes straight to
    /// `run_and_deliver` as an ordinary, non-silenced turn — the model
    /// decides for itself whether to respond, call more tools, or yield
    /// again, exactly like any other multi-step tool round.
    ///
    /// Known simplification: unlike `process_turn`, this does not compute
    /// the attachment-diff reminder (new skills/agents/memory/date deltas)
    /// — those catch up on the next turn that does go through
    /// `process_turn`. Acceptable for now; revisit if a long yield-wait
    /// turns out to need them injected sooner.
    pub async fn resume_after_yield(
        self: &Arc<Self>,
        runtime: AgentRuntime,
    ) -> anyhow::Result<TurnResult> {
        let turn_guard = Arc::clone(&self.turn_lock).lock_owned().await;
        let mut session = Arc::clone(&self.session).lock_owned().await;

        let persist_hook = session.persist.clone();
        let channel_for_send = session.resolve_channel();
        let reply_target = session.reply_target().unwrap_or("").to_string();

        let cancel_token = tokio_util::sync::CancellationToken::new();
        *self.turn_cancel.lock().unwrap() = cancel_token.clone();
        session.cancel_token = Some(cancel_token);

        let session_override = session.session_override.clone();
        let mut prompt_config = runtime.defaults.prompt.clone();
        if let Some(pm) = session_override.permission_mode {
            prompt_config.permission_mode = pm;
        }
        // A resumed turn is Interactive (the default) — same as a user/wake/
        // recovery turn (RFC channel-role-split §1.1); it isn't a
        // cron/heartbeat Background message.
        prompt_config.run_mode = Default::default();

        let system_prompt = match &session_override.system_prompt_override {
            Some(custom) => custom.clone(),
            None => runtime.build_system_prompt(&prompt_config),
        };

        let thinking = session_override.to_thinking_config();
        let model_id = session_override.model.as_deref();
        let turn_ctx = TurnContext {
            system_prompt: &system_prompt,
            model_id,
            thinking: thinking.as_ref(),
            permission_mode: prompt_config.permission_mode,
            run_mode: prompt_config.run_mode,
        };

        let notice_guard = NoticeTurnGuard::new(self, false);

        self.run_and_deliver(
            session,
            turn_guard,
            turn_ctx,
            &runtime,
            channel_for_send,
            reply_target,
            false,
            notice_guard,
            persist_hook,
        )
        .await
    }
}

// ── #151 Phase 8+ PendingWorkSession facade ──────────────────────────────────
impl crate::api::session_store::PendingWorkSession for SessionContext {
    fn add_pending_task(&self, task_id: String) {
        SessionContext::add_pending_task(self, task_id);
    }
}

/// issue #238: `try_fill_pending_yield` behavior tests — the queue/fill
/// mechanism only, not the spawned resume turn itself (that requires a real
/// or mocked LLM provider; these tests only need to observe the synchronous
/// state mutation that happens before the resume is even spawned).
#[cfg(test)]
mod pending_yield_tests {
    use super::*;
    use crate::agents::SessionManager;
    use crate::agents::session::PendingYield;

    fn make_ctx() -> Arc<SessionContext> {
        let manager = Arc::new(SessionManager::in_memory());
        manager.get_or_create_context("mock:default:u1")
    }

    fn bailing_runtime() -> AgentRuntime {
        crate::agents::agent::tests::bailing_runtime()
    }

    #[tokio::test]
    async fn noop_without_a_pending_yield() {
        let ctx = make_ctx();
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "sub-agent A done".to_string(),
        });

        ctx.try_fill_pending_yield(bailing_runtime()).await;

        // Nothing to fill it into — the event must stay queued, not be
        // silently dropped, so a later yield can still pick it up.
        assert_eq!(ctx.take_pending_yield_events().len(), 1);
    }

    #[tokio::test]
    async fn puts_pending_yield_back_when_queue_is_empty() {
        let ctx = make_ctx();
        {
            let mut session = ctx.session.lock().await;
            session.pending_yield = Some(PendingYield {
                tool_call_id: "call_y1".to_string(),
                implicit: false,
            });
        }

        ctx.try_fill_pending_yield(bailing_runtime()).await;

        let session = ctx.session.lock().await;
        assert_eq!(
            session.pending_yield.as_ref().map(|p| p.tool_call_id.as_str()),
            Some("call_y1"),
            "pending_yield must be restored, not lost, when there's nothing to fill it with yet"
        );
        assert!(
            !session
                .history
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("call_y1")),
            "must not fabricate a result when the queue is empty"
        );
    }

    #[tokio::test]
    async fn batches_queued_events_into_one_tool_result() {
        let ctx = make_ctx();
        {
            let mut session = ctx.session.lock().await;
            session.pending_yield = Some(PendingYield {
                tool_call_id: "call_y2".to_string(),
                implicit: false,
            });
        }
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "sub-agent A done: result A".to_string(),
        });
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "sub-agent B done: result B".to_string(),
        });

        ctx.try_fill_pending_yield(bailing_runtime()).await;

        let session = ctx.session.lock().await;
        assert!(
            session.pending_yield.is_none(),
            "pending_yield must be cleared once filled"
        );
        let filled: Vec<_> = session
            .history
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("call_y2"))
            .collect();
        assert_eq!(filled.len(), 1, "must be exactly ONE tool_result, not one per event");
        let text = filled[0]
            .parts
            .iter()
            .filter_map(|p| match p {
                crate::providers::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("result A"), "got: {text}");
        assert!(text.contains("result B"), "got: {text}");
        assert_eq!(filled[0].name.as_deref(), Some("sessions_yield"));
        assert_eq!(filled[0].is_error, Some(false));
    }

    /// issue #240: a queued event must survive a daemon restart — enqueue,
    /// simulate restart (drop + re-materialize the SessionContext from the
    /// same backend), and confirm the event is still there for the next
    /// fill.
    #[tokio::test]
    async fn enqueued_event_survives_restart() {
        let manager = Arc::new(SessionManager::in_memory());
        let ctx = manager.get_or_create_context("mock:default:u1");
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "sub-agent A done".to_string(),
        });

        manager.drop_context("mock:default:u1");
        let ctx2 = manager.get_or_create_context("mock:default:u1");

        let events = ctx2.take_pending_yield_events();
        assert_eq!(events.len(), 1, "queued event must survive the restart");
        assert_eq!(events[0].content, "sub-agent A done");
    }

    /// issue #240: multiple queued events must round-trip in FIFO order —
    /// the on-disk representation is a JSON array, and a naive
    /// implementation could reverse or reorder it.
    #[tokio::test]
    async fn multiple_queued_events_survive_restart_in_order() {
        let manager = Arc::new(SessionManager::in_memory());
        let ctx = manager.get_or_create_context("mock:default:u1");
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "first".to_string(),
        });
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "second".to_string(),
        });

        manager.drop_context("mock:default:u1");
        let ctx2 = manager.get_or_create_context("mock:default:u1");

        let events = ctx2.take_pending_yield_events();
        let contents: Vec<&str> = events.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(contents, vec!["first", "second"]);
    }

    /// issue #240: once a fill consumes the queue, the on-disk copy must be
    /// cleared too — otherwise a restart right after a fill would resurrect
    /// already-delivered events and double-deliver them.
    #[tokio::test]
    async fn filled_queue_does_not_resurrect_after_restart() {
        let manager = Arc::new(SessionManager::in_memory());
        let ctx = manager.get_or_create_context("mock:default:u1");
        {
            let mut session = ctx.session.lock().await;
            session.pending_yield = Some(PendingYield {
                tool_call_id: "call_y3".to_string(),
                implicit: false,
            });
        }
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "sub-agent A done".to_string(),
        });
        ctx.try_fill_pending_yield(bailing_runtime()).await;

        manager.drop_context("mock:default:u1");
        let ctx2 = manager.get_or_create_context("mock:default:u1");

        assert!(
            ctx2.take_pending_yield_events().is_empty(),
            "a delivered event must not come back after a restart"
        );
    }
}

/// Phase 2b (issue #256): `park_for_yield`'s three outcomes — fast path
/// (already queued), live-waiter delivery (park, then wake via
/// `try_fill_pending_yield`), and cancellation (a superseding waiter
/// registration drops the original sender). Exercises the new logic
/// directly rather than through a full `process_turn`/LLM round-trip, same
/// spirit as `pending_yield_tests` above.
#[cfg(test)]
mod park_for_yield_tests {
    use super::*;
    use crate::agents::SessionManager;
    use crate::agents::session::PendingYield;

    fn make_ctx() -> Arc<SessionContext> {
        let manager = Arc::new(SessionManager::in_memory());
        manager.get_or_create_context("mock:default:u1")
    }

    async fn guards(ctx: &Arc<SessionContext>) -> (OwnedMutexGuard<Session>, OwnedMutexGuard<()>) {
        let session = Arc::clone(&ctx.session).lock_owned().await;
        let turn_guard = Arc::clone(&ctx.turn_lock).lock_owned().await;
        (session, turn_guard)
    }

    /// The queue already has an event by the time `park_for_yield` runs
    /// (issue #238's original race, event-before-yield) — must deliver
    /// immediately without ever registering a waiter or actually parking.
    #[tokio::test]
    async fn fast_path_delivers_from_an_already_queued_event() {
        let ctx = make_ctx();
        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "sub-agent A done".to_string(),
        });
        let (mut session, turn_guard) = guards(&ctx).await;
        session.pending_yield = Some(PendingYield {
            tool_call_id: "call_fast".to_string(),
            implicit: false,
        });

        let (session, _turn_guard, filled) = ctx
            .park_for_yield(session, turn_guard, "call_fast".to_string())
            .await;

        assert!(filled, "an already-queued event must be delivered without parking");
        assert!(session.pending_yield.is_none());
        assert!(
            ctx.yield_waiter.lock().unwrap().is_none(),
            "the fast path must never register a waiter"
        );
        let tool_msg = session
            .history
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_fast"))
            .expect("tool_result must be written");
        assert!(tool_msg.text_content().contains("sub-agent A done"));
    }

    /// Nothing queued yet: `park_for_yield` must register a waiter and
    /// actually suspend — `try_fill_pending_yield` (the same entry point
    /// `route_notice` drives) then finds that live waiter and signals it
    /// directly instead of writing to history itself, and the parked task
    /// wakes up, writes the result, and reports `filled = true`.
    #[tokio::test]
    async fn live_waiter_is_signaled_and_the_parked_task_delivers_it() {
        let ctx = make_ctx();
        let (mut session, turn_guard) = guards(&ctx).await;
        session.pending_yield = Some(PendingYield {
            tool_call_id: "call_park".to_string(),
            implicit: false,
        });
        // Drop our own guards before spawning — park_for_yield needs to
        // acquire them itself (it owns them from here), and this test's
        // "waker" side below needs the session lock free to enqueue +
        // fill.
        drop(session);
        drop(turn_guard);
        let (session, turn_guard) = guards(&ctx).await;

        let park_ctx = Arc::clone(&ctx);
        let handle = tokio::spawn(async move {
            park_ctx
                .park_for_yield(session, turn_guard, "call_park".to_string())
                .await
        });

        // Give the spawned task a chance to register its waiter before we
        // check for it (no fixed sleep — poll with a short bound).
        for _ in 0..200 {
            if ctx.yield_waiter.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            ctx.yield_waiter.lock().unwrap().is_some(),
            "park_for_yield must register a waiter when nothing is queued yet"
        );

        ctx.enqueue_pending_yield_event(PendingYieldEvent {
            content: "sub-agent B done".to_string(),
        });
        ctx.try_fill_pending_yield(crate::agents::agent::tests::bailing_runtime())
            .await;

        let (session, _turn_guard, filled) =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                .await
                .expect("park_for_yield must wake promptly once signaled")
                .expect("spawned task must not panic");

        assert!(filled, "the live-waiter path must report a successful delivery");
        assert!(session.pending_yield.is_none());
        let tool_msg = session
            .history
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_park"))
            .expect("tool_result must be written by the woken task itself");
        assert!(tool_msg.text_content().contains("sub-agent B done"));
        // try_fill_pending_yield's live-waiter branch must NOT have written
        // the result itself — single writer stays the woken park task.
        assert!(
            ctx.yield_waiter.lock().unwrap().is_none(),
            "the waiter slot must be empty again after delivery"
        );
    }

    /// A supersede (a second `park_for_yield` registering its own waiter
    /// for a DIFFERENT tool_call) must cancel the first one outright — its
    /// `rx` resolves to `Err`, and it must report `filled = false` and
    /// leave history untouched rather than racing the new owner.
    #[tokio::test]
    async fn superseding_waiter_cancels_the_original_park() {
        let ctx = make_ctx();
        let (mut session, turn_guard) = guards(&ctx).await;
        session.pending_yield = Some(PendingYield {
            tool_call_id: "call_old".to_string(),
            implicit: false,
        });
        drop(session);
        drop(turn_guard);
        let (session, turn_guard) = guards(&ctx).await;

        let park_ctx = Arc::clone(&ctx);
        let handle = tokio::spawn(async move {
            park_ctx
                .park_for_yield(session, turn_guard, "call_old".to_string())
                .await
        });

        for _ in 0..200 {
            if ctx.yield_waiter.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(ctx.yield_waiter.lock().unwrap().is_some());

        // Simulate a new sessions_yield superseding the old one: overwrite
        // the single-slot waiter registry directly, exactly what a second
        // `park_for_yield` call for a new tool_call would do.
        let (_tx, _rx) = tokio::sync::oneshot::channel::<String>();
        *ctx.yield_waiter.lock().unwrap() = Some(YieldWaiter {
            tool_call_id: "call_new".to_string(),
            tx: _tx,
        });

        let (session, _turn_guard, filled) =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                .await
                .expect("a cancelled park must still resolve promptly")
                .expect("spawned task must not panic");

        assert!(!filled, "a cancelled park must report no delivery");
        assert!(
            !session
                .history
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("call_old")),
            "a cancelled park must not write anything to history"
        );
    }

    /// Regression: content already handed to the parked task via its waiter
    /// (the `Ok(content)` branch) must not be silently dropped if
    /// `pending_yield` gets superseded (by a new yield, or a #248
    /// interjection close) in the narrow window between the send and the
    /// parked task reacquiring the session lock — it must be re-queued so
    /// whatever superseded it can still pick it up.
    #[tokio::test]
    async fn content_delivered_after_a_late_supersede_is_requeued_not_lost() {
        let ctx = make_ctx();
        let (mut session, turn_guard) = guards(&ctx).await;
        session.pending_yield = Some(PendingYield {
            tool_call_id: "call_a".to_string(),
            implicit: false,
        });
        drop(session);
        drop(turn_guard);
        let (session, turn_guard) = guards(&ctx).await;

        let park_ctx = Arc::clone(&ctx);
        let handle = tokio::spawn(async move {
            park_ctx
                .park_for_yield(session, turn_guard, "call_a".to_string())
                .await
        });

        for _ in 0..200 {
            if ctx.yield_waiter.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(ctx.yield_waiter.lock().unwrap().is_some());

        // Hold the session lock ourselves so the spawned task's own
        // reacquire (right after `rx.await` resolves) blocks until we
        // release it below — this deterministically orders "content sent"
        // before "pending_yield superseded", reproducing the race exactly.
        let mut held_session = Arc::clone(&ctx.session).lock_owned().await;

        let waiter = ctx.yield_waiter.lock().unwrap().take().expect("waiter must be registered");
        assert_eq!(waiter.tool_call_id, "call_a");
        let _ = waiter.tx.send("sub-agent A done".to_string());

        // Simulate a #248 interjection close superseding call_a while the
        // parked task's content is already in flight: the waiter slot is
        // already empty (taken above), so a real interjection's cancel
        // would be a no-op here too.
        held_session.pending_yield = None;
        drop(held_session);

        let (session, _turn_guard, filled) =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                .await
                .expect("a superseded-after-send park must still resolve promptly")
                .expect("spawned task must not panic");

        assert!(!filled, "a superseded delivery must report no delivery");
        assert!(
            !session
                .history
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("call_a")),
            "the superseded tool_call must not receive a second write"
        );
        let requeued = ctx.take_pending_yield_events();
        assert_eq!(
            requeued.len(),
            1,
            "the already-delivered content must be re-queued, not dropped"
        );
        assert_eq!(requeued[0].content, "sub-agent A done");
    }
}

/// issue #248: a plain-text user interjection while a `sessions_yield` is
/// still pending must close that yield's tool_result instead of appending a
/// new `[user]` message. `process_turn` is called end-to-end with a bailing
/// provider (`agent.run` returns Err quickly) — irrelevant here since the
/// closing logic runs, and mutates history, BEFORE `run_and_deliver` ever
/// calls the provider.
#[cfg(test)]
mod yield_interjection_tests {
    use super::*;
    use crate::agents::SessionManager;
    use crate::agents::session::PendingYield;
    use crate::api::message::{
        ChannelFile, ChannelFileMeta, ChannelMessageContent, LocalFileBody, MessageReceiver,
        MessageSender,
    };

    fn make_ctx() -> Arc<SessionContext> {
        let manager = Arc::new(SessionManager::in_memory());
        manager.get_or_create_context("mock:default:u1")
    }

    fn bailing_runtime() -> AgentRuntime {
        crate::agents::agent::tests::bailing_runtime()
    }

    fn inbound(content: &str, files: Vec<ChannelFile>) -> ChannelInboundMessage {
        ChannelInboundMessage {
            id: "m1".to_string(),
            sender: MessageSender::new("u1".to_string()),
            receiver: MessageReceiver::new("u1".to_string()),
            content: ChannelMessageContent {
                text: content.to_string(),
                files,
                buttons: vec![],
            },
            timestamp: 0,
            interruption_scope_id: None,
            silenced_override: None,
            run_mode: Default::default(),
        }
    }

    #[tokio::test]
    async fn interjection_closes_pending_yield_instead_of_adding_a_user_message() {
        let ctx = make_ctx();
        {
            let mut session = ctx.session.lock().await;
            session.pending_yield = Some(PendingYield {
                tool_call_id: "call_y1".to_string(),
                implicit: false,
            });
        }

        let _ = ctx
            .process_turn(inbound("怎么样了？", vec![]), None, bailing_runtime())
            .await;

        let session = ctx.session.lock().await;
        assert!(
            session.pending_yield.is_none(),
            "the interjection must close the pending yield"
        );
        assert!(
            !session.history.iter().any(|m| m.role == "user"),
            "must not also add a [user] message for the same interjection"
        );
        let filled: Vec<_> = session
            .history
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("call_y1"))
            .collect();
        assert_eq!(filled.len(), 1, "must close exactly the pending tool_call");
        assert_eq!(filled[0].role, "tool");
        assert_eq!(filled[0].name.as_deref(), Some("sessions_yield"));
        assert_eq!(filled[0].is_error, Some(false));
        let text = filled[0]
            .parts
            .iter()
            .filter_map(|p| match p {
                crate::providers::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(
            text, "The user interrupted the wait and said: 怎么样了？",
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn no_pending_yield_takes_the_ordinary_user_message_path() {
        let ctx = make_ctx();

        let _ = ctx
            .process_turn(inbound("hello", vec![]), None, bailing_runtime())
            .await;

        let session = ctx.session.lock().await;
        assert!(
            session.history.iter().any(|m| m.role == "user"),
            "with no pending yield, an ordinary [user] message must still be added"
        );
        assert!(
            !session.history.iter().any(|m| m.tool_call_id.is_some()),
            "no tool_result should be synthesized when there was nothing to close"
        );
    }

    #[tokio::test]
    async fn media_carrying_interjection_falls_through_to_the_ordinary_path() {
        let ctx = make_ctx();
        {
            let mut session = ctx.session.lock().await;
            session.pending_yield = Some(PendingYield {
                tool_call_id: "call_y2".to_string(),
                implicit: false,
            });
        }

        let tmp = std::env::temp_dir().join(format!("myclaw-test-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, b"hello").unwrap();
        let file = ChannelFile {
            meta: ChannelFileMeta {
                file_name: "note.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                size_bytes: Some(5),
                source_url: None,
            },
            body: Arc::new(LocalFileBody::new(tmp.clone())),
        };

        let _ = ctx
            .process_turn(inbound("看这个文件", vec![file]), None, bailing_runtime())
            .await;
        let _ = std::fs::remove_file(&tmp);

        let session = ctx.session.lock().await;
        assert!(
            session.pending_yield.is_some(),
            "add_tool_result is text-only — a media-carrying interjection must NOT close the \
             yield (known simplification, same as resume_after_yield's own disclosed gaps)"
        );
        assert!(
            session.history.iter().any(|m| m.role == "user"),
            "falls through to the ordinary add_user_with_media path"
        );
        assert!(!session.history.iter().any(|m| m.tool_call_id.is_some()));

        // InMemoryBackend::save_session_file (agents/session/backend.rs)
        // deliberately writes attached files to real disk under
        // `<cwd>/sessions/<id>/` even for the "in-memory" test backend —
        // clean up the directory this test caused it to create.
        let session_dir = std::env::current_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("sessions")
            .join(crate::ids::bare_dir_name(&session.id));
        drop(session);
        let _ = std::fs::remove_dir_all(&session_dir);
    }
}
