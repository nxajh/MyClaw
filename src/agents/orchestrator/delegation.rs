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
use crate::agents::turn::SubStatus;
use crate::agents::{DelegationEvent, MessageKind, SessionContext};
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
const SILENCE_GUIDANCE: &str = "[系统提示] 本轮为中间恢复轮：任务尚未全部完成，你的本轮输出将作为进度说明展示给用户。你可以继续处理其他任务；若需要等待子代理结果，请调用 sessions_yield 结束当前轮——子代理完成时会自动唤醒你并把结果作为下一条消息注入。绝不轮询（不要反复查询子代理状态）。";

/// Append the silence guidance when the resume turn is not final — the
/// session still has pending delegations, so this turn's output is delivered
/// as a progress message, not the end of the turn.
fn maybe_append_silence_guidance(sctx: &SessionContext, content: &mut String) {
    if sctx.has_pending_delegations() {
        content.push_str("\n\n");
        content.push_str(SILENCE_GUIDANCE);
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
                 如需重试请重新委托，或先用 agent_list 确认无残留任务。",
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
            if let Some(snap) = sctx.record_terminal(
                sub_session_id.clone(),
                status,
                content.clone(),
                sent_message_count,
            ) {
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
        }
    }

    // Route the synthesized notice (terminal or message) into the session.
    route_notice(ctx, &parent_session_id, content, synthetic_id).await;
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
    // `has_pending_delegations()` equals `!snap.pending.is_empty()` of the
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
    let silenced_override = sctx_opt.as_ref().map(|s| s.has_pending_delegations());

    let routing_key = &session.owner;
    let is_active = ctx
        .sessions
        .active_session_id(routing_key)
        .is_some_and(|id| id == session_id);

    if is_active {
        // Active session — route through dispatch_turn so output streams live.
        let key = match SessionKey::parse(routing_key) {
            Some(k) => k,
            None => {
                tracing::warn!(routing_key = %routing_key, "invalid routing key in delegation event");
                return;
            }
        };
        if ctx.channel(&key.account_key()).is_none() {
            tracing::warn!(routing_key = %routing_key, "channel for delegation event not found, falling back to non-active path");
            process_non_active(ctx, session_id, &content, silenced_override).await;
            return;
        }

        // 单 preview (2026-08-12): this notice turn is now dispatched —
        // count it in-flight synchronously (same sync section as
        // `record_terminal`, no await in between, zero race window with the
        // origin turn's `clear_suspension_if_collected`). The RAII guard in
        // `process_turn` decrements it on exit.
        if let Some(sctx) = &sctx_opt {
            sctx.bump_notice_turn();
        }

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
        };
        super::inbound::dispatch_turn(ctx, &key, synthetic).await;
    } else {
        // Non-active session — load a temporary context, process the turn,
        // persist the result. The user sees it when they switch back.
        process_non_active(ctx, session_id, &content, silenced_override).await;
    }
}

/// Process a delegation event for a non-active session.
///
/// Loads a temporary `SessionContext` (not registered in the table), runs
/// `process_turn` with `channel=None`, and drops the context when done.
/// The LLM's response is persisted to history so the user sees it on
/// `/switch` return.
async fn process_non_active(
    ctx: &OrchestratorCtx,
    session_id: &str,
    content: &str,
    silenced_override: Option<bool>,
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
            id: format!("delegation:{}", uuid::Uuid::new_v4()),
            sender: crate::channels::MessageSender::new("system".to_string()),
            receiver: crate::channels::MessageReceiver::new(String::new()),
            content: crate::channels::ChannelMessageContent::text(content_owned),
            timestamp: chrono::Utc::now().timestamp() as u64,
            interruption_scope_id: None,
            silenced_override,
        };

        match session_ctx.process_turn(synthetic, None, runtime).await {
            Ok(_) => {
                tracing::info!(session_id = %session_id_owned, "non-active delegation turn completed");
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
    use super::super::test_support::test_ctx;
    use super::*;
    use crate::agents::{AgentMessage, SessionContext};
    use std::sync::Arc;

    /// A session registered in `ctx` with one pending task "t1".
    fn suspended_session(ctx: &OrchestratorCtx) -> Arc<SessionContext> {
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        sctx
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
        assert!(content.contains("子代理完成时会自动唤醒你并把结果作为下一条消息注入"));
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
        // derives the intent from `has_pending_delegations()` — t2 remains →
        // intermediate notice.
        let _ = sctx.record_terminal("t1".into(), SubStatus::Completed, "t1 done".into(), 0);
        let intent_t1 = Some(sctx.has_pending_delegations());
        assert_eq!(intent_t1, Some(true));

        // Race: t2's terminal lands BEFORE wake-1's turn runs — live pending
        // is now empty, but wake-1's intent (Some(true)) keeps it silenced.
        let _ = sctx.record_terminal("t2".into(), SubStatus::Completed, "t2 done".into(), 0);
        let live = sctx.suspension_snapshot();
        assert!(live.as_ref().unwrap().pending.is_empty());
        assert!(crate::agents::session_context::decide_silenced(intent_t1, live.clone()));

        // wake-2 (t2 terminal): final notice → loud summary.
        let intent_t2 = Some(sctx.has_pending_delegations());
        assert_eq!(intent_t2, Some(false));
        assert!(!crate::agents::session_context::decide_silenced(intent_t2, live));
    }
}
