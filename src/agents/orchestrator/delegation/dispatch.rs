use super::super::ctx::OrchestratorCtx;
use super::super::key::SessionKey;
use crate::agents::{DelegationNotice, SessionContext};
use crate::api::message::ChannelInboundMessage;

/// P1 (2026-08-13, RFC delegation-notice-queue §4): drain the session's
/// enqueued delegation notices. Triggered by `route_notice` when the
/// session is idle (immediate) and by the `dispatch_turn` tail (turn-end).
/// Dedupes by notice id within a pass; a single pass is bounded (notices
/// enqueued DURING the drain stay for the next pass, picked up by the
/// recursive turn-end drain — see `dispatch_turn_spawn`'s tail).
///
/// Issue #106, two rounds:
/// 1. (`b66f1f0`) Each notice's turn used to be fired via an independent
///    `tokio::spawn` with nothing serializing which order they'd actually
///    reach the shared `turn_lock` — arrival order depended on runtime
///    scheduling, not event order. Fixed by awaiting each notice's turn to
///    completion before dispatching the next.
/// 2. (this change) That fix delivered in order, but still cost one full
///    LLM turn *per notice* — a pass of N accumulated notices took N
///    sequential round-trips to fully deliver (observed: up to ~2min for
///    the last of 4). All notices for the *active* session are now merged
///    into ONE synthetic turn instead: one round-trip delivers the whole
///    backlog, in order, and the "one wake, one notice" annoyance the
///    issue's "expected behavior" section asked about is gone by
///    construction. The non-active fallback (`process_non_active`) is
///    intentionally left one-call-per-notice — it persists to history for
///    a switched-away session, not a live wait, so there is no user-facing
///    latency to fix there, and merging it would need its own separate
///    audit of that path's persistence semantics.
pub(crate) async fn drain_delegation_notices(ctx: &OrchestratorCtx, session_id: &str) {
    let session = match ctx.sessions.get_by_id(session_id) {
        Some(s) => s,
        None => {
            // P2 semantics (already how route_notice behaves): a vanished
            // session is the only dead-letter case — nothing to deliver to.
            tracing::warn!(session_id = %session_id, "session not found for delegation notice drain");
            return;
        }
    };
    // The queue lives on the REGISTERED context (the one `route_notice`
    // enqueued onto). A switched-away session has no registered context and
    // never goes through the queue (process_non_active path instead).
    let sctx = match ctx.sessions.registered_context_by_session_id(session_id) {
        Some(s) => s,
        None => {
            tracing::warn!(session_id = %session_id, "no registered context for delegation notice drain");
            return;
        }
    };
    let notices = sctx.take_delegation_notices();
    if notices.is_empty() {
        return;
    }

    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<DelegationNotice> = notices
        .into_iter()
        .filter(|n| {
            if seen.insert(n.id.clone()) {
                true
            } else {
                tracing::warn!(notice_id = %n.id, "duplicate delegation notice in drain pass; skipping");
                false
            }
        })
        .collect();
    if deduped.is_empty() {
        return;
    }
    tracing::info!(session_id = %session_id, count = deduped.len(), "draining delegation notices");

    let routing_key = &session.owner;
    let key = match SessionKey::parse(routing_key) {
        Some(k) => k,
        None => {
            tracing::warn!(routing_key = %routing_key, "invalid routing key in delegation notice drain");
            return;
        }
    };

    // Checked ONCE, before any dispatch: taking the queue and deduping above
    // has no await point, so activity cannot have changed since `notices`
    // was read. This is at least as safe as the old per-notice re-check —
    // that existed to guard against a session switch happening *during* a
    // previous notice's full turn (an await point); merging into one turn
    // removes all but the last of those await points, shrinking the race
    // window rather than widening it.
    let is_active = ctx
        .sessions
        .active_session_id(routing_key)
        .is_some_and(|id| id == session_id);

    if is_active && ctx.channel(&key.account_key()).is_some() {
        dispatch_notice_batch(ctx, &key, &session, &sctx, deduped).await;
    } else {
        // Session went inactive (or channel disappeared) — fall back to the
        // non-active path so each notice still lands in history. P2: pass
        // the persisted id so a successful non-active turn marks the stored
        // entry delivered — otherwise it would stay Pending and be
        // re-delivered after every restart.
        for notice in deduped {
            process_non_active(
                ctx,
                session_id,
                &key.sender,
                &notice.content,
                notice.silenced_override,
                Some(notice.id),
            )
            .await;
        }
    }
}

/// Split a notice batch's ids: the first becomes the synthetic message's
/// own `id` (what `process_turn` persists to history and what
/// `dispatch_turn_spawn` naturally marks delivered on success), the rest
/// ride along as `extra_notice_ids` so their own completion-queue entries
/// also get marked delivered — otherwise they'd stay Pending forever and
/// be redelivered on every restart despite having actually been delivered.
/// `batch` must be non-empty.
pub(super) fn split_batch_ids(batch: &[DelegationNotice]) -> (String, Vec<String>) {
    let primary = batch[0].id.clone();
    let extra = batch[1..].iter().map(|n| n.id.clone()).collect();
    (primary, extra)
}

/// Deliver a whole batch of queued delegation notices as ONE synthetic
/// turn (issue #106 round 2 — see `drain_delegation_notices` doc). `batch`
/// must be non-empty and already deduped/FIFO-ordered.
/// Reply receiver for a synthetic delegation notice turn (#144).
///
/// Prefers the receiver of the session's last message (preserves thread /
/// reply-to context); falls back to the session key's sender when `last_message`
/// is missing **or carries an empty receiver id** — an empty id would make
/// Telegram reject the fallback send with `chat_id is empty` and the notice
/// would be silently dropped. The empty-receiver filter also self-heals
/// sessions already polluted by the pre-#144 `record_inbound` overwrite.
pub(crate) fn notice_receiver(
    last_message: Option<&crate::api::message::PersistedChannelMessage>,
    sender: &str,
) -> crate::api::message::MessageReceiver {
    crate::api::message::MessageReceiver::new(
        last_message
            .map(|m| m.receiver.id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| sender.to_string()),
    )
}

/// Sender for the non-active notice path when no parsed `SessionKey` is at
/// hand (#144): `route_notice` only parses the key inside its active branch,
/// so the non-active fallback re-derives the sender from the raw routing key
/// (`channel:account:sender`). Unparsable keys degrade to `"system"` — the
/// pre-#144 behaviour (empty receiver) is never worse than that.
pub(crate) fn notice_fallback_sender(routing_key: &str) -> String {
    SessionKey::parse(routing_key)
        .map(|k| k.sender)
        .unwrap_or_else(|| "system".to_string())
}

async fn dispatch_notice_batch(
    ctx: &OrchestratorCtx,
    key: &SessionKey,
    session: &crate::agents::session::Session,
    sctx: &std::sync::Arc<SessionContext>,
    batch: Vec<DelegationNotice>,
) {
    // 单 preview (2026-08-12): count this AS ONE notice turn in-flight,
    // synchronously (same sync section as `record_terminal`, no await in
    // between) — the RAII guard in `process_turn` decrements on exit.
    sctx.bump_notice_turn();

    // silenced_override: each notice captured this at its own enqueue time
    // (route_notice), but delivering them together means only the state
    // AFTER the whole batch matters — recompute fresh rather than reuse any
    // individual notice's now-possibly-stale snapshot.
    let silenced_override = Some(sctx.has_pending_async_work());

    let (batch_id, extra_notice_ids) = split_batch_ids(&batch);
    let combined_content = batch
        .iter()
        .map(|n| n.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    tracing::debug!(
        session_id = %sctx.session_id,
        batch_size = batch.len(),
        "dispatching delegation notices as one merged turn"
    );

    let synthetic = ChannelInboundMessage {
        id: batch_id,
        sender: crate::api::message::MessageSender::new(key.sender.clone()),
        receiver: notice_receiver(session.last_message.as_ref(), &key.sender),
        content: crate::api::message::ChannelMessageContent::text(combined_content),
        timestamp: chrono::Utc::now().timestamp() as u64,
        interruption_scope_id: None,
        silenced_override,
        // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
        run_mode: Default::default(),
    };
    if let Some(handle) =
        super::super::inbound::dispatch_turn_spawn(ctx, key, synthetic, extra_notice_ids)
    {
        if let Err(e) = handle.await {
            tracing::warn!(
                session_id = %sctx.session_id,
                err = %e,
                "delegation notice batch turn task panicked"
            );
        }
    }
}

/// Process a delegation event for a non-active session.
///
/// Loads a temporary `SessionContext` (not registered in the table), runs
/// `process_turn` with `channel=None`, and drops the context when done.
/// The LLM's response is persisted to history so the user sees it on
/// `/switch` return.
///
/// P2 (2026-08-13, RFC delegation-notice-queue §5.3): when `notice_id` is
/// `Some`, the synthetic turn uses that id and a successful turn marks the
/// persisted entry delivered (at-least-once). `None` (wake for a non-active
/// session, recovery-synthesized notices) leaves the store untouched.
///
/// `sender` (#144): the routing key's sender id, used as the FALLBACK
/// receiver only — the session's persisted `last_message` receiver wins
/// when present (keeps the notice in the user's group/thread), mirroring
/// `route_notice` / `dispatch_notice_batch` / startup recovery semantics.
pub(crate) async fn process_non_active(
    ctx: &OrchestratorCtx,
    session_id: &str,
    sender: &str,
    content: &str,
    silenced_override: Option<bool>,
    notice_id: Option<String>,
) {
    let session_ctx = match ctx.sessions.load_context_by_session_id(session_id) {
        Some(c) => c,
        None => {
            tracing::warn!(session_id = %session_id, "cannot load context for non-active delegation event");
            return;
        }
    };

    // #144 review: prefer the session's persisted last-message receiver —
    // it keeps the notice in the same group/thread the user was actually
    // using (sender-as-receiver would reroute group chats to a private DM).
    // Only when it is absent or empty does the routing key's sender win.
    let receiver = {
        let session = session_ctx.session.lock().await;
        notice_receiver(session.last_message.as_ref(), sender)
    };

    let runtime = ctx.runtime.clone();
    let session_id_owned = session_id.to_string();
    let content_owned = content.to_string();
    let turn_tracker = ctx.turn_tracker.clone();
    // P2: keep the notice id + store handle for the post-turn delivery mark.
    let notice_id_owned = notice_id.clone();
    let completion_queue = ctx.completion_queue.clone();
    // issue #260: delegate handle for post-turn lifecycle reconciliation.
    let delegator = ctx.delegator.clone();
    let content_for_reconcile = content_owned.clone();

    // 单 preview (2026-08-12): any delegation notice (silenced or loud) is
    // part of the suspension sequence — count it in-flight before the spawn
    // (same sync section as `record_terminal` in the wake path; no await
    // between). The RAII guard in `process_turn` decrements on exit.
    if silenced_override.is_some() {
        session_ctx.bump_notice_turn();
    }

    tokio::spawn(async move {
        let _guard = turn_tracker.track();
        let synthetic = ChannelInboundMessage {
            id: notice_id.unwrap_or_else(|| format!("delegation:{}", uuid::Uuid::new_v4())),
            sender: crate::api::message::MessageSender::new("system".to_string()),
            receiver,
            content: crate::api::message::ChannelMessageContent::text(content_owned),
            timestamp: chrono::Utc::now().timestamp() as u64,
            interruption_scope_id: None,
            silenced_override,
            // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
            run_mode: Default::default(),
        };

        match session_ctx.process_turn(synthetic, None, runtime).await {
            Ok(_) => {
                tracing::info!(session_id = %session_id_owned, "non-active delegation turn completed");
                // issue #260: this turn may be the one that actually finished
                // the sub-agent's work (route_notice's non-active fallback).
                // Reconcile the delegation lifecycle so the in-flight entry
                // doesn't linger until the wall-clock timer fires and the
                // parent receives a spurious "timed out, use agent_resume"
                // for work that is already done.
                if let Some(d) = &delegator {
                    d.reconcile_notice_completed(&session_id_owned, &content_for_reconcile);
                }
                // P2 (RFC §5.3): content persisted to history — mark the
                // stored entry delivered (drop the file) so a restart does
                // not re-deliver it. On Err the entry stays Pending
                // (at-least-once). `Ok(false)` = id never persisted (wake
                // without a store / recovery notices) — no-op.
                if let (Some(store), Some(id)) = (&completion_queue, &notice_id_owned) {
                    match store.mark_delivered(id) {
                        Ok(true) => {
                            tracing::debug!(notice_id = %id, "completion queue: notice marked delivered");
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(
                                notice_id = %id,
                                err = %e,
                                "completion queue: mark delivered failed"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id_owned, err = %e, "non-active delegation turn failed");
            }
        }
    });
}
