use super::super::ctx::OrchestratorCtx;
use super::super::key::SessionKey;
use super::dispatch::{notice_fallback_sender, notice_receiver, process_non_active};
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
    //
    // issue #240 (item 3): two parallel renderings of the same event —
    // `content` (Chinese) for the paths that still use message-injection
    // semantics (`process_non_active`, the no-materialized-context and
    // channel-not-found fallbacks in `route_notice`), and `queue_content`
    // (English) for the #238 queue path, which ends up as a `sessions_yield`
    // tool_result the model reads directly and paraphrases back to the user
    // in their own language on its own next turn — same convention #237
    // already established for `latest_entry_summary` (proc_entry.rs).
    let (
        sub_session_id,
        parent_session_id,
        status,
        sent_message_count,
        mut content,
        mut queue_content,
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
            let (content, queue_content) = if sent_message_count > 0 {
                (
                    format!(
                        "[系统通知] 子代理已完成后台任务 (session_id: {}, 耗时: {}s)。结果已通过子代理消息实时同步。",
                        sub_session_id, duration_secs
                    ),
                    format!(
                        "[system notice] The sub-agent finished its background task (session_id: {}, duration: {}s). The result was already streamed via sub-agent messages.",
                        sub_session_id, duration_secs
                    ),
                )
            } else {
                (
                    format!(
                        "[系统通知] 子代理已完成后台任务 (session_id: {}, 耗时: {}s)，结果如下：\n{}",
                        sub_session_id, duration_secs, summary
                    ),
                    format!(
                        "[system notice] The sub-agent finished its background task (session_id: {}, duration: {}s). Result:\n{}",
                        sub_session_id, duration_secs, summary
                    ),
                )
            };
            let synthetic_id = format!("delegation:{}", sub_session_id);
            (
                sub_session_id.clone(),
                parent_session_id,
                Some(SubStatus::Completed),
                sent_message_count,
                content,
                queue_content,
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
            let queue_content = format!(
                "[system notice] The sub-agent's background task failed (session_id: {}). Error:\n{}",
                sub_session_id, error
            );
            let synthetic_id = format!("delegation:{}", sub_session_id);
            (
                sub_session_id.clone(),
                parent_session_id,
                Some(SubStatus::Failed),
                0,
                content,
                queue_content,
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
            let queue_content = format!(
                "[system notice] The sub-agent's background task timed out (session_id: {}, timeout: {}s, ran: {}s) and was aborted. \
                 The sub-session's completed work is preserved: to continue this task, use the agent_resume tool with its session_id \
                 (it gets a fresh time budget and continues from where it left off); to redo it instead, just delegate again.",
                sub_session_id, timeout_secs, duration_secs
            );
            let synthetic_id = format!("delegation:{}", sub_session_id);
            (
                sub_session_id.clone(),
                parent_session_id,
                Some(SubStatus::TimedOut),
                0,
                content,
                queue_content,
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
            let queue_content = format!(
                "[sub-agent message] From sub-agent '{}' (session_id: {}):\n{}",
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
                queue_content,
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
                "delegation_notice",
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
                        let mut enriched_queue = queue_content;
                        enriched_queue.push_str("\n\nTask progress log:\n");
                        for line in lines {
                            enriched.push_str(&format!("- {}\n", line));
                            enriched_queue.push_str(&format!("- {}\n", line));
                        }
                        content = enriched;
                        queue_content = enriched_queue;
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
    route_notice(ctx, &parent_session_id, content, queue_content, synthetic_id).await;
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
        match sctx.record_terminal(
            sc.process_id.clone(),
            status,
            sc.content.clone(),
            0,
            "shell_completion",
        ) {
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

    // issue #240 (item 3): shell completion content (`proc_entry.rs`'s
    // `build_completion_content`) is out of scope for this translation pass
    // — it stays Chinese on every path for now, so the same string serves as
    // both the legacy-path and queue-path content here.
    route_notice(ctx, &sc.session_id, sc.content.clone(), sc.content, synthetic_id).await;
}

/// Route a synthesized system notice into the parent session:
/// active → the pending-yield queue when the session has a live
/// `SessionContext` (issue #238: delivered as the tool_result of whatever
/// `sessions_yield` is currently pending, batched with anything else queued
/// — never a synthesized `[user]` message; issue #240 item 3: this path
/// gets `queue_content`, in English per #237's "structured content fed to
/// the model" convention); a couple of narrower fallbacks (no live context,
/// or a channel we can't resolve) still use the older message-injection
/// style, and so does the non-active path (see `process_non_active`) — all
/// three of those keep using `content` (whatever language the caller built
/// it in; delegation notices build it in Chinese, matching the pre-#238
/// behavior they still preserve). Shared by `wake` (delegation events),
/// `route_shell_completion`, and `recover_suspension` (P1-1 startup
/// recovery of persisted suspensions) — the latter two currently pass the
/// same string for both parameters (neither is in scope for translation
/// yet).
pub(crate) async fn route_notice(
    ctx: &OrchestratorCtx,
    session_id: &str,
    mut content: String,
    queue_content: String,
    synthetic_id: String,
) {
    // Resolve the session to get its routing key (owner).
    let session = match ctx.sessions.get_by_id(session_id) {
        Some(s) => s,
        None => {
            tracing::warn!(session_id = %session_id, "session not found for delegation event");
            return;
        }
    };

    // issue #238: `sctx_opt` used to also drive the silenced/SILENCE_GUIDANCE
    // machinery for the active-session path — that's gone now (see below),
    // it's kept only for the two paths that still use the old message-
    // injection style: the "no materialized context" fallback right below
    // and the non-active `process_non_active` path at the bottom of this
    // function, neither of which this issue's redesign touches.
    let sctx_opt = ctx
        .sessions
        .registered_context_by_session_id(session_id)
        .or_else(|| ctx.sessions.load_context_by_session_id(session_id));

    let routing_key = &session.owner;
    let is_active = ctx
        .sessions
        .active_session_id(routing_key)
        .is_some_and(|id| id == session_id);

    if is_active {
        let key = match SessionKey::parse(routing_key) {
            Some(k) => k,
            None => {
                tracing::warn!(routing_key = %routing_key, "invalid routing key in delegation event");
                return;
            }
        };
        if ctx.channel(&key.account_key()).is_none() {
            let mut fallback_content = content.clone();
            if let Some(sctx) = &sctx_opt {
                maybe_append_silence_guidance(sctx, &mut fallback_content);
            }
            let silenced_override = sctx_opt.as_ref().map(|s| s.has_pending_async_work());
            tracing::warn!(routing_key = %routing_key, "channel for delegation event not found, falling back to non-active path");
            process_non_active(ctx, session_id, &key.sender, &fallback_content, silenced_override, None).await;
            return;
        }

        // issue #238: this event no longer gets wrapped into a synthesized
        // `[user]` message — it's queued for whenever this session next has
        // a pending `sessions_yield` tool_call to deliver it to (any
        // currently-open one, or the next one that appears), and delivered
        // as ITS tool_result. See `Session::pending_yield` /
        // `SessionContext::try_fill_pending_yield`. This sidesteps the
        // Gemini tool→user adjacency problem and the whole SILENCE_GUIDANCE
        // apparatus entirely for this path — there's no ambiguous "is this a
        // new turn" question to resolve, it's unambiguously a continuation
        // of the same still-open tool round.
        //
        // #242 (issue #240 item 1) closed the restart-persistence gap this
        // comment used to disclose: the queue is now persisted per-session
        // and `Session::pending_yield` is reconstructed from
        // `identify_breakpoint` on load, so an event queued here survives a
        // daemon restart before it's delivered.
        if let Some(sctx) = &sctx_opt {
            sctx.enqueue_pending_yield_event(crate::agents::session_context::PendingYieldEvent {
                content: queue_content,
            });
            let fill_ctx = ctx.clone();
            let fill_sid = session_id.to_string();
            tokio::spawn(async move {
                if let Some(sctx) = fill_ctx.sessions.registered_context_by_session_id(&fill_sid) {
                    sctx.try_fill_pending_yield(fill_ctx.runtime.clone()).await;
                }
            });
        } else {
            // No materialized context (should not happen for an active
            // session) — fall back to the pre-P1 direct dispatch so the
            // notice is not silently lost. No bump (no queue to track it);
            // matches the old no-bump path. This is the one remaining
            // active-session path still using message injection — it has
            // no SessionContext to hang a pending_yield off of.
            super::super::inbound::dispatch_turn(
                ctx,
                &key,
                ChannelInboundMessage {
                    id: synthetic_id,
                    sender: crate::api::message::MessageSender::new(key.sender.clone()),
                    receiver: notice_receiver(session.last_message.as_ref(), &key.sender),
                    content: crate::api::message::ChannelMessageContent::text(content),
                    timestamp: chrono::Utc::now().timestamp() as u64,
                    interruption_scope_id: None,
                    silenced_override: None,
                    // RFC channel-role-split: delegation wake/notice turns are Interactive (a user may resume).
                    run_mode: Default::default(),
                },
            )
            .await;
        }
    } else {
        // Non-active session — load a temporary context, process the turn,
        // persist the result. The user sees it when they switch back.
        // P2: not persisted here (no queue to enter; the wake→turn window is
        // small and RFC §5 keeps this path in-memory-only) — `notice_id`
        // stays None so the store is untouched.
        //
        // issue #238: unchanged — a non-active session has no live turn to
        // hang a pending_yield off of, so it keeps the old message-injection
        // style for now (still needs the silenced/SILENCE_GUIDANCE
        // treatment this function's active-session path no longer uses).
        if let Some(sctx) = &sctx_opt {
            maybe_append_silence_guidance(sctx, &mut content);
        }
        let silenced_override = sctx_opt.as_ref().map(|s| s.has_pending_async_work());
        process_non_active(ctx, session_id, &notice_fallback_sender(routing_key), &content, silenced_override, None).await;
    }
}
