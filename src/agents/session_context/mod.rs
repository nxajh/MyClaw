//! `SessionContext` — bundles a `Session` with everything the per-turn
//! pipeline needs.
//!
//! RFC v2 §三.A: SessionContext is the boundary that owns per-session
//! mutable state (attachments, pending retry, turn lock) and drives
//! `process_turn`. `Agent::run` takes `&mut Session` plus a `TurnContext`
//! (already-resolved decisions); SessionContext is what the Orchestrator
//! holds in its session table and what `process_turn` operates on per turn.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agents::session::{PersistHook, Session};

pub(crate) mod helpers;
pub(crate) mod suspension;
pub(crate) mod tts;

pub(crate) use helpers::{age_session_media, history_looks_incomplete, is_bare_continue};
pub(crate) use suspension::{decide_silenced, semantic_stop_reason};
pub use suspension::TerminalRecord;
pub(crate) use tts::prepare_text_for_tts;
use crate::agents::turn::{PreviewState, SubResult, SubStatus, TurnResult, TurnSuspension};
use crate::agents::{Agent, AgentRuntime, TurnContext, UserProfile};
use crate::api::message::{Channel, ChannelInboundMessage};

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
    pub delegation_notice_queue:
        std::sync::Mutex<std::collections::VecDeque<DelegationNotice>>,
    /// Per-turn cancel token. Recreated each turn by `process_turn`.
    /// `/stop` fires this to cancel the in-flight turn without locking `session`.
    pub turn_cancel: std::sync::Mutex<tokio_util::sync::CancellationToken>,
}

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
        };
        ctx.restore_suspension();
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
        };
        ctx.restore_suspension();
        ctx
    }

    /// 方案 C (RFC §5): hydrate `turn_suspension` from
    /// `sessions/<sid>/suspension.json` at construction (daemon-restart
    /// recovery). Corrupt JSON is warned about and ignored — the session
    /// just starts loud like an unsuspended one.
    fn restore_suspension(&self) {
        let Some(hook) = &self.suspension_persist else {
            return;
        };
        let Some(json) = hook.load_suspension(&self.session_id) else {
            return;
        };
        if json.trim().is_empty() {
            return;
        }
        match serde_json::from_str::<TurnSuspension>(&json) {
            Ok(s) => {
                let pending = s.pending.len();
                *self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
                tracing::info!(
                    session = %self.session_id,
                    pending = pending,
                    "restored suspended turn from disk"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session = %self.session_id,
                    err = %e,
                    "suspension.json corrupt; ignoring"
                );
            }
        }
    }

    /// 方案 C (RFC §5): write the current `turn_suspension` to
    /// `sessions/<sid>/suspension.json`; `None` → empty string (file
    /// deleted). No-op without a persist hook. Called after every mutation
    /// point so a crash/restart never loses collected progress.
    fn persist_suspension(&self) {
        let Some(hook) = &self.suspension_persist else {
            return;
        };
        let json = match self
            .turn_suspension
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(s) => match serde_json::to_string(s) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(session = %self.session_id, err = %e, "serialize suspension failed");
                    return;
                }
            },
            None => String::new(),
        };
        hook.save_suspension(&self.session_id, &json);
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
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
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
        let _ = self.notice_turns_in_flight.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |v| Some(v.saturating_sub(1)),
        );
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
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
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
    /// active.
    pub fn record_terminal(
        &self,
        sub_session_id: String,
        status: SubStatus,
        content: String,
        sent_message_count: u64,
    ) -> TerminalRecord {
        let snapshot = {
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
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
                sub_session_id,
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
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
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
        let _turn_guard = self.turn_lock.lock().await;
        let mut session = self.session.lock().await;

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
        let mut notice_guard = NoticeTurnGuard::new(self, silenced_intent.is_some());

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
        session.turn_headless = matches!(
            turn_run_mode,
            crate::config::agent::RunMode::Background
        );
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
            self.suspension_snapshot()
                .and_then(|s| s.preview)
                .map(|p| crate::api::turn_stream::FoldCandidate {
                    msg_id: p.msg_id,
                    text: p.text,
                    // 单 preview (2026-08-12): cumulative counters + wall-clock
                    // start ride along so the FINAL summary line reflects the
                    // WHOLE message ("summary 没有累计", user-confirmed).
                    thinking_steps: p.thinking_steps,
                    tool_count: p.tool_count,
                    commentary_notes: p.commentary_notes,
                    started_at_unix_secs: p.started_at_unix_secs,
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

        // RFC §三.A line 312-323: process_turn computes the attachment
        // delta (skills/agents/MCP/memory/date/autonomy) against the
        // history's announced state and prepends a <system-reminder> to
        // the user content before recording the turn.
        let reminder = {
            let skills_snap = runtime.skills.read();
            // Clone history to avoid borrow conflict with attachments.
            let history_clone = session.history.clone();
            session.attachments.diff_skills(&skills_snap, &history_clone, Some(&session.owner));
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
            session.attachments.diff_date(runtime.context_engine.timezone_offset(), &history_clone);
            session.attachments.diff_autonomy(&prompt_config.permission_mode, &history_clone);
            // Daily throttled draft-skill backlog reminder (issue #89,
            // layer ②) — best-effort, never blocks the turn.
            if let Some(names) = crate::agents::skill_draft_reminder::check_and_arm(
                std::path::Path::new(&runtime.defaults.prompt.base_dir),
                runtime.context_engine.timezone_offset(),
            ) {
                session.attachments.push_skill_draft_reminder(names);
            }
            // Inject user/feedback memory index as system-reminder.
            let memory_root = &runtime.defaults.prompt.memory_root;
            let memory_entries: Vec<crate::memory::IndexEntry> = if !memory_root.is_empty() {
                let memory_dir = std::path::Path::new(memory_root);
                let files = crate::memory::scan_memory_files(memory_dir);
                files.iter().map(crate::memory::IndexEntry::from).collect()
            } else {
                Vec::new()
            };
            session.attachments.diff_memory(&memory_entries, &history_clone);
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

        // Notify channel that processing has started (typing indicator, etc.)
        // — suppressed on silenced resume turns (RFC §3.3).
        if !silenced {
            if let Some(ref ch) = channel_for_send {
                ch.on_status(&reply_target, crate::ProcessingStatus::Thinking)
                    .await;
            }
        }

        let result = self.agent.run(&mut session, turn_ctx, &runtime).await;
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
                let semantic =
                    semantic_stop_reason(silenced, turn_result.has_pending, turn_result.stop_reason);
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
                    && (delivery == crate::api::turn_stream::StreamDelivery::Pending || !suspended_turn)
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
                        if let Ok((tts_provider, tts_model)) =
                            runtime.providers.get_tts_provider()
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
                                                let mut r = crate::api::message::MessageReceiver::new(
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
                                                        audio_resp.audio.bytes.len() as u64,
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
                                                    content: crate::api::message::ChannelMessageContent {
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
                Ok(turn_result)
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
                Err(e)
            }
        }
    }
}



// ── #151 Phase 8+ PendingWorkSession facade ──────────────────────────────────
impl crate::api::session_store::PendingWorkSession for SessionContext {
    fn add_pending_task(&self, task_id: String) {
        SessionContext::add_pending_task(self, task_id);
    }
}
