use super::super::ctx::OrchestratorCtx;
use super::super::key::SessionKey;
use super::dispatch::{
    drain_delegation_notices, notice_fallback_sender, notice_receiver, process_non_active,
};
use crate::agents::session_context::TerminalRecord;
use crate::agents::turn::SubStatus;
use crate::agents::{DelegationEvent, MessageKind, SessionContext};
use crate::api::message::ChannelInboundMessage;

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
pub(super) fn maybe_append_silence_guidance(sctx: &SessionContext, content: &mut String) {
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
pub(crate) struct NoticeMeta {
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
pub(crate) async fn wake(ctx: &OrchestratorCtx, event: DelegationEvent) {
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
pub(crate) async fn route_shell_completion(
    ctx: &OrchestratorCtx,
    sc: crate::tools::shell::ShellCompletion,
) {
    let synthetic_id = format!("shell:{}", sc.process_id);
    // issue #140: SubStatus only distinguishes Completed/Failed/TimedOut —
    // a shell process's exit_code collapses onto the first two (there's no
    // shell-side equivalent of a sub-agent wall-clock timeout kill; a
    // foreground call that outran timeout_secs was NOT killed, so it always
    // eventually exits one way or the other).
    //
    // `exit_code: None` (review finding on #142, xiaoer-bot) is the adopted-
    // orphan reaper's case — we aren't its real parent, so we can only
    // observe that it's gone, never how it exited (see `spawn_adopted_reaper`
    // / `build_completion_content`'s "exit_code: null"). Mapping that to
    // Failed contradicted the notice's own content ("后台命令已完成") —
    // self-contradictory metadata (status says failed, text says completed).
    // Completed is the better default: the process DID reach a terminal
    // state, we just can't say how. `Some(n) if n != 0` stays Failed — that
    // one's unambiguous.
    let status = match sc.exit_code {
        Some(0) | None => SubStatus::Completed,
        Some(_) => SubStatus::Failed,
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
pub(crate) async fn route_notice(
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
            process_non_active(ctx, session_id, &key.sender, &content, silenced_override, None).await;
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
                let entry = crate::agents::orchestrator::CompletionNoticeEntry {
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
                    delivery_state: crate::agents::orchestrator::DeliveryState::Pending,
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
                sender: crate::api::message::MessageSender::new(key.sender.clone()),
                receiver: notice_receiver(session.last_message.as_ref(), &key.sender),
                content: crate::api::message::ChannelMessageContent::text(content),
                timestamp: chrono::Utc::now().timestamp() as u64,
                interruption_scope_id: None,
                silenced_override,
                // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
                run_mode: Default::default(),
            };
            super::super::inbound::dispatch_turn(ctx, &key, synthetic).await;
        }
    } else {
        // Non-active session — load a temporary context, process the turn,
        // persist the result. The user sees it when they switch back.
        // P2: not persisted here (no queue to enter; the wake→turn window is
        // small and RFC §5 keeps this path in-memory-only) — `notice_id`
        // stays None so the store is untouched.
        process_non_active(ctx, session_id, &notice_fallback_sender(routing_key), &content, silenced_override, None).await;
    }
}
