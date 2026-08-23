//! Delegation wakes.
//!
//! A sub-agent completing (or failing) a background task is a *system* event,
//! not a user message. We synthesize a system-note `ChannelInboundMessage` and
//! drive it into the parent session.
//!
//! ## Routing
//!
//! The `DelegationEvent.session_id` is a hex session ID, NOT a routing key.
//! We look up the session to find its `owner` (the routing key
//! `channel:account:sender`), then:
//!
//! - **Active session** — route through `inbound::dispatch_turn` so the turn
//!   streams to the user's UI in real time.
//! - **Non-active session** — load a temporary `SessionContext` (not
//!   registered in the table) and call `process_turn` directly with
//!   `channel=None`. The LLM processes the result and the response is
//!   persisted to history; the user sees it when they switch back.

use super::ctx::OrchestratorCtx;
use super::key::SessionKey;
use crate::agents::session_context::TerminalRecord;
use crate::agents::turn::SubStatus;
use crate::agents::{DelegationEvent, DelegationNotice, MessageKind, SessionContext};
use crate::channels::ChannelInboundMessage;

/// 方案 C (§3.3, 2026-08-10): appended to every resume-turn notice while
/// the session still has pending delegations — that turn's output is
/// delivered as an ordinary *intermediate* message (progress commentary, not
/// a turn end), so the model must not draft a final summary yet. Borrowed
/// from Claude Code's fork-async pre-announcement ("results will arrive in a
/// subsequent message"): telling the model the turn isn't final keeps it
/// from committing to a conclusion before all results land.
///
/// 架构修正 (2026-08-12): also tells the model it may keep working after
/// spawning a delegation — the sub-agent completion wakes it with the result
/// injected, so it must not emit a final conclusion while results are
/// outstanding.
///
/// 异步委派通知改造 (2026-08-13, docs/delegation-notice-queue-rfc.md §3.3):
/// deleted the "结果未齐时不得输出最终结论" hard rule — it was the deadlock
/// root cause (the model never ends the turn, so completion notices queue
/// behind `turn_lock` forever). Replaced with `sessions_yield` guidance:
/// the model may legally end the turn (yield or natural EndTurn) and the
/// completion arrives as the next message. "绝不轮询" borrowed verbatim
/// from openclaw `system-prompt.ts` L124 ("never poll").
/// issue #140: wording generalized from "子代理结果" to "子代理/后台任务结果"
/// — `has_pending_async_work()` (checked below) now also covers armed
/// shell background processes, so this guidance fires for shell-only
/// pending too, not just sub-agent delegations.
const SILENCE_GUIDANCE: &str = "[系统提示] 本轮为中间恢复轮：任务尚未全部完成，你的本轮输出将作为进度说明展示给用户。你可以继续处理其他任务；若需要等待子代理/后台任务结果，请调用 sessions_yield 结束当前轮——完成时会自动唤醒你并把结果作为下一条消息注入。绝不轮询（不要反复查询状态）。";

/// Append the silence guidance when the resume turn is not final — the
/// session still has pending async work (delegations and/or shell
/// background processes, issue #140), so this turn's output is delivered
/// as a progress message, not the end of the turn.
fn maybe_append_silence_guidance(sctx: &SessionContext, content: &mut String) {
    if sctx.has_pending_async_work() {
        content.push_str("\n\n");
        content.push_str(SILENCE_GUIDANCE);
    }
}

/// P2 (2026-08-13, RFC delegation-notice-queue §5.2): metadata carried from
/// `wake` into `route_notice` for persistence. Terminal events carry a
/// terminal status; sub-agent `Message` events carry `status: None` (the RFC
/// entry allows null). `recover_suspension`'s recovery-synthesized notice
/// passes `None` for the whole struct — it is never persisted.
#[derive(Debug, Clone)]
pub(super) struct NoticeMeta {
    pub sub_session_id: String,
    pub status: Option<SubStatus>,
    pub sent_message_count: u64,
}

/// P2 (RFC §5.2): lowercase terminal status stored in persisted entries.
/// Hand-written — serde's default variant naming would produce PascalCase.
fn substatus_str(s: SubStatus) -> &'static str {
    match s {
        SubStatus::Completed => "completed",
        SubStatus::Failed => "failed",
        SubStatus::TimedOut => "timed_out",
    }
}

/// Wake the parent agent on a `DelegationEvent` (sub-agent completion/failure/message).
pub(super) async fn wake(ctx: &OrchestratorCtx, event: DelegationEvent) {
    // 方案 C (turn-suspension RFC §2.3): `Message{kind: Progress}` is never
    // injected into the parent context — suspended sessions fold it into the
    // task's `progress` list (surfaced with the terminal result); non-suspended
    // sessions drop it. Either way we do NOT wake the parent.
    if let DelegationEvent::Message(msg) = &event {
        if msg.kind == MessageKind::Progress {
            match ctx
                .sessions
                .registered_context_by_session_id(&msg.parent_session_id)
            {
                Some(registered) => {
                    registered.add_progress(&msg.sub_session_id, &msg.text);
                    tracing::debug!(
                        sub_session_id = %msg.sub_session_id,
                        "progress report suppressed into suspension"
                    );
                }
                None => {
                    tracing::debug!(
                        sub_session_id = %msg.sub_session_id,
                        "progress report dropped (parent session not suspended)"
                    );
                }
            }
            return;
        }
    }

    // Resolve the event into (sub_session_id, parent_session_id, optional
    // terminal status, sent_message_count, synthesized content, unique
    // synthetic id). `status` is Some for terminal events
    // (Completed/Failed/TimedOut) and None for `Message{Final}` — only
    // terminals enter the suspension's `results` list.
    let (
        sub_session_id,
        parent_session_id,
        status,
        sent_message_count,
        mut content,
        synthetic_id,
    ) = match event {
        DelegationEvent::Completed {
            sub_session_id,
            parent_session_id,
            summary,
            duration_secs,
            sent_message_count,
        } => {
            tracing::info!(sub_session_id = %sub_session_id, duration_secs, sent_message_count, "delegation completed, waking main agent");
            // If the sub-agent already streamed its result to the parent via
            // `Message` events, the summary would duplicate what the parent
            // has seen — degrade the note to pure metadata (④).
            let content = if sent_message_count > 0 {
                format!(
                    "[系统通知] 子代理已完成后台任务 (session_id: {}, 耗时: {}s)。结果已通过子代理消息实时同步。",
                    sub_session_id, duration_secs
                )
            } else {
                format!(
                    "[系统通知] 子代理已完成后台任务 (session_id: {}, 耗时: {}s)，结果如下：\n{}",
                    sub_session_id, duration_secs, summary
                )
            };
            let synthetic_id = format!("delegation:{}", sub_session_id);
            (
                sub_session_id.clone(),
                parent_session_id,
                Some(SubStatus::Completed),
                sent_message_count,
                content,
                synthetic_id,
            )
        }
        DelegationEvent::Failed {
            sub_session_id,
            parent_session_id,
            error,
        } => {
            tracing::warn!(sub_session_id = %sub_session_id, "delegation failed, waking main agent");
            let content = format!(
                "[系统通知] 子代理后台任务失败 (session_id: {})，错误：\n{}",
                sub_session_id, error
            );
            let synthetic_id = format!("delegation:{}", sub_session_id);
            (
                sub_session_id.clone(),
                parent_session_id,
                Some(SubStatus::Failed),
                0,
                content,
                synthetic_id,
            )
        }
        DelegationEvent::TimedOut {
            sub_session_id,
            parent_session_id,
            timeout_secs,
            duration_secs,
        } => {
            tracing::warn!(
                sub_session_id = %sub_session_id,
                timeout_secs,
                duration_secs,
                "delegation timed out, waking main agent"
            );
            let content = format!(
                "[系统通知] 子代理后台任务超时 (session_id: {}, 超时上限: {}s, 已运行: {}s)，任务已中止。\
                 子会话的已完成工作已保留：如需继续该任务，用 agent_resume 工具以其 session_id 恢复\
                 （会获得新的时间预算并从断点继续）；如需重做，重新委托即可。",
                sub_session_id, timeout_secs, duration_secs
            );
            let synthetic_id = format!("delegation:{}", sub_session_id);
            (
                sub_session_id.clone(),
                parent_session_id,
                Some(SubStatus::TimedOut),
                0,
                content,
                synthetic_id,
            )
        }
        // RFC agent-messaging §3.4: a sub-agent messaged its parent while
        // running in background. `sub_session_id` is the sub-agent's own id
        // (identity — lets the parent reply via `recipient`),
        // `parent_session_id` is the session to wake. Routed exactly like
        // Completed / Failed: queued behind the turn lock, never preempting.
        // Not a terminal event — never enters the suspension's results.
        DelegationEvent::Message(msg) => {
            tracing::info!(sub_session_id = %msg.sub_session_id, sender = %msg.sender_name, "sub-agent message, waking main agent");
            let content = format!(
                "[子代理消息] 来自子代理 '{}' (session_id: {}):\n{}",
                msg.sender_name, msg.sub_session_id, msg.text
            );
            // Unique synthetic id per message (a task may emit many messages).
            let synthetic_id = format!("delegation-msg:{}", msg.msg_id);
            (
                msg.sub_session_id,
                msg.parent_session_id,
                None,
                0,
                content,
                synthetic_id,
            )
        }
    };

    // 方案 C (§3.2): terminal events update the suspension BEFORE routing —
    // move the sub-session out of `pending` and append its `SubResult` (with
    // folded progress). Lookup is TWO-tier (P1-1): the registered context
    // first (active sessions), falling back to a temporary context for
    // switched-away sessions. A switched-away suspended session leaves
    // `suspension.json` on disk; the temp context restores it at load
    // (`SessionContext::new` → `restore_suspension`), collects the terminal
    // event and writes the updated state back — without this the pending
    // sub-session would linger forever (no registered context exists to
    // collect on).
    if let Some(status) = status {
        let sctx = ctx
            .sessions
            .registered_context_by_session_id(&parent_session_id)
            .or_else(|| ctx.sessions.load_context_by_session_id(&parent_session_id));
        if let Some(sctx) = sctx {
            // 方案4 (terminal-event idempotency): only route a notice on first
            // collection (`Recorded`). A `Duplicate` hit means the same
            // sub_session_id was already collected (e.g. recover_suspension
            // injected a failure before the sub-agent's natural terminal
            // arrived); `NoSuspension` means no active suspension — in both
            // cases the notice must NOT be re-sent.
            match sctx.record_terminal(
                sub_session_id.clone(),
                status,
                content.clone(),
                sent_message_count,
            ) {
                TerminalRecord::Recorded(snap) => {
                    // RFC §2.3: suppressed progress reports surface as part of
                    // the result entry — append them to the injected content so
                    // the parent sees the task's interim reports together with
                    // its terminal note (they never enter the context otherwise).
                    if let Some(lines) = snap
                        .results
                        .iter()
                        .rev()
                        .find(|r| r.sub_session_id == sub_session_id)
                        .map(|r| r.progress.as_slice())
                        .filter(|p| !p.is_empty())
                    {
                        let mut enriched = content;
                        enriched.push_str("\n\n任务过程记录：\n");
                        for line in lines {
                            enriched.push_str(&format!("- {}\n", line));
                        }
                        content = enriched;
                    }
                    tracing::info!(
                        sub_session_id = %sub_session_id,
                        pending = snap.pending.len(),
                        collected = snap.results.len(),
                        "suspension updated on terminal event"
                    );
                }
                TerminalRecord::Duplicate | TerminalRecord::NoSuspension => {
                    tracing::debug!(
                        sub_session_id = %sub_session_id,
                        "terminal event already recorded, skipping duplicate notice"
                    );
                    return;
                }
            }
        }
    }

    // Route the synthesized notice (terminal or message) into the session.
    // P2 (RFC §5.2): carry the metadata for persistence — terminal and
    // `Message` events both persist (Message with `status: None`); the
    // recovery-synthesized notice passes `None` instead (not persisted).
    let notice_meta = NoticeMeta {
        sub_session_id: sub_session_id.clone(),
        status,
        sent_message_count,
    };
    route_notice(
        ctx,
        &parent_session_id,
        content,
        synthetic_id,
        Some(notice_meta),
    )
    .await;
}

/// Route a shell command's completion (issue #129) into the session that
/// spawned it. Reuses `route_notice` wholesale — same persistence
/// (`completion_queue`, at-least-once across a restart), same batching of
/// concurrent completions into one turn (`drain_delegation_notices`).
///
/// issue #140: also mirrors `wake`'s suspension bookkeeping — `process_id`
/// fills the `sub_session_id` slot in `record_terminal`/`TurnSuspension`
/// exactly as it already did in the persisted `completion_queue` entry
/// (nothing downstream treats it as an actual sub-session, just an opaque
/// id). This is what makes `has_pending_async_work()` (and therefore
/// silenced/fold/drain/`set_preview`) automatically cover shell background
/// work too, once `ShellTool::register_pending` has called `add_pending_task`
/// for it — the two sides of the same suspension the shell tool call itself
/// registered into.
///
/// Unlike `wake`, a `NoSuspension` result does NOT drop the notice: a shell
/// background process can run far longer than a delegation's bounded
/// timeout, so by completion time the session's `SessionContext` may have
/// been reloaded (RFC §三.A reload semantics) since `register_pending` ran —
/// a legitimate race, not a bug. Falling through preserves #129's original
/// guarantee (every notify-armed process gets a notice) for that case; it
/// only loses the pending-suspension treatment for this one notice.
pub(super) async fn route_shell_completion(
    ctx: &OrchestratorCtx,
    sc: crate::tools::shell::ShellCompletion,
) {
    let synthetic_id = format!("shell:{}", sc.process_id);
    // issue #140: SubStatus only distinguishes Completed/Failed/TimedOut —
    // a shell process's exit_code collapses onto the first two (there's no
    // shell-side equivalent of a sub-agent wall-clock timeout kill; a
    // foreground call that outran timeout_secs was NOT killed, so it always
    // eventually exits one way or the other).
    let status = if sc.exit_code == Some(0) {
        SubStatus::Completed
    } else {
        SubStatus::Failed
    };

    let sctx = ctx
        .sessions
        .registered_context_by_session_id(&sc.session_id)
        .or_else(|| ctx.sessions.load_context_by_session_id(&sc.session_id));
    if let Some(sctx) = &sctx {
        match sctx.record_terminal(sc.process_id.clone(), status, sc.content.clone(), 0) {
            TerminalRecord::Recorded(_) => {}
            TerminalRecord::Duplicate => {
                tracing::debug!(
                    process_id = %sc.process_id,
                    "shell completion already recorded, skipping duplicate notice"
                );
                return;
            }
            TerminalRecord::NoSuspension => {
                tracing::debug!(
                    process_id = %sc.process_id,
                    "shell completion has no suspension to collect (session reloaded while running, or registration never reached it) — routing as an ordinary notice"
                );
            }
        }
    }

    let notice_meta = NoticeMeta {
        sub_session_id: sc.process_id,
        status: Some(status),
        sent_message_count: 0,
    };
    route_notice(ctx, &sc.session_id, sc.content, synthetic_id, Some(notice_meta)).await;
}

/// Route a synthesized system notice into the parent session:
/// active → `dispatch_turn` (live streaming to the user's UI); non-active →
/// `process_non_active` (temporary context, persisted to history). Shared by
/// `wake` (delegation events) and `recover_suspension` (P1-1 startup
/// recovery of persisted suspensions).
///
/// `silenced_override` (fix v2): wake-time silence intent captured in this
/// sync section (see below). The turn is silenced only while pending
/// delegations remain — the model's output is then delivered as an ordinary
/// intermediate message and the turn does NOT end.
pub(super) async fn route_notice(
    ctx: &OrchestratorCtx,
    session_id: &str,
    mut content: String,
    synthetic_id: String,
    notice_meta: Option<NoticeMeta>,
) {
    // Resolve the session to get its routing key (owner).
    let session = match ctx.sessions.get_by_id(session_id) {
        Some(s) => s,
        None => {
            tracing::warn!(session_id = %session_id, "session not found for delegation event");
            return;
        }
    };

    // 方案 C (§3.3): a resume turn with pending delegations is *silenced* —
    // attach the guidance so the model never assumes the user has seen a
    // draft summary (Claude Code-style pre-announcement; no backfill).
    //
    // Race fix (2026-08-10, E2E 恢复轮1): the silence INTENT is captured HERE
    // (wake/route time — same sync section right after `record_terminal`, so
    // `has_pending_async_work()` equals `!snap.pending.is_empty()` of the
    // just-collected terminal), NOT at turn start: a queued notice may run
    // after later terminal events cleared `pending`, and the live snapshot
    // would wrongly mark the intermediate notice loud (it streamed as a
    // normal message instead of the progress preview). The intent rides the
    // synthetic `ChannelInboundMessage.silenced_override` into `process_turn`.
    let sctx_opt = ctx
        .sessions
        .registered_context_by_session_id(session_id)
        .or_else(|| ctx.sessions.load_context_by_session_id(session_id));
    if let Some(sctx) = &sctx_opt {
        maybe_append_silence_guidance(sctx, &mut content);
    }
    let silenced_override = sctx_opt.as_ref().map(|s| s.has_pending_async_work());

    let routing_key = &session.owner;
    let is_active = ctx
        .sessions
        .active_session_id(routing_key)
        .is_some_and(|id| id == session_id);

    if is_active {
        // Active session — route through the notice queue (P1,
        // docs/delegation-notice-queue-rfc.md §4). The synthetic turn is
        // built by `drain_delegation_notices`; this function only enqueues
        // and decides whether an immediate drain is safe.
        let key = match SessionKey::parse(routing_key) {
            Some(k) => k,
            None => {
                tracing::warn!(routing_key = %routing_key, "invalid routing key in delegation event");
                return;
            }
        };
        if ctx.channel(&key.account_key()).is_none() {
            tracing::warn!(routing_key = %routing_key, "channel for delegation event not found, falling back to non-active path");
            process_non_active(ctx, session_id, &content, silenced_override, None).await;
            return;
        }

        // Enqueue BEFORE the idle check: the queue must be non-empty by the
        // time the drain (or the turn-end drain trigger) reads it.
        if let Some(sctx) = &sctx_opt {
            // P2 (2026-08-13, RFC delegation-notice-queue §5.2): persist the
            // notice BEFORE it enters the in-memory queue — a crash after
            // enqueue but before the turn persists its content must not lose
            // the notice (startup recovery re-enqueues Pending entries).
            // Fail-open: a storage error still delivers now via the queue
            // (degraded to P1 for this notice; it just won't survive a
            // restart). Dedup: the store's `seen` set returns `None` on a
            // duplicate id — no double file.
            if let (Some(store), Some(meta)) = (&ctx.completion_queue, &notice_meta) {
                let entry = crate::storage::CompletionNoticeEntry {
                    seq: 0,
                    id: synthetic_id.clone(),
                    sub_session_id: meta.sub_session_id.clone(),
                    parent_session_id: session_id.to_string(),
                    status: meta.status.map(|s| substatus_str(s).to_string()),
                    content: content.clone(),
                    silenced_override,
                    // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
                    sent_message_count: meta.sent_message_count,
                    enqueued_at: chrono::Utc::now().timestamp() as u64,
                    delivery_state: crate::storage::DeliveryState::Pending,
                };
                if let Err(e) = store.append(entry) {
                    tracing::warn!(
                        notice_id = %synthetic_id,
                        err = %e,
                        "completion queue: persist failed; delivering in-memory only"
                    );
                }
            }
            sctx.enqueue_delegation_notice(crate::agents::DelegationNotice {
                id: synthetic_id,
                content,
                silenced_override,
            });

            // Idle check: tokio Mutex::try_lock succeeds ONLY when the lock
            // is free AND has no waiters (FIFO fairness) — so a success
            // means the drain runs immediately (equivalently: dispatch_turn
            // today). A busy lock means a user/notice turn is running (or
            // waiting); the turn-end drain trigger in `dispatch_turn` picks
            // the queue up — no spawn, no lock contention here.
            if sctx.turn_lock.try_lock().is_ok() {
                tracing::debug!(
                    session_id = %session_id,
                    "session idle; draining delegation notices immediately"
                );
                let drain_ctx = ctx.clone();
                let drain_sid = session_id.to_string();
                tokio::spawn(async move {
                    drain_delegation_notices(&drain_ctx, &drain_sid).await;
                });
            } else {
                tracing::debug!(
                    session_id = %session_id,
                    "session busy; delegation notice queued for turn-end drain"
                );
            }
        } else {
            // No materialized context (should not happen for an active
            // session) — fall back to the pre-P1 direct dispatch so the
            // notice is not silently lost. No bump (no queue to track it);
            // matches the old no-bump path.
            let synthetic = ChannelInboundMessage {
                id: synthetic_id,
                sender: crate::channels::MessageSender::new(key.sender.clone()),
                receiver: crate::channels::MessageReceiver::new(
                    session
                        .last_message
                        .as_ref()
                        .map(|m| m.receiver.id.clone())
                        .unwrap_or_default(),
                ),
                content: crate::channels::ChannelMessageContent::text(content),
                timestamp: chrono::Utc::now().timestamp() as u64,
                interruption_scope_id: None,
                silenced_override,
                // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
                run_mode: Default::default(),
            };
            super::inbound::dispatch_turn(ctx, &key, synthetic).await;
        }
    } else {
        // Non-active session — load a temporary context, process the turn,
        // persist the result. The user sees it when they switch back.
        // P2: not persisted here (no queue to enter; the wake→turn window is
        // small and RFC §5 keeps this path in-memory-only) — `notice_id`
        // stays None so the store is untouched.
        process_non_active(ctx, session_id, &content, silenced_override, None).await;
    }
}

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
pub(super) async fn drain_delegation_notices(ctx: &OrchestratorCtx, session_id: &str) {
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
fn split_batch_ids(batch: &[DelegationNotice]) -> (String, Vec<String>) {
    let primary = batch[0].id.clone();
    let extra = batch[1..].iter().map(|n| n.id.clone()).collect();
    (primary, extra)
}

/// Deliver a whole batch of queued delegation notices as ONE synthetic
/// turn (issue #106 round 2 — see `drain_delegation_notices` doc). `batch`
/// must be non-empty and already deduped/FIFO-ordered.
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
        sender: crate::channels::MessageSender::new(key.sender.clone()),
        receiver: crate::channels::MessageReceiver::new(
            session
                .last_message
                .as_ref()
                .map(|m| m.receiver.id.clone())
                .unwrap_or_default(),
        ),
        content: crate::channels::ChannelMessageContent::text(combined_content),
        timestamp: chrono::Utc::now().timestamp() as u64,
        interruption_scope_id: None,
        silenced_override,
        // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
        run_mode: Default::default(),
    };
    if let Some(handle) =
        super::inbound::dispatch_turn_spawn(ctx, key, synthetic, extra_notice_ids)
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
pub(super) async fn process_non_active(
    ctx: &OrchestratorCtx,
    session_id: &str,
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

    let runtime = ctx.runtime.clone();
    let session_id_owned = session_id.to_string();
    let content_owned = content.to_string();
    let turn_tracker = ctx.turn_tracker.clone();
    // P2: keep the notice id + store handle for the post-turn delivery mark.
    let notice_id_owned = notice_id.clone();
    let completion_queue = ctx.completion_queue.clone();

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
            sender: crate::channels::MessageSender::new("system".to_string()),
            receiver: crate::channels::MessageReceiver::new(String::new()),
            content: crate::channels::ChannelMessageContent::text(content_owned),
            timestamp: chrono::Utc::now().timestamp() as u64,
            interruption_scope_id: None,
            silenced_override,
            // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
            run_mode: Default::default(),
        };

        match session_ctx.process_turn(synthetic, None, runtime).await {
            Ok(_) => {
                tracing::info!(session_id = %session_id_owned, "non-active delegation turn completed");
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

/// P1-4: `wake` routing tests — terminal events collect into the parent's
/// suspension (progress folding, degraded summary), Progress messages are
/// suppressed, and unknown sessions are a no-op. `test_ctx` has no channels,
/// so `route_notice` falls back to the spawned non-active path (NullRegistry
/// fails fast) and never blocks the assertions.
#[cfg(test)]
mod tests {
    use super::super::test_support::{test_ctx, MockChannel};
    use super::*;
    use crate::agents::{AgentMessage, DelegationNotice, SessionContext};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    /// A session registered in `ctx` with one pending task "t1".
    fn suspended_session(ctx: &OrchestratorCtx) -> Arc<SessionContext> {
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        sctx
    }

    /// P2 (2026-08-13, RFC delegation-notice-queue §5.2): a wake for an
    /// ACTIVE session (auto-activated by `get_or_create_context`) with a
    /// channel present persists the notice BEFORE it enters the queue. The
    /// test runtime's NullRegistry makes the notice turn fail, so the entry
    /// stays Pending — exactly the at-least-once state a crash would leave
    /// (recovery re-enqueues it on the next start).
    #[tokio::test]
    async fn active_wake_persists_notice_before_enqueue() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::storage::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
        );
        let channel: Arc<dyn crate::channels::Channel> = MockChannel::new();
        let mut ctx = test_ctx(vec![(("mock".to_string(), "default".to_string()), channel)]);
        ctx.completion_queue = Some(Arc::clone(&store));
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid.clone(),
                summary: "persisted summary".to_string(),
                duration_secs: 5,
                sent_message_count: 0,
            },
        )
        .await;
        let pending = store.pending();
        assert_eq!(pending.len(), 1, "notice must be persisted before enqueue");
        let e = &pending[0];
        assert_eq!(e.id, "delegation:t1");
        assert_eq!(e.sub_session_id, "t1");
        assert_eq!(e.parent_session_id, sid);
        assert_eq!(e.status.as_deref(), Some("completed"));
        assert_eq!(e.sent_message_count, 0);
        // The just-collected terminal cleared `pending` — wake-time silence
        // intent is false (see route_notice race-fix comment).
        assert_eq!(e.silenced_override, Some(false));
        assert!(e.content.contains("persisted summary"));
        assert_eq!(e.delivery_state, crate::storage::DeliveryState::Pending);
    }

    /// issue #129/#140: `route_shell_completion` reuses `route_notice`
    /// wholesale for a shell background completion — this session never
    /// called `add_pending_task` for this process_id, so `record_terminal`
    /// resolves `NoSuspension` and the notice still routes/persists as an
    /// ordinary notice (falls through, does not drop it — see the function's
    /// doc comment). Persists to `completion_queue` the same way a
    /// delegation notice does, with the shell command's `process_id` riding
    /// in the `sub_session_id` slot as an opaque id (not a real sub-session).
    #[tokio::test]
    async fn route_shell_completion_persists_before_enqueue() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::storage::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
        );
        let channel: Arc<dyn crate::channels::Channel> = MockChannel::new();
        let mut ctx = test_ctx(vec![(("mock".to_string(), "default".to_string()), channel)]);
        ctx.completion_queue = Some(Arc::clone(&store));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        let sid = sctx.session_id.clone();

        route_shell_completion(
            &ctx,
            crate::tools::shell::ShellCompletion {
                session_id: sid.clone(),
                process_id: "sh_abc123".to_string(),
                content: "[系统通知] 后台命令已完成 (process_id: sh_abc123, exit_code: 0)。"
                    .to_string(),
                exit_code: Some(0),
            },
        )
        .await;

        let pending = store.pending();
        assert_eq!(pending.len(), 1, "shell completion must persist before enqueue");
        let e = &pending[0];
        assert_eq!(e.id, "shell:sh_abc123");
        assert_eq!(e.sub_session_id, "sh_abc123");
        assert_eq!(e.parent_session_id, sid);
        assert_eq!(e.status, Some("completed".to_string()));
        assert!(e.content.contains("sh_abc123"));
    }

    /// issue #140: the core value of pending-unification — two shell
    /// background processes registered as pending on the SAME session
    /// (mirrors `ShellTool::register_pending`'s `add_pending_task` calls).
    /// The first completion, with the second still pending, must route as a
    /// SILENCED intermediate notice (wake-time intent captured synchronously
    /// right after `record_terminal`, exactly like a delegation terminal
    /// event); the second (last pending item) must be loud/final.
    #[tokio::test]
    async fn concurrent_shell_completions_silence_intermediate_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::storage::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
        );
        let channel: Arc<dyn crate::channels::Channel> = MockChannel::new();
        let mut ctx = test_ctx(vec![(("mock".to_string(), "default".to_string()), channel)]);
        ctx.completion_queue = Some(Arc::clone(&store));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        let sid = sctx.session_id.clone();
        sctx.add_pending_task("sh_a".to_string());
        sctx.add_pending_task("sh_b".to_string());

        route_shell_completion(
            &ctx,
            crate::tools::shell::ShellCompletion {
                session_id: sid.clone(),
                process_id: "sh_a".to_string(),
                content: "a done".to_string(),
                exit_code: Some(0),
            },
        )
        .await;

        let after_a = store.pending();
        let a_entry = after_a
            .iter()
            .find(|e| e.id == "shell:sh_a")
            .expect("sh_a notice persisted");
        assert_eq!(
            a_entry.silenced_override,
            Some(true),
            "sh_b still pending — sh_a's notice must be silenced (intermediate)"
        );
        assert_eq!(sctx.suspension_snapshot().unwrap().pending, vec!["sh_b".to_string()]);

        route_shell_completion(
            &ctx,
            crate::tools::shell::ShellCompletion {
                session_id: sid,
                process_id: "sh_b".to_string(),
                content: "b done".to_string(),
                exit_code: Some(1),
            },
        )
        .await;

        let after_b = store.pending();
        let b_entry = after_b
            .iter()
            .find(|e| e.id == "shell:sh_b")
            .expect("sh_b notice persisted");
        assert_eq!(
            b_entry.silenced_override,
            Some(false),
            "sh_b is the last pending item — its notice must be loud (final)"
        );
        assert_eq!(b_entry.status, Some("failed".to_string()));
        assert!(sctx.suspension_snapshot().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn completed_collects_result_with_summary() {
        let ctx = test_ctx(vec![]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                summary: "the final summary".to_string(),
                duration_secs: 7,
                sent_message_count: 0,
            },
        )
        .await;
        let snap = sctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        let r = &snap.results[0];
        assert_eq!(r.status, SubStatus::Completed);
        assert!(r.content.contains("the final summary"));
        assert_eq!(r.sent_message_count, 0);
        assert!(snap.pending.is_empty());
    }

    #[tokio::test]
    async fn completed_with_messages_degrades_summary() {
        let ctx = test_ctx(vec![]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                summary: "duplicate summary".to_string(),
                duration_secs: 3,
                sent_message_count: 3,
            },
        )
        .await;
        let snap = sctx.suspension_snapshot().unwrap();
        let r = &snap.results[0];
        assert!(!r.content.contains("duplicate summary"));
        assert!(r.content.contains("实时同步"));
        assert_eq!(r.sent_message_count, 3);
    }

    #[tokio::test]
    async fn failed_collects_error() {
        let ctx = test_ctx(vec![]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::Failed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                error: "provider exploded".to_string(),
            },
        )
        .await;
        let snap = sctx.suspension_snapshot().unwrap();
        let r = &snap.results[0];
        assert_eq!(r.status, SubStatus::Failed);
        assert!(r.content.contains("provider exploded"));
    }

    #[tokio::test]
    async fn timed_out_collects_timeout_note() {
        let ctx = test_ctx(vec![]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::TimedOut {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                timeout_secs: 600,
                duration_secs: 600,
            },
        )
        .await;
        let snap = sctx.suspension_snapshot().unwrap();
        let r = &snap.results[0];
        assert_eq!(r.status, SubStatus::TimedOut);
        assert!(r.content.contains("超时"));
    }

    #[tokio::test]
    async fn progress_message_is_suppressed_not_waking() {
        let ctx = test_ctx(vec![]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::Message(AgentMessage {
                msg_id: "m1".to_string(),
                sender_name: "coder".to_string(),
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                text: "working on it".to_string(),
                kind: MessageKind::Progress,
            }),
        )
        .await;
        let snap = sctx.suspension_snapshot().unwrap();
        assert!(snap.results.is_empty());
        assert_eq!(snap.pending, vec!["t1".to_string()]);
        assert_eq!(
            snap.progress_by_sub_session.get("t1").unwrap(),
            &vec!["working on it".to_string()]
        );
    }

    #[tokio::test]
    async fn progress_folds_into_terminal_result() {
        let ctx = test_ctx(vec![]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::Message(AgentMessage {
                msg_id: "m1".to_string(),
                sender_name: "coder".to_string(),
                sub_session_id: "t1".to_string(),
                parent_session_id: sid.clone(),
                text: "working on it".to_string(),
                kind: MessageKind::Progress,
            }),
        )
        .await;
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                summary: "done".to_string(),
                duration_secs: 5,
                sent_message_count: 0,
            },
        )
        .await;
        let snap = sctx.suspension_snapshot().unwrap();
        let r = &snap.results[0];
        assert_eq!(r.progress, vec!["working on it".to_string()]);
        assert!(r.content.contains("done"));
        assert!(snap.pending.is_empty());
    }

    #[tokio::test]
    async fn unknown_session_event_is_a_noop() {
        let ctx = test_ctx(vec![]);
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "ghost".to_string(),
                parent_session_id: "no-such-session".to_string(),
                summary: "x".to_string(),
                duration_secs: 0,
                sent_message_count: 0,
            },
        )
        .await;
        wake(
            &ctx,
            DelegationEvent::Failed {
                sub_session_id: "ghost".to_string(),
                parent_session_id: "no-such-session".to_string(),
                error: "x".to_string(),
            },
        )
        .await;
        // no panic; nothing further to assert
    }

    #[tokio::test]
    async fn silence_guidance_appended_while_pending_remains() {
        let ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        sctx.add_pending_task("t2".to_string());
        let mut content = "[系统通知] 子代理已完成后台任务 (session_id: t1)".to_string();
        maybe_append_silence_guidance(&sctx, &mut content);
        assert!(content.contains("中间恢复轮"));
        // 异步委派通知改造 (2026-08-13): 删掉"不会终结/不得输出最终结论"
        // 硬约束(死锁根源), 改为 sessions_yield 引导 + 绝不轮询.
        assert!(content.contains("将作为进度说明展示给用户"));
        assert!(content.contains("sessions_yield"));
        assert!(content.contains("结束当前轮"));
        assert!(content.contains("完成时会自动唤醒你并把结果作为下一条消息注入"));
        assert!(content.contains("绝不轮询"));
        assert!(!content.contains("不得输出最终结论"));
        assert!(!content.contains("不会终结"));
    }

    #[tokio::test]
    async fn silence_guidance_omitted_when_last_terminal_lands() {
        let ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        // The terminal event already collected (pending empty) — this resume
        // turn is the loud final summary, no guidance.
        let _ = sctx.record_terminal(
            "t1".to_string(),
            SubStatus::Completed,
            "done".to_string(),
            0,
        );
        let mut content = "[系统通知] 子代理已完成后台任务 (session_id: t1)".to_string();
        maybe_append_silence_guidance(&sctx, &mut content);
        assert_eq!(content, "[系统通知] 子代理已完成后台任务 (session_id: t1)");
    }

    #[tokio::test]
    async fn silence_guidance_omitted_when_not_suspended() {
        let ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        let mut content = "[系统通知] 子代理已完成后台任务 (session_id: t1)".to_string();
        maybe_append_silence_guidance(&sctx, &mut content);
        assert_eq!(content, "[系统通知] 子代理已完成后台任务 (session_id: t1)");
    }

    /// Race fix (E2E 恢复轮1): the silenced intent is captured at
    /// wake/route time (`record_terminal` → `route_notice`, same sync
    /// section) — the value is what `route_notice` puts on the synthetic
    /// `ChannelInboundMessage.silenced_override`. A queued notice may start
    /// after later terminals cleared `pending`; the override keeps the
    /// intermediate notice silenced even though the live snapshot at turn
    /// start would be empty.
    #[tokio::test]
    async fn silence_intent_captured_at_wake_time_survives_late_collection() {
        let ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        sctx.add_pending_task("t2".to_string());

        // wake-1 (t1 terminal): record_terminal runs first, then route_notice
        // derives the intent from `has_pending_async_work()` — t2 remains →
        // intermediate notice.
        let _ = sctx.record_terminal("t1".into(), SubStatus::Completed, "t1 done".into(), 0);
        let intent_t1 = Some(sctx.has_pending_async_work());
        assert_eq!(intent_t1, Some(true));

        // Race: t2's terminal lands BEFORE wake-1's turn runs — live pending
        // is now empty, but wake-1's intent (Some(true)) keeps it silenced.
        let _ = sctx.record_terminal("t2".into(), SubStatus::Completed, "t2 done".into(), 0);
        let live = sctx.suspension_snapshot();
        assert!(live.as_ref().unwrap().pending.is_empty());
        assert!(crate::agents::session_context::decide_silenced(intent_t1, live.clone()));

        // wake-2 (t2 terminal): final notice → loud summary.
        let intent_t2 = Some(sctx.has_pending_async_work());
        assert_eq!(intent_t2, Some(false));
        assert!(!crate::agents::session_context::decide_silenced(intent_t2, live));
    }

    // ---- P1 (2026-08-13, RFC delegation-notice-queue §4): 内存投递队列 ----

    #[tokio::test]
    async fn notice_queue_fifo_roundtrip() {
        let ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "n1".to_string(),
            content: "one".to_string(),
            silenced_override: Some(false),
        });
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "n2".to_string(),
            content: "two".to_string(),
            silenced_override: None,
        });
        assert!(sctx.has_queued_delegation_notices());
        // P1: `has_notice_turns_in_flight` covers the undrained queue too.
        assert!(sctx.has_notice_turns_in_flight());
        let taken = sctx.take_delegation_notices();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].id, "n1");
        assert_eq!(taken[1].id, "n2");
        assert!(!sctx.has_queued_delegation_notices());
        assert!(!sctx.has_notice_turns_in_flight());
    }

    #[tokio::test]
    async fn busy_turn_lock_holds_notices_for_turn_end_drain() {
        let channel = MockChannel::new();
        let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        // Simulate a running turn: hold `turn_lock` so the wake's idle check
        // fails.
        let _busy = sctx.turn_lock.lock().await;
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                summary: "done".to_string(),
                duration_secs: 3,
                sent_message_count: 0,
            },
        )
        .await;
        // Enqueued, NOT dispatched — no immediate drain while busy.
        assert!(sctx.has_queued_delegation_notices());
        assert_eq!(sctx.notice_turns_in_flight.load(Ordering::SeqCst), 0);
        assert!(channel.sent.lock().unwrap().is_empty());
        drop(_busy);
    }

    #[tokio::test]
    async fn idle_wake_drains_immediately() {
        let channel = MockChannel::new();
        let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
        let sctx = suspended_session(&ctx);
        let sid = sctx.session_id.clone();
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                summary: "done".to_string(),
                duration_secs: 3,
                sent_message_count: 0,
            },
        )
        .await;
        // wake spawned the drain; give it a moment to take + dispatch.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!sctx.has_queued_delegation_notices());
        assert_eq!(sctx.notice_turns_in_flight.load(Ordering::SeqCst), 0);
        // The notice turn ran (NullRegistry → error notice lands on channel).
        assert!(!channel.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn drain_merges_all_notices_into_one_fifo_ordered_turn() {
        let channel = MockChannel::new();
        let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
        let sctx = suspended_session(&ctx);
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "n1".to_string(),
            content: "first".to_string(),
            silenced_override: Some(false),
        });
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "n2".to_string(),
            content: "second".to_string(),
            silenced_override: Some(false),
        });
        drain_delegation_notices(&ctx, &sctx.session_id).await;
        // `drain_delegation_notices` awaits the single batched turn's
        // dispatch `JoinHandle` before returning, so by the time this
        // `.await` resolves the turn has genuinely finished, not just been
        // scheduled.
        assert!(!sctx.has_queued_delegation_notices());
        assert_eq!(sctx.notice_turns_in_flight.load(Ordering::SeqCst), 0);
        // Issue #106 round 2: both notices are delivered as ONE turn, not
        // two — it fails on NullRegistry and sends exactly one error
        // message (not one per notice).
        assert_eq!(channel.sent.lock().unwrap().len(), 1);
        // The merged synthetic message (persisted to history before the
        // provider call, even though the call itself then fails) preserves
        // FIFO order: "first" appears before "second".
        let session = sctx.session_snapshot().await;
        let merged = session
            .history
            .iter()
            .find(|m| m.role == "user" && m.text_content().contains("first"))
            .unwrap_or_else(|| panic!("no merged notice message found in history: {:?}", session.history));
        let text = merged.text_content();
        assert!(text.contains("first"), "got: {text}");
        assert!(text.contains("second"), "got: {text}");
        assert!(
            text.find("first") < text.find("second"),
            "expected FIFO order (first before second), got: {text}"
        );
    }

    #[tokio::test]
    async fn drain_dedupes_duplicate_notice_ids() {
        let channel = MockChannel::new();
        let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
        let sctx = suspended_session(&ctx);
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "dup".to_string(),
            content: "first".to_string(),
            silenced_override: Some(false),
        });
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "dup".to_string(),
            content: "second".to_string(),
            silenced_override: Some(false),
        });
        drain_delegation_notices(&ctx, &sctx.session_id).await;
        // drain only awaits take+spawn; the notice turn runs in background.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!sctx.has_queued_delegation_notices());
        // Only one notice dispatched (dedup) — its failed turn sends one
        // user-facing error message (issue #113).
        assert_eq!(channel.sent.lock().unwrap().len(), 1);
    }

    fn notice(id: &str) -> DelegationNotice {
        DelegationNotice {
            id: id.to_string(),
            content: String::new(),
            silenced_override: None,
        }
    }

    /// issue #106 round 2: every id in a batch beyond the first must ride
    /// along as `extra_notice_ids` — this is what feeds
    /// `dispatch_turn_spawn`'s mark-delivered loop; a bug here means a
    /// notice was actually delivered (it's in the merged turn content) but
    /// its completion-queue entry stays Pending forever, redelivering it on
    /// every daemon restart despite the user having already seen it.
    #[test]
    fn split_batch_ids_first_is_primary_rest_are_extra() {
        let batch = vec![notice("a"), notice("b"), notice("c")];
        let (primary, extra) = split_batch_ids(&batch);
        assert_eq!(primary, "a");
        assert_eq!(extra, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn split_batch_ids_single_notice_has_no_extras() {
        let batch = vec![notice("only")];
        let (primary, extra) = split_batch_ids(&batch);
        assert_eq!(primary, "only");
        assert!(extra.is_empty());
    }

    #[tokio::test]
    async fn undrained_notices_keep_suspension_alive() {
        let ctx = test_ctx(vec![]);
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        // Wake burst collected the terminal...
        let _ = sctx.record_terminal(
            "t1".to_string(),
            SubStatus::Completed,
            "done".to_string(),
            0,
        );
        // ...but a notice is still waiting for a drain — suspension survives.
        sctx.enqueue_delegation_notice(DelegationNotice {
            id: "n1".to_string(),
            content: "notice".to_string(),
            silenced_override: Some(false),
        });
        sctx.clear_suspension_if_collected();
        assert!(sctx.suspension_snapshot().is_some());
        // Drain completes the sequence — next clear drops the suspension.
        let _ = sctx.take_delegation_notices();
        sctx.clear_suspension_if_collected();
        assert!(sctx.suspension_snapshot().is_none());
    }

    /// 方案4: a duplicate terminal event (same sub_session_id sent twice via
    /// `wake`) must only deliver ONE notice. The first `record_terminal`
    /// returns `Recorded` → route_notice fires; the second returns `Duplicate`
    /// → `wake` returns early without routing. A second pending task keeps the
    /// suspension alive so the second call hits the idempotent guard rather
    /// than `NoSuspension`.
    #[tokio::test]
    async fn wake_dedupes_duplicate_terminal_event() {
        let channel = MockChannel::new();
        let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
        let sctx = suspended_session(&ctx);
        // Second pending task keeps the suspension alive after t1 is collected.
        sctx.add_pending_task("t2".to_string());
        let sid = sctx.session_id.clone();

        // First wake: record_terminal → Recorded → notice routed.
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid.clone(),
                summary: "done".to_string(),
                duration_secs: 1,
                sent_message_count: 0,
            },
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let count_after_first = channel.sent.lock().unwrap().len();
        assert!(
            count_after_first > 0,
            "first wake should deliver a notice"
        );

        // Second wake (same t1): record_terminal → Duplicate → no notice.
        wake(
            &ctx,
            DelegationEvent::Completed {
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                summary: "done again".to_string(),
                duration_secs: 1,
                sent_message_count: 0,
            },
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let count_after_second = channel.sent.lock().unwrap().len();
        assert_eq!(
            count_after_second, count_after_first,
            "duplicate wake must not deliver another notice"
        );
    }
}
