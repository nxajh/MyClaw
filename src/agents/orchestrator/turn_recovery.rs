//! Startup recovery: resume turns interrupted by a previous crash / SIGKILL.
//!
//! User sessions with incomplete turns are resumed through the normal turn
//! path (`dispatch_turn` → `process_turn`) so channel-scoped tools (e.g.
//! `send_message`) stay available via `Session::resolve_channel()`. Only the
//! routing key's active session is recovered; inactive sessions resume when
//! the user switches back.
//!
//! Unfinished sub-agents are recovered via `Agent::run_recovery` and the
//! terminal event is delivered through [`CompletionSink`].

use std::collections::BTreeMap;
use std::sync::Arc;

use super::ctx::OrchestratorCtx;
use super::delegation::{drain_delegation_notices, process_non_active, route_notice};
use super::key::SessionKey;
use super::turn::ResolvedTurn;
use crate::agents::session_context::TerminalRecord;
use crate::agents::turn::{SubStatus, TurnSuspension};
use crate::agents::{
    AgentRuntime, DelegationCoordinator, DelegationEvent, DelegationNotice, SessionContext,
    UnfinishedSubAgent,
};
use crate::api::message::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};

/// Where a recovered sub-agent turn's output goes.
enum CompletionSink {
    /// Sub-agent session: emit `DelegationEvent::Completed` to wake the parent.
    Delegate {
        sub_session_id: String,
        parent_session_id: String,
        #[allow(dead_code)]
        reply_target: String,
        delegator: Option<Arc<DelegationCoordinator>>,
    },
}

impl CompletionSink {
    #[allow(dead_code)]
    async fn deliver(self, text: String) {
        let CompletionSink::Delegate {
            sub_session_id,
            parent_session_id,
            reply_target: _,
            delegator,
        } = self;
        if let Some(dm) = delegator {
            if let Some(tx) = dm.event_sender() {
                let _ = tx
                    .send(DelegationEvent::Completed {
                        sub_session_id,
                        parent_session_id,
                        summary: text,
                        duration_secs: 0,
                        sent_message_count: 0,
                    })
                    .await;
            }
        }
    }

    /// Emit a `Failed` terminal event for a delegate sink. Called when the
    /// recovery turn itself errors.
    async fn fail(self, error: String) {
        let CompletionSink::Delegate {
            sub_session_id,
            parent_session_id,
            delegator,
            ..
        } = self;
        if let Some(dm) = delegator {
            if let Some(tx) = dm.event_sender() {
                let _ = tx
                    .send(DelegationEvent::Failed {
                        sub_session_id,
                        parent_session_id,
                        error,
                    })
                    .await;
            }
        }
    }
}

/// Spawn the recovery turn for one session. The LLM work runs in the
/// background so the event loop starts without blocking.
#[allow(dead_code)]
fn spawn_recovery(
    turn_tracker: super::ctx::SharedTurnTracker,
    session_ctx: Arc<SessionContext>,
    runtime: AgentRuntime,
    label: &'static str,
    id: String,
    sink: CompletionSink,
) {
    tokio::spawn(async move {
        let _guard = turn_tracker.track();
        let _turn_guard = session_ctx.turn_lock.lock().await;
        let mut session = session_ctx.session.lock().await;

        let resolved = ResolvedTurn::resolve(&session, &runtime);
        let turn_ctx = resolved.turn_context();

        match session_ctx
            .agent
            .run_recovery(&mut session, turn_ctx, &runtime)
            .await
        {
            Ok(Some(tr)) if !tr.text.is_empty() => {
                tracing::info!(id = %id, "{label}: turn completed");
                sink.deliver(tr.text).await;
            }
            Ok(_) => {
                tracing::debug!(id = %id, "{label}: no recovery needed");
            }
            Err(e) => {
                tracing::warn!(id = %id, err = %e, "{label} failed");
                // P1-3: a failed sub-agent recovery must still emit a
                // terminal event — `recover_suspension` skips pending tasks
                // "covered" by this loop, so without this the parent
                // suspension would never resolve (hang).
                sink.fail(e.to_string()).await;
            }
        }
    });
}

/// Return whether startup should dispatch recovery for this session.
fn should_recover_active_session(
    active_id: Option<&str>,
    session_id: &str,
    history: &[crate::providers::capability_chat::ChatMessage],
    suspended: bool,
) -> bool {
    active_id == Some(session_id)
        && !history.is_empty()
        && super::history_has_incomplete_turn(history)
        && !suspended
}

/// Resume all incomplete user sessions and sub-agents found on disk.
///
/// User sessions: dispatch active sessions through the normal turn path
/// (`dispatch_turn` → `process_turn`) so channel-scoped tools (e.g.
/// `send_message`) remain available via `Session::resolve_channel()` during
/// the resumed turn. Inactive sessions are left for the normal message path.
///
/// Sub-agents: resume via `run_recovery` and emit the terminal event.
///
/// Whether a persisted `DelegationCheckpoint.status` is a terminal tombstone
/// (方案 A): the sub-agent's lifecycle already ended (Completed / Failed /
/// TimedOut / Cancelled) and the parent side received the terminal event, so
/// startup must NOT resume it — re-running would duplicate the work. Only
/// "running" / "checkpointed" checkpoints are resumable; a missing checkpoint
/// is a crash remnant.
fn is_terminal_checkpoint_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "timed_out" | "cancelled")
}

/// Collect the sub-session ids whose checkpoint is a terminal tombstone.
fn terminal_checkpoint_ids(checkpoints: &[crate::storage::DelegationCheckpoint]) -> std::collections::HashSet<String> {
    checkpoints
        .iter()
        .filter(|cp| is_terminal_checkpoint_status(&cp.status))
        .map(|cp| cp.sub_session_id.clone())
        .collect()
}

pub fn run_startup(
    ctx: &Arc<OrchestratorCtx>,
    unfinished: &[UnfinishedSubAgent],
    all_sessions: Vec<crate::storage::SessionInfo>,
) {
    let sessions = Arc::clone(&ctx.sessions);
    let _runtime = ctx.runtime.clone();
    let channels = ctx.channels.clone();
    let delegator = ctx.delegator.clone();
    let _turn_tracker = ctx.turn_tracker.clone();
    let backend = Arc::clone(sessions.backend());

    // 方案 A: sub-agents whose durable checkpoint carries a terminal status
    // (tombstone) already finished their lifecycle — the parent received the
    // terminal event before the daemon died. Exclude them from recovery AND
    // from the `covered` set (their terminal event will NOT be re-emitted, so
    // a parent suspension still holding such a pending task must fall through
    // to the uncovered-fail backstop and let the parent re-decide on the next
    // turn).
    let terminal_ids: std::collections::HashSet<String> =
        terminal_checkpoint_ids(&backend.load_delegation_checkpoints());

    // P1-1: sessions with a persisted non-empty suspension (daemon died while
    // a turn was suspended). These are excluded from the incomplete-turn
    // recovery below — a suspended turn waits on delegation events, not an
    // incomplete history — and recovered via `recover_suspension` instead.
    // Corrupt/unparseable suspension files are ignored here; the session's
    // own restore path warns about them.
    let suspended_ids: std::collections::HashSet<String> = all_sessions
        .iter()
        .filter(|info| {
            backend
                .load_suspension(&info.id)
                .and_then(|j| serde_json::from_str::<TurnSuspension>(&j).ok())
                .is_some_and(|s| !s.pending.is_empty())
        })
        .map(|info| info.id.clone())
        .collect();

    // Sub-session ids (FQID) the sub-agent recovery loop below will complete:
    // their terminal events arrive through the normal wake path, so
    // `recover_suspension` must not fail them. Terminal tombstones are
    // EXCLUDED — their terminal event is not re-emitted, so a parent
    // suspension still holding such a pending task must fall through to the
    // uncovered-fail backstop below instead of waiting forever.
    let covered: std::collections::HashSet<String> = unfinished
        .iter()
        .filter(|sa| !terminal_ids.contains(&sa.sub_session_id))
        .map(|sa| sa.sub_session_id.clone())
        .collect();

    // Recover only the active session for each routing key. Inactive sessions
    // resume when the user switches back and sends a normal message.
    let mut seen_owners = std::collections::HashSet::new();
    for info in &all_sessions {
        let key = info.owner.clone();
        if !seen_owners.insert(key.clone()) {
            continue;
        }
        let active_id = sessions.active_session_id(&key);
        let snap = sessions.get_or_create(&key);
        if !should_recover_active_session(
            active_id.as_deref(),
            &snap.id,
            &snap.history,
            suspended_ids.contains(&snap.id),
        ) {
            continue;
        }
        let Some(parsed) = SessionKey::parse(&key) else {
            tracing::warn!(routing_key = %key, "startup recovery: invalid routing key");
            continue;
        };
        if channels.get(&parsed.account_key()).is_none() {
            tracing::warn!(routing_key = %key, "startup recovery: channel not found");
            continue;
        }
        // Reply target for the recovered turn's output: the receiver of the
        // session's last persisted message (thread preserved), falling back to
        // a DM to the sender.
        let reply_target = snap
            .last_message
            .as_ref()
            .map(|m| m.receiver.clone())
            .unwrap_or_else(|| MessageReceiver::new(parsed.sender.clone()));
        tracing::info!(session = %key, "startup recovery: found incomplete turn, spawning recovery + replay task");
        let ctx = Arc::clone(ctx);
        tokio::spawn(async move {
            recover_active_session(ctx, parsed, reply_target).await;
        });
    }

    for sa in unfinished {
        if sa.sub_session_id.is_empty() || sa.parent_session_id.is_empty() {
            tracing::debug!(session_id = %sa.sub_session_id, "sub-agent recovery: skipping (no sub_session_id or parent_session_id)");
            continue;
        }

        // 方案 A tombstone: a terminal checkpoint means the sub-agent's
        // lifecycle already ended (the parent received its terminal event
        // before the daemon died) and only the mid-turn history remains.
        // Resuming would RE-RUN the finished task — instead skip it, remove
        // the tombstone (the sub-session history is kept for reference), and
        // emit nothing: `covered` already excludes this id, so a parent
        // suspension still holding the pending task falls through to the
        // uncovered-fail backstop and re-decides on the next turn.
        if terminal_ids.contains(&sa.sub_session_id) {
            tracing::info!(
                session_id = %sa.sub_session_id,
                agent = %sa.agent_name,
                "sub-agent startup recovery: terminal checkpoint tombstone — skipping resume"
            );
            if let Err(e) = backend.delete_delegation_checkpoint(&sa.sub_session_id) {
                tracing::warn!(session_id = %sa.sub_session_id, err = %e, "cleanup terminal tombstone failed");
            }
            continue;
        }

        // Build the completion sink up front so both branches (recover /
        // fail) can use it.
        let sink = CompletionSink::Delegate {
            sub_session_id: sa.sub_session_id.clone(),
            // The event's `parent_session_id` must be the parent's SESSION ID
            // (wake / route_notice index by session id, not routing
            // key). E29 switched the other senders to session ids but
            // missed this one — it still passed the routing key
            // (`sa.session_key`), so the recovered task's Completed
            // event was unroutable and the covered pending entry in a
            // persisted suspension would never resolve.
            parent_session_id: sa.parent_session_id.clone(),
            reply_target: sa.reply_target.clone(),
            delegator: delegator.clone(),
        };

        // Load the sub-session by its ID — NOT via a SubAgentKey routing
        // key. The session's `owner` field is the parent's routing key
        // (e.g. "telegram:myclaw:…"), not the SubAgentKey format
        // ("main:myclaw/s/…"), so get_or_create would create a brand-new
        // empty session instead of loading the existing one. That caused
        // the history-empty check below to skip the task WITHOUT emitting
        // a terminal event, leaving the parent's suspension pending list
        // permanently stuck (turn_lock deadlock).
        let snap = sessions.get_by_id(&sa.sub_session_id);
        let needs_recovery = snap.as_ref().is_some_and(|s| {
            !s.history.is_empty() && super::history_has_incomplete_turn(&s.history)
        });

        if !needs_recovery {
            // The sub-agent session can't be recovered (not found, empty,
            // or already complete). Emit a Failed terminal event so the
            // parent's suspension clears — otherwise the pending entry
            // stays forever and deadlocks the turn_lock.
            let reason = match &snap {
                None => "daemon 重启，子代理会话未找到".to_string(),
                Some(s) if s.history.is_empty() => "daemon 重启，子代理会话历史为空".to_string(),
                Some(_) => "daemon 重启，子代理任务已完成".to_string(),
            };
            tracing::info!(
                session_id = %sa.sub_session_id,
                agent = %sa.agent_name,
                reason = %reason,
                "sub-agent startup recovery: no incomplete turn, emitting Failed terminal event"
            );
            tokio::spawn(async move {
                sink.fail(reason).await;
            });
            continue;
        }

        tracing::info!(session_id = %sa.sub_session_id, agent = %sa.agent_name, "sub-agent startup recovery: found incomplete turn, spawning background task");
        let session_ctx = sessions
            .load_context_by_session_id(&sa.sub_session_id)
            .unwrap_or_else(|| {
                panic!(
                    "sub-agent session {} existed in get_by_id but not load_context_by_session_id",
                    sa.sub_session_id
                )
            });
        if let Some(ref delegator) = delegator {
            // Compute the *remaining* timeout from the durable checkpoint
            // instead of re-issuing the full wall-clock budget: the sub-agent
            // already consumed part of its time before the crash, so the
            // resumed task should not get a fresh full timeout.
            //
            // remaining = started_at + timeout_secs - now, clamped to ≥ 1 s.
            // Falls back to the full `sa.timeout_secs` when no checkpoint is
            // found (crash remnant) — same budget as the original run.
            let remaining_secs = backend
                .load_delegation_checkpoint(&sa.sub_session_id)
                .map(|cp| {
                    let elapsed = (chrono::Utc::now() - cp.started_at)
                        .num_seconds()
                        .max(0) as u64;
                    cp.timeout_secs.saturating_sub(elapsed).max(1)
                })
                .unwrap_or(sa.timeout_secs);
            delegator.recover_async(
                sa.sub_session_id.clone(),
                sa.agent_name.clone(),
                sa.parent_session_id.clone(),
                session_ctx,
                remaining_secs,
                sa.allowed_tools.clone(),
            );
        } else {
            tokio::spawn(async move {
                sink.fail("daemon restarts, sub-agents disabled".to_string()).await;
            });
        }
    }

    // P1-1 (RFC §5): resume persisted suspensions. Prefer the registered
    // context when the session is active (its in-memory suspension is
    // authoritative); otherwise load a temporary one (restores the state from
    // disk at construction). Each uncovered pending task is failed with a
    // "daemon restarted" note; the merged notice then routes one resume turn.
    for info in &all_sessions {
        if !suspended_ids.contains(&info.id) {
            continue;
        }
        let Some(session_ctx) = sessions
            .registered_context_by_session_id(&info.id)
            .or_else(|| sessions.load_context_by_session_id(&info.id))
        else {
            tracing::warn!(session = %info.id, "startup recovery: cannot load context for suspended session");
            continue;
        };
        tracing::info!(session = %info.id, "startup recovery: found persisted suspension, spawning resume");
        let ctx = Arc::clone(ctx);
        let covered = covered.clone();
        tokio::spawn(async move {
            recover_suspension(&ctx, session_ctx, &covered).await;
        });
    }

    // P2 (2026-08-13, RFC delegation-notice-queue §5): re-deliver persisted
    // completion notices (crash between wake-persist and turn-delivery). Runs
    // AFTER suspension recovery so active-session lookups see the same
    // registrations the suspension loop just materialized.
    recover_completion_queue(ctx);

    // RFC inbound-spool §6.4: replay persisted inbound messages (crash between
    // receive and dispatch). Runs after completion-queue recovery so session
    // registrations are settled; the baseline watermark (captured at spool
    // open) already excludes messages appended by a hot-switch successor.
    recover_inbound_spool(ctx);
}

/// Recover ONE active session's interrupted turn, then replay its Pending
/// spool entries (RFC §6.4). Replaces the old synthetic `[系统通知]` message +
/// `dispatch_turn`: the recovery runs `Agent::run_recovery` directly against
/// the existing history — no synthetic user message is appended to history and
/// no restart notice is injected. Zero user confirmation, zero notification.
///
/// Phase 1 (holds `turn_lock` + session lock, like `spawn_recovery`): run
/// `run_recovery` — channel-scoped tools resolve their channel via
/// `Session::resolve_channel()` (RFC channel-role-split §1.2, wired by
/// SessionManager at materialization), so there is no handle to install or
/// uninstall here. Capture the recovered text + `pending_retry`, clear
/// `incomplete_turn`. The text is delivered OUTSIDE the locks.
///
/// Phase 2 (lock-free): replay Pending spooled entries for the key — AFTER the
/// recovery turn so the incomplete history is resolved before the next user
/// message is processed. `process_turn` acquires `turn_lock` itself, so the
/// locks must be released first (no deadlock).
async fn recover_active_session(
    ctx: Arc<OrchestratorCtx>,
    key: SessionKey,
    reply_target: MessageReceiver,
) {
    let sk = key.to_string();
    // Delivery handle for the recovered text — resolved from the live
    // registry (same channel the session's tools would resolve).
    let Some(channel) = ctx.channel(&key.account_key()) else {
        tracing::warn!(session = %sk, "startup recovery: channel gone; skipping session recovery");
        return;
    };
    let session_ctx = ctx.session_context_for(&sk);

    let text = {
        let _turn_guard = session_ctx.turn_lock.lock().await;
        let mut session = session_ctx.session.lock().await;
        let resolved = ResolvedTurn::resolve(&session, &ctx.runtime);
        let turn_ctx = resolved.turn_context();
        let result = session_ctx
            .agent
            .run_recovery(&mut session, turn_ctx, &ctx.runtime)
            .await;
        session.incomplete_turn = false;
        // Stash `pending_retry` while the session lock is held (same pattern
        // as process_turn's tail) so a later retry button keeps working.
        if let Ok(Some(tr)) = &result {
            if let Some(pr) = &tr.pending_retry {
                *session_ctx.pending_retry.lock().await = Some(pr.clone());
            }
        }
        drop(session);
        match result {
            Ok(Some(tr)) => {
                tracing::info!(session = %sk, "startup recovery: turn completed");
                tr.text
            }
            Ok(None) => {
                tracing::debug!(session = %sk, "startup recovery: no recovery needed");
                String::new()
            }
            Err(e) => {
                tracing::warn!(session = %sk, err = %e, "startup recovery failed");
                crate::agents::user_messages::user_facing_error_message(&e)
            }
        }
    };
    if !text.is_empty() {
        let message = ChannelOutboundMessage {
            receiver: reply_target,
            content: ChannelMessageContent::text(text),
            options: Default::default(),
        };
        if let Err(e) = channel.send_message(&message).await {
            tracing::warn!(session = %sk, err = %e, "startup recovery: deliver failed");
        }
    }

    // Fix (2026-08-15, notice-queue starvation after recovery turn): the
    // recovery turn holds `turn_lock` WITHOUT going through
    // `dispatch_turn_spawn`, so its end must run the same queue-drain tail —
    // delegation notices enqueued while the recovery turn ran (`route_notice`
    // saw a busy lock and queued for a "turn-end drain" that never came)
    // would otherwise starve forever. (issue #131 decision 3: the mirrored
    // queued-user-message drain that used to follow this — the actual
    // trigger for the 2026-08-14 incident this comment references — is gone;
    // user messages no longer queue at all, see the removed
    // `pending_user_messages` subsystem.)
    if session_ctx.has_queued_delegation_notices() {
        super::delegation::drain_delegation_notices(&ctx, &session_ctx.session_id).await;
    }

    // Phase 2 — lock-free (process_turn takes turn_lock itself).
    super::inbound::replay_pending_for_key(&ctx, &key).await;
}

/// RFC inbound-spool §6.4: replay Pending inbound messages persisted before a
/// crash. `pending()` returns only entries with `seq <= baseline` (open-time
/// watermark), so a hot-switch successor never replays messages it appended
/// while waiting for the old process.
///
/// v4: replay is grouped per routing key `(channel, account, sender_id)` and
/// only for ACTIVE sessions:
/// - no active session → entries stay in the spool; the `DispatchTurn`
///   pre-hook replays them when the user switches back;
/// - active session with an incomplete turn → skipped here; the
///   `recover_active_session` task (spawned by run_startup's incomplete-turn
///   loop) recovers the turn first, then replays the entries (Phase 2);
/// - active session, complete history → one task replays the key's entries
///   serially (oldest first) through `replay_pending_for_key`, which runs the
///   replay chain (full interception semantics minus the terminal
///   `DispatchTurn`) and marks each entry Done after its turn returns
///   (mark-after keeps at-least-once).
fn recover_inbound_spool(ctx: &Arc<OrchestratorCtx>) {
    let Some(spool) = ctx.inbound_spool.clone() else {
        // Degraded (in-memory-only delivery) or tests — nothing to recover.
        return;
    };
    let pending = spool.pending();
    if pending.is_empty() {
        return;
    }

    // Group by the routing-key triple. `sender` may itself contain ':', so the
    // triple is matched explicitly — never by string-splitting the key.
    let mut by_key: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for entry in &pending {
        *by_key
            .entry((
                entry.channel.clone(),
                entry.account.clone(),
                entry.msg.sender_id.clone(),
            ))
            .or_default() += 1;
    }
    tracing::info!(
        count = pending.len(),
        keys = by_key.len(),
        "replaying persisted inbound spool entries (active sessions)"
    );

    for ((channel, account, sender), count) in by_key {
        let key = SessionKey::new(channel, account, sender);
        let sk = key.to_string();
        let Some(active_id) = ctx.sessions.active_session_id(&sk) else {
            // Inactive session: leave the entries in the spool — the
            // switch-back replay (`DispatchTurn` pre-hook) drains them.
            tracing::debug!(session = %sk, count, "spool recovery: session inactive, leaving entries for switch-back");
            continue;
        };
        // Active session with an incomplete turn: `recover_active_session`
        // owns this key (Phase 1 recovery + Phase 2 replay) — skip to avoid a
        // double replay.
        if ctx.sessions.get_by_id(&active_id).is_some_and(|s| {
            !s.history.is_empty() && super::history_has_incomplete_turn(&s.history)
        }) {
            continue;
        }
        tracing::info!(session = %sk, count, "spool recovery: replaying active session");
        let ctx = Arc::clone(ctx);
        tokio::spawn(async move {
            super::inbound::replay_pending_for_key(&ctx, &key).await;
        });
    }
}

/// P2 (2026-08-13, RFC delegation-notice-queue §5): re-deliver completion
/// notices persisted before a crash. Entries written by `route_notice` are
/// re-enqueued into their parent session's notice queue (active sessions get
/// an immediate drain; non-active ones take the `process_non_active` path,
/// which marks the entry delivered once the turn persists its content).
/// Entries whose parent session no longer exists are dead-lettered (marked
/// delivered / dropped) with a warning — there is nothing left to deliver to.
fn recover_completion_queue(ctx: &Arc<OrchestratorCtx>) {
    let Some(store) = ctx.completion_queue.clone() else {
        // Degraded (P1 in-memory delivery) or tests — nothing to recover.
        return;
    };
    let pending = store.pending();
    if pending.is_empty() {
        return;
    }
    tracing::info!(count = pending.len(), "recovering persisted completion notices");

    for entry in pending {
        let session_id = entry.parent_session_id.clone();

        // Dead-letter: the parent session vanished — drop the entry.
        let Some(session) = ctx.sessions.get_by_id(&session_id) else {
            tracing::warn!(
                session_id = %session_id,
                notice_id = %entry.id,
                "completion queue: parent session not found; dead-lettering notice"
            );
            if let Err(e) = store.mark_delivered(&entry.id) {
                tracing::warn!(notice_id = %entry.id, err = %e, "completion queue: dead-letter mark failed");
            }
            continue;
        };

        // The entry's routing key (owner) decides activity + channel.
        let Some(key) = SessionKey::parse(&session.owner) else {
            tracing::warn!(
                session_id = %session_id,
                owner = %session.owner,
                "completion queue: invalid routing key; dead-lettering notice"
            );
            if let Err(e) = store.mark_delivered(&entry.id) {
                tracing::warn!(notice_id = %entry.id, err = %e, "completion queue: dead-letter mark failed");
            }
            continue;
        };

        let is_active = ctx
            .sessions
            .active_session_id(&session.owner)
            .is_some_and(|id| id == session_id);

        if is_active && ctx.channel(&key.account_key()).is_some() {
            // Active: re-enqueue into the registered context's notice queue
            // and drain immediately (mirrors `route_notice`'s active path).
            let Some(sctx) = ctx.sessions.registered_context_by_session_id(&session_id) else {
                // Materialization race (session switched away mid-startup) —
                // fall back to the non-active path.
                let ctx = Arc::clone(ctx);
                let sid = session_id.clone();
                let id = entry.id.clone();
                let content = entry.content.clone();
                let silenced = entry.silenced_override;
                tokio::spawn(async move {
                    process_non_active(&ctx, &sid, &key.sender, &content, silenced, Some(id)).await;
                });
                continue;
            };
            sctx.enqueue_delegation_notice(DelegationNotice {
                id: entry.id,
                content: entry.content,
                silenced_override: entry.silenced_override,
            });
            let ctx = Arc::clone(ctx);
            let sid = session_id.clone();
            tokio::spawn(async move {
                drain_delegation_notices(&ctx, &sid).await;
            });
        } else {
            // Non-active (or channel missing): process via the non-active
            // path with the persisted id so a successful turn marks the
            // entry delivered.
            let ctx = Arc::clone(ctx);
            let sid = session_id.clone();
            let id = entry.id.clone();
            let content = entry.content.clone();
            let silenced = entry.silenced_override;
            tokio::spawn(async move {
                process_non_active(&ctx, &sid, &key.sender, &content, silenced, Some(id)).await;
            });
        }
    }
}

/// P1-1 (RFC §5): recover one persisted suspension after a daemon restart.
///
/// Pending sub-sessions NOT covered by the sub-agent recovery loop (whose
/// terminal events arrive through the normal `wake` path) are failed with a
/// "daemon 重启，子代理中断" note — mirroring wake's terminal-event handling
/// (progress folding). The merged notice then routes one resume turn; once the
/// covered sub-sessions' terminal events land, `pending` is empty and the final
/// resume turn is loud (RFC §3.4).
///
/// issue #140: `pending` can now also hold shell `process_id`s (see
/// `ShellTool::register_pending`) — identified by `is_shell_process_id`'s
/// `sh_` prefix, checked against `ctx.shell_registry`'s POST-`adopt_after_restart`
/// state instead of the sub-agent `covered` set (a completely different
/// survival mechanism: shell has no timeout-bounded recovery loop, just
/// "did this process's real PID survive"). A shell entry still `running`
/// there IS covered — its adopted reaper will complete it naturally through
/// the normal `route_shell_completion` path — exactly parallel to how a
/// covered sub-session's terminal event arrives through `wake`.
async fn recover_suspension(
    ctx: &Arc<OrchestratorCtx>,
    session_ctx: Arc<SessionContext>,
    covered: &std::collections::HashSet<String>,
) {
    let snapshot = match session_ctx.suspension_snapshot() {
        Some(s) => s,
        None => return,
    };

    // Fail every pending item neither recovery loop will cover.
    let mut failed: Vec<String> = Vec::new();
    for id in snapshot.pending.iter() {
        if crate::tools::shell::is_shell_process_id(id) {
            let still_running = match &ctx.shell_registry {
                Some(registry) => registry.read().await.get(id).is_some_and(|e| e.is_running()),
                None => false,
            };
            if still_running {
                continue;
            }
            let content = match &ctx.shell_registry {
                Some(registry) => crate::tools::shell::recovery_lost_content(registry, id).await,
                None => format!(
                    "[系统通知] 后台命令状态未知 (process_id: {}): daemon 重启，无法查询该进程记录。若仍需要这次任务的结果，请重新执行。",
                    id
                ),
            };
            match session_ctx.record_terminal(id.clone(), SubStatus::Failed, content.clone(), 0) {
                TerminalRecord::Recorded(_) => failed.push(content),
                TerminalRecord::Duplicate | TerminalRecord::NoSuspension => {
                    tracing::debug!(
                        process_id = %id,
                        "recover_suspension: shell entry already collected or no suspension, skipping failure notice"
                    );
                }
            }
            continue;
        }

        let sub_session_id = id;
        if covered.contains(sub_session_id) {
            continue;
        }
        let progress = snapshot
            .progress_by_sub_session
            .get(sub_session_id)
            .cloned()
            .unwrap_or_default();
        let mut content = format!(
            "[系统通知] 子代理后台任务中断 (session_id: {}): daemon 重启，子代理进程已终止。请重新委托该任务。",
            sub_session_id
        );
        if !progress.is_empty() {
            content.push_str("\n\n任务过程记录：\n");
            for line in &progress {
                content.push_str(&format!("- {}\n", line));
            }
        }
        // 方案4: only include in the failure notice when this is a first
        // recording. A `Duplicate` (already collected by another path, e.g.
        // the sub-agent recovery loop's terminal event arriving first) means
        // the notice must NOT be re-sent — previously the return value was
        // discarded, causing duplicate failure notices.
        match session_ctx.record_terminal(
            sub_session_id.clone(),
            SubStatus::Failed,
            content.clone(),
            0,
        ) {
            TerminalRecord::Recorded(_) => {
                failed.push(content);
            }
            TerminalRecord::Duplicate | TerminalRecord::NoSuspension => {
                tracing::debug!(
                    sub_session_id = %sub_session_id,
                    "recover_suspension: already collected or no suspension, skipping failure notice"
                );
                continue;
            }
        }
    }

    // Every pending item is covered by one of the two recovery loops —
    // terminal events arrive naturally; nothing to synthesize here.
    if failed.is_empty() {
        return;
    }

    // Merge the failure notices (with the suspension gap) into one resume
    // turn. `pending` may still hold covered tasks, making this first resume
    // turn silent; the final loud summary comes when the last covered
    // terminal event lands.
    let mut merged = String::new();
    for content in &failed {
        merged.push_str(&format!("- {}\n", content));
    }
    let suspended_secs = chrono::Utc::now()
        .timestamp()
        .saturating_sub(snapshot.suspended_at as i64);
    let notice = format!(
        "[系统通知] daemon 重启，以下后台任务已中断（挂起时长约 {}s）：\n{}",
        suspended_secs, merged
    );
    route_notice(
        ctx,
        &session_ctx.session_id,
        notice,
        format!("recovery:{}", session_ctx.session_id),
        // P2: recovery-synthesized notices are NOT persisted (no NoticeMeta)
        // — the store only tracks wake-time delegation events, and the
        // `recovery:` id would never be marked delivered anyway.
        None,
    )
    .await;
}

/// P1-4: recovery unit tests — CompletionSink terminal-event delivery
/// (session_id must be the parent SESSION id, E29) and `recover_suspension`
/// semantics (fail uncovered tasks, leave covered ones for the natural wake
/// path, all-covered no-op).
#[cfg(test)]
mod tests {
    use super::super::test_support::test_ctx;
    use super::*;
    use crate::agents::session::SessionManager;
    use crate::agents::{AgentRegistry, DelegationCoordinator};
    use crate::config::sub_agent::{AgentIsolation, SubAgentConfig};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn delegator() -> Arc<DelegationCoordinator> {
        let registry = Arc::new(AgentRegistry::from_vec(vec![SubAgentConfig {
            name: "coder".to_string(),
            system_prompt: "coding specialist".to_string(),
            tools: crate::config::filters::ToolFilter::all(),
            skills: crate::config::filters::SkillFilter::all(),
            mcp: crate::config::filters::McpFilter::all(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::Shared,
            timeout: None,
        }]));
        let manager = Arc::new(SessionManager::in_memory());
        Arc::new(DelegationCoordinator::new(
            registry,
            manager,
            PathBuf::new(),
            "test",
            3,
            Default::default(),
        ))
    }

    /// 方案 A: only terminal checkpoint statuses are classified as tombstones
    /// (excluded from startup recovery); running/checkpointed are resumable.
    #[test]
    fn terminal_checkpoint_ids_filters_terminal_statuses() {
        use crate::storage::DelegationCheckpoint;
        let mk = |id: &str, status: &str| DelegationCheckpoint {
            parent_session_id: "test/s/p".to_string(),
            sub_session_id: id.to_string(),
            agent_name: "coder".to_string(),
            status: status.to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: 300,
            allowed_tools: None,
            last_checkpoint: None,
        };
        let cps = vec![
            mk("s/running", "running"),
            mk("s/checkpointed", "checkpointed"),
            mk("s/completed", "completed"),
            mk("s/failed", "failed"),
            mk("s/timed_out", "timed_out"),
            mk("s/cancelled", "cancelled"),
            mk("s/unknown", "bogus"),
        ];
        let ids = terminal_checkpoint_ids(&cps);
        for terminal in ["s/completed", "s/failed", "s/timed_out", "s/cancelled"] {
            assert!(ids.contains(terminal), "{terminal} should be terminal");
        }
        for resumable in ["s/running", "s/checkpointed", "s/unknown"] {
            assert!(!ids.contains(resumable), "{resumable} should NOT be terminal");
        }
    }

    /// P2 (2026-08-13, RFC delegation-notice-queue §5): entries whose parent
    /// session vanished are dead-lettered (dropped) — no re-delivery loop, no
    /// crash, and the file is removed.
    #[test]
    fn recovery_dead_letters_vanished_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::agents::orchestrator::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
        );
        store
            .append(crate::agents::orchestrator::CompletionNoticeEntry {
                seq: 0,
                id: "delegation:t1".to_string(),
                sub_session_id: "t1".to_string(),
                parent_session_id: "ghost-session".to_string(),
                status: Some("completed".to_string()),
                content: "notice".to_string(),
                silenced_override: None,
                sent_message_count: 0,
                enqueued_at: 0,
                delivery_state: crate::agents::orchestrator::DeliveryState::Pending,
            })
            .unwrap();
        let mut ctx = test_ctx(vec![]);
        ctx.completion_queue = Some(Arc::clone(&store));
        recover_completion_queue(&Arc::new(ctx));
        assert!(
            store.pending().is_empty(),
            "dead-lettered entry must be dropped"
        );
        assert!(
            !tmp.path().join("queue").join("1.json").exists(),
            "dead-lettered file must be removed"
        );
    }

    /// P2: an entry whose parent session EXISTS is re-delivered through the
    /// non-active path (spawned; no channel in the ctx → not the active
    /// branch). NullRegistry makes the turn fail, so the entry stays Pending —
    /// at-least-once semantics preserved (re-tried on the next restart).
    #[tokio::test]
    async fn recovery_keeps_pending_when_turn_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::agents::orchestrator::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
        );
        let mut ctx = test_ctx(vec![]);
        ctx.completion_queue = Some(Arc::clone(&store));
        // Create the parent session (get_or_create auto-activates; the
        // missing channel routes recovery through the non-active path).
        let session = ctx.sessions.get_or_create("mock:default:u1");
        let sid = session.id.clone();
        store
            .append(crate::agents::orchestrator::CompletionNoticeEntry {
                seq: 0,
                id: "delegation:t1".to_string(),
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                status: Some("completed".to_string()),
                content: "notice".to_string(),
                silenced_override: None,
                sent_message_count: 0,
                enqueued_at: 0,
                delivery_state: crate::agents::orchestrator::DeliveryState::Pending,
            })
            .unwrap();
        recover_completion_queue(&Arc::new(ctx));
        assert_eq!(
            store.pending().len(),
            1,
            "failed turn must keep the entry Pending (at-least-once)"
        );
    }

    #[tokio::test]
    async fn delegate_sink_delivers_completed_to_parent_session() {
        let dm = delegator();
        let (tx, mut rx) = mpsc::channel(8);
        dm.set_event_sender(tx);
        let sink = CompletionSink::Delegate {
            sub_session_id: "sub-session-1".to_string(),
            // E29: the event's parent_session_id must be the parent SESSION id
            // so wake / route_notice can index it.
            parent_session_id: "parent-session-1".to_string(),
            reply_target: String::new(),
            delegator: Some(dm),
        };
        sink.deliver("recovered text".to_string()).await;
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Completed {
                sub_session_id,
                parent_session_id,
                summary,
                duration_secs,
                sent_message_count,
            } => {
                assert_eq!(sub_session_id, "sub-session-1");
                assert_eq!(parent_session_id, "parent-session-1");
                assert_eq!(summary, "recovered text");
                assert_eq!(duration_secs, 0);
                assert_eq!(sent_message_count, 0);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_sink_fail_emits_failed_to_parent_session() {
        let dm = delegator();
        let (tx, mut rx) = mpsc::channel(8);
        dm.set_event_sender(tx);
        let sink = CompletionSink::Delegate {
            sub_session_id: "sub-session-1".to_string(),
            parent_session_id: "parent-session-1".to_string(),
            reply_target: String::new(),
            delegator: Some(dm),
        };
        sink.fail("recovery turn failed".to_string()).await;
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Failed {
                sub_session_id,
                parent_session_id,
                error,
            } => {
                assert_eq!(sub_session_id, "sub-session-1");
                assert_eq!(parent_session_id, "parent-session-1");
                assert_eq!(error, "recovery turn failed");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recover_suspension_fails_uncovered_tasks() {
        let ctx = Arc::new(test_ctx(vec![]));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("s1".to_string());
        sctx.add_pending_task("s2".to_string());
        sctx.add_progress("s1", "halfway report");
        let covered: HashSet<String> = ["s2".to_string()].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;

        let snap = sctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        let r = &snap.results[0];
        assert_eq!(r.sub_session_id, "s1");
        assert_eq!(r.status, SubStatus::Failed);
        assert!(r.content.contains("daemon 重启"), "content: {}", r.content);
        assert!(r.content.contains("halfway report"));
        // the covered sub-session is untouched — its terminal event arrives via wake
        assert_eq!(snap.pending, vec!["s2".to_string()]);
    }

    #[tokio::test]
    async fn recover_suspension_all_covered_is_noop() {
        let ctx = Arc::new(test_ctx(vec![]));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("s1".to_string());
        sctx.add_pending_task("s2".to_string());
        let covered: HashSet<String> = ["s1".to_string(), "s2".to_string()].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;
        let snap = sctx.suspension_snapshot().unwrap();
        assert!(snap.results.is_empty());
        assert_eq!(snap.pending.len(), 2);
    }

    /// issue #140: a shell `process_id` in `pending` that's still `running`
    /// in the registry after `adopt_after_restart` survived a hot switch —
    /// its adopted reaper will complete it naturally through the normal
    /// `route_shell_completion` path, exactly parallel to a covered
    /// sub-session. `recover_suspension` must leave it alone (no failure
    /// notice, no `record_terminal`).
    #[tokio::test]
    async fn recover_suspension_skips_shell_entry_still_running_after_adopt() {
        let registry: crate::tools::shell::ShellRegistry =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let mut ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        let sid = sctx.session_id.clone();
        sctx.add_pending_task("sh_alive".to_string());
        registry.write().await.insert(
            "sh_alive".to_string(),
            crate::tools::shell::test_proc_entry("sh_alive", &sid, "running"),
        );
        ctx.shell_registry = Some(registry);
        let ctx = Arc::new(ctx);

        let covered: HashSet<String> = [].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;

        let snap = sctx.suspension_snapshot().unwrap();
        assert!(snap.results.is_empty(), "a still-running shell entry must not be failed");
        assert_eq!(snap.pending, vec!["sh_alive".to_string()]);
    }

    /// issue #140: a shell `process_id` in `pending` that is NOT `running`
    /// post-adopt (cold restart lost it, or it died during the hot-switch
    /// window) did NOT survive — `recover_suspension` must synthesize a
    /// shell-specific failure notice for it (not the sub-agent wording) and
    /// collect it via `record_terminal`, same as an uncovered sub-session.
    #[tokio::test]
    async fn recover_suspension_fails_shell_entry_not_running_after_adopt() {
        let registry: crate::tools::shell::ShellRegistry =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let mut ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        let sid = sctx.session_id.clone();
        sctx.add_pending_task("sh_lost".to_string());
        registry.write().await.insert(
            "sh_lost".to_string(),
            crate::tools::shell::test_proc_entry("sh_lost", &sid, "lost_on_restart"),
        );
        ctx.shell_registry = Some(registry);
        let ctx = Arc::new(ctx);

        let covered: HashSet<String> = [].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;

        let snap = sctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        let r = &snap.results[0];
        assert_eq!(r.sub_session_id, "sh_lost");
        assert_eq!(r.status, SubStatus::Failed);
        assert!(r.content.contains("后台命令"), "must use shell wording, not sub-agent wording: {}", r.content);
        assert!(!r.content.contains("子代理"), "must NOT use sub-agent wording: {}", r.content);
        assert!(snap.pending.is_empty());
    }

    /// issue #140: a shell `process_id` in `pending` whose registry entry is
    /// gone entirely (very old restart already swept by `ENTRY_RETENTION_SECS`,
    /// or `ctx.shell_registry` itself is `None`) still gets a minimal failure
    /// notice — never silently stuck forever.
    #[tokio::test]
    async fn recover_suspension_fails_shell_entry_with_no_registry() {
        let ctx = Arc::new(test_ctx(vec![])); // shell_registry: None
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("sh_unknown".to_string());

        let covered: HashSet<String> = [].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;

        let snap = sctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        assert_eq!(snap.results[0].sub_session_id, "sh_unknown");
        assert_eq!(snap.results[0].status, SubStatus::Failed);
        assert!(snap.pending.is_empty());
    }

    /// 方案4: if a pending task was already collected by the sub-agent
    /// recovery loop's terminal event (arriving before recover_suspension
    /// reaches it in the loop), `record_terminal` returns `Duplicate` and the
    /// task must be skipped — no failure entry, no failure notice. This
    /// simulates the race by pre-collecting the task, then re-adding it to
    /// `pending` to mimic a snapshot taken before the terminal arrived.
    #[tokio::test]
    async fn recover_suspension_skips_pre_collected_task() {
        let ctx = Arc::new(test_ctx(vec![]));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("s1".to_string());
        sctx.add_pending_task("s2".to_string());
        // Pre-collect s1 (sub-agent recovery loop won the race)
        let _ = sctx.record_terminal(
            "s1".to_string(),
            SubStatus::Completed,
            "done by sub-agent".into(),
            0,
        );
        // Re-add s1 to pending — simulates recover_suspension's snapshot
        // having captured s1 before the terminal event collected it.
        {
            let mut guard = sctx.turn_suspension.lock().unwrap();
            if let Some(s) = guard.as_mut() {
                s.pending.push("s1".to_string());
            }
        }
        let covered: HashSet<String> = [].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;
        // s1 was already collected → Duplicate → skipped (no Failed entry).
        let snap = sctx.suspension_snapshot().unwrap();
        let s1 = snap
            .results
            .iter()
            .find(|r| r.sub_session_id == "s1")
            .unwrap();
        assert_eq!(s1.status, SubStatus::Completed);
        assert_eq!(s1.content, "done by sub-agent");
        // s2 was uncovered and NOT pre-collected → Failed entry.
        let s2 = snap
            .results
            .iter()
            .find(|r| r.sub_session_id == "s2")
            .unwrap();
        assert_eq!(s2.status, SubStatus::Failed);
    }

    #[test]
    fn should_recover_active_session_true_for_active_incomplete() {
        // Session is active, history is incomplete (trailing user, no assistant
        // response yet), not suspended.
        let history = vec![
            crate::providers::capability_chat::ChatMessage::user_text("hi"),
            crate::providers::capability_chat::ChatMessage::assistant_text("hello"),
            crate::providers::capability_chat::ChatMessage::user_text("again"),
        ];
        assert!(should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &history,
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_no_active_session() {
        let history = vec![crate::providers::capability_chat::ChatMessage::user_text(
            "hi",
        )];
        assert!(!should_recover_active_session(
            None, "sess-1", &history, false
        ));
    }

    #[test]
    fn should_recover_false_when_session_not_active() {
        let history = vec![crate::providers::capability_chat::ChatMessage::user_text(
            "hi",
        )];
        // active_id differs from this session's id.
        assert!(!should_recover_active_session(
            Some("other-session"),
            "sess-1",
            &history,
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_history_empty() {
        assert!(!should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &[],
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_history_complete() {
        let history = vec![
            crate::providers::capability_chat::ChatMessage::user_text("hi"),
            crate::providers::capability_chat::ChatMessage::assistant_text("hello"),
        ];
        assert!(!should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &history,
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_suspended() {
        let history = vec![crate::providers::capability_chat::ChatMessage::user_text(
            "hi",
        )];
        assert!(!should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &history,
            true,
        ));
    }

    /// RFC inbound-spool §6.4 (v4): Pending entries persisted by a crashed
    /// process are replayed only for ACTIVE sessions and marked Done after
    /// each replay turn returns. Simulates the crash by appending in one
    /// store instance (dropped without mark_done), reopening as the
    /// successor, then running recovery. The test creates the active session
    /// up front — without one, v4 leaves the entries in the spool for the
    /// switch-back hook (see `replay_noop_without_spool` for the no-spool
    /// case).
    #[tokio::test]
    async fn replay_persisted_inbound_and_marks_done() {
        let tmp = tempfile::tempdir().unwrap();
        let spool_dir = tmp.path().join("inbound_spool");
        {
            let spool = crate::agents::orchestrator::InboundSpool::open(spool_dir.clone()).unwrap();
            spool
                .append(
                    "telegram",
                    "acc1",
                    &super::super::test_support::inbound_msg("u1", "hi from before crash"),
                )
                .unwrap();
        }
        let spool = Arc::new(crate::agents::orchestrator::InboundSpool::open(spool_dir).unwrap());
        assert_eq!(
            spool.pending().len(),
            1,
            "reopened spool must see the pre-crash Pending entry"
        );
        let mut ctx = test_ctx(vec![]);
        // v4 replays only active sessions — register `telegram:acc1:u1` (the
        // key derived from the spooled entry's routing triple) as active.
        let _ = ctx.sessions.get_or_create("telegram:acc1:u1");
        ctx.inbound_spool = Some(Arc::clone(&spool));
        recover_inbound_spool(&Arc::new(ctx));
        // The replay task runs async — poll until mark_done lands.
        for _ in 0..100 {
            if spool.pending().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            spool.pending().is_empty(),
            "replay must dispatch and mark the entry done"
        );
        assert_eq!(spool.len(), 0);
    }

    /// RFC inbound-spool §6.4: no spool (degraded / tests) → recovery is a
    /// no-op; nothing to replay.
    #[tokio::test]
    async fn replay_noop_without_spool() {
        let ctx = test_ctx(vec![]);
        recover_inbound_spool(&Arc::new(ctx));
        // No panic; nothing to assert beyond completion.
    }

    /// The remaining-time computation used by `run_startup` when resuming a
    /// checkpointed sub-agent: `remaining = started_at + timeout_secs - now`,
    /// clamped to ≥ 1 s. This verifies the formula with a checkpoint whose
    /// `started_at` is 50 s ago and `timeout_secs` is 100 s → remaining ≈ 50.
    #[test]
    fn remaining_timeout_from_checkpoint_is_elapsed_budget() {
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(50);
        let timeout_secs = 100u64;

        // Same computation as `run_startup`'s recover_async block.
        let elapsed = (chrono::Utc::now() - started_at)
            .num_seconds()
            .max(0) as u64;
        let remaining = timeout_secs.saturating_sub(elapsed).max(1);

        // ~50 s remain (±5 for scheduling / timing slack).
        assert!(
            (45..=55).contains(&remaining),
            "expected ~50 s remaining, got {remaining}"
        );
    }

    /// When the checkpoint's timeout has already fully elapsed (started_at is
    /// longer ago than `timeout_secs`), the remaining time clamps to 1 s so
    /// the recovered task still gets a minimal window instead of 0.
    #[test]
    fn remaining_timeout_clamps_to_one_when_expired() {
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(200);
        let timeout_secs = 100u64;

        let elapsed = (chrono::Utc::now() - started_at)
            .num_seconds()
            .max(0) as u64;
        let remaining = timeout_secs.saturating_sub(elapsed).max(1);

        assert_eq!(remaining, 1, "expired checkpoint should clamp to 1 s");
    }

    /// Fix (2026-08-15, notice-queue starvation): the recovery turn holds
    /// `turn_lock` without going through `dispatch_turn_spawn`, so its end
    /// must run the same drain tail — a delegation notice enqueued while the
    /// recovery turn ran (route_notice saw a busy lock) must be drained when
    /// the turn ends, not left to starve the dispatch gate forever
    /// (2026-08-14 hot-switch incident: notices queued at 16:13:37/46 were
    /// never drained; subsequent user messages were silently queued).
    #[tokio::test]
    async fn recovery_turn_end_drains_queued_notices() {
        use super::super::test_support::MockChannel;

        let channel = MockChannel::new();
        let ctx = Arc::new(test_ctx(vec![(("mock".into(), "default".into()), channel)]));
        let key = SessionKey::new("mock", "default", "u1");
        let sk = key.to_string();
        // Active session (get_or_create auto-activates) with a materialized
        // context — mirrors a session mid-recovery at startup.
        let _ = ctx.sessions.get_or_create(&sk);
        let sctx = ctx.session_context_for(&sk);
        // Simulate the incident: a notice enqueued while the recovery turn
        // held `turn_lock` (silenced_override Some(false) = loud notice,
        // exactly what route_notice records with no pending delegations).
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "delegation:test-sub".to_string(),
            content: "[系统通知] 子代理已完成后台任务".to_string(),
            silenced_override: Some(false),
        });
        assert!(sctx.has_queued_delegation_notices());

        recover_active_session(
            Arc::clone(&ctx),
            key,
            crate::api::message::MessageReceiver::new("u1"),
        )
        .await;

        assert!(
            !sctx.has_queued_delegation_notices(),
            "recovery turn end must drain the notice queue"
        );
        assert!(
            sctx.take_delegation_notices().is_empty(),
            "drained queue must stay empty (notices went to notice turns)"
        );
    }
}
