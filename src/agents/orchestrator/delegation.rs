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
use crate::agents::{DelegationEvent, MessageKind};
use crate::channels::ChannelInboundMessage;

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
                .registered_context_by_session_id(&msg.session_id)
            {
                Some(registered) => {
                    registered.add_progress(&msg.task_id, &msg.text);
                    tracing::debug!(
                        task_id = %msg.task_id,
                        "progress report suppressed into suspension"
                    );
                }
                None => {
                    tracing::debug!(
                        task_id = %msg.task_id,
                        "progress report dropped (parent session not suspended)"
                    );
                }
            }
            return;
        }
    }

    // Resolve the event into (task, session, optional terminal status,
    // sent_message_count, synthesized content, unique synthetic id).
    // `status` is Some for terminal events (Completed/Failed/TimedOut) and
    // None for `Message{Final}` — only terminals enter the suspension's
    // `results` list.
    let (task_id, session_id, status, sent_message_count, mut content, synthetic_id) =
        match event {
        DelegationEvent::Completed {
            task_id,
            session_id,
            summary,
            duration_secs,
            sent_message_count,
        } => {
            tracing::info!(task_id = %task_id, duration_secs, sent_message_count, "delegation completed, waking main agent");
            // If the sub-agent already streamed its result to the parent via
            // `Message` events, the summary would duplicate what the parent
            // has seen — degrade the note to pure metadata (④).
            let content = if sent_message_count > 0 {
                format!(
                    "[系统通知] 子代理已完成后台任务 (task_id: {}, 耗时: {}s)。结果已通过子代理消息实时同步。",
                    task_id, duration_secs
                )
            } else {
                format!(
                    "[系统通知] 子代理已完成后台任务 (task_id: {}, 耗时: {}s)，结果如下：\n{}",
                    task_id, duration_secs, summary
                )
            };
            let synthetic_id = format!("delegation:{}", task_id);
            (
                task_id,
                session_id,
                Some(SubStatus::Completed),
                sent_message_count,
                content,
                synthetic_id,
            )
        }
        DelegationEvent::Failed {
            task_id,
            session_id,
            error,
        } => {
            tracing::warn!(task_id = %task_id, "delegation failed, waking main agent");
            let content = format!(
                "[系统通知] 子代理后台任务失败 (task_id: {})，错误：\n{}",
                task_id, error
            );
            let synthetic_id = format!("delegation:{}", task_id);
            (
                task_id,
                session_id,
                Some(SubStatus::Failed),
                0,
                content,
                synthetic_id,
            )
        }
        DelegationEvent::TimedOut {
            task_id,
            session_id,
            timeout_secs,
            duration_secs,
        } => {
            tracing::warn!(
                task_id = %task_id,
                timeout_secs,
                duration_secs,
                "delegation timed out, waking main agent"
            );
            let content = format!(
                "[系统通知] 子代理后台任务超时 (task_id: {}, 超时上限: {}s, 已运行: {}s)，任务已中止。\
                 如需重试请重新委托，或先用 agent_list 确认无残留任务。",
                task_id, timeout_secs, duration_secs
            );
            let synthetic_id = format!("delegation:{}", task_id);
            (
                task_id,
                session_id,
                Some(SubStatus::TimedOut),
                0,
                content,
                synthetic_id,
            )
        }
        // RFC agent-messaging §3.4: a sub-agent messaged its parent while
        // running in background. `task_id` is the sub-agent's own id
        // (identity — lets the parent reply via `recipient`), `session_id`
        // is the parent session to wake. Routed exactly like Completed /
        // Failed: queued behind the turn lock, never preempting. Not a
        // terminal event — never enters the suspension's results.
        DelegationEvent::Message(msg) => {
            tracing::info!(task_id = %msg.task_id, sender = %msg.sender_name, "sub-agent message, waking main agent");
            let content = format!(
                "[子代理消息] 来自子代理 '{}' (task_id: {}):\n{}",
                msg.sender_name, msg.task_id, msg.text
            );
            // Unique synthetic id per message (a task may emit many messages).
            let synthetic_id = format!("delegation-msg:{}", msg.msg_id);
            (msg.task_id, msg.session_id, None, 0, content, synthetic_id)
        }
    };

    // 方案 C (§3.2): terminal events update the suspension BEFORE routing —
    // move the task out of `pending` and append its `SubResult` (with folded
    // progress). Lookup is TWO-tier (P1-1): the registered context first
    // (active sessions), falling back to a temporary context for
    // switched-away sessions. A switched-away suspended session leaves
    // `suspension.json` on disk; the temp context restores it at load
    // (`SessionContext::new` → `restore_suspension`), collects the terminal
    // event and writes the updated state back — without this the pending
    // task would linger forever (no registered context exists to collect on).
    if let Some(status) = status {
        let sctx = ctx
            .sessions
            .registered_context_by_session_id(&session_id)
            .or_else(|| ctx.sessions.load_context_by_session_id(&session_id));
        if let Some(sctx) = sctx {
            if let Some(snap) = sctx.record_terminal(
                task_id.clone(),
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
                    .find(|r| r.task_id == task_id)
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
                    task_id = %task_id,
                    pending = snap.pending.len(),
                    collected = snap.results.len(),
                    "suspension updated on terminal event"
                );
            }
        }
    }

    // Route the synthesized notice (terminal or message) into the session.
    route_notice(ctx, &session_id, content, synthetic_id).await;
}

/// Route a synthesized system notice into the parent session:
/// active → `dispatch_turn` (live streaming to the user's UI); non-active →
/// `process_non_active` (temporary context, persisted to history). Shared by
/// `wake` (delegation events) and `recover_suspension` (P1-1 startup
/// recovery of persisted suspensions).
pub(super) async fn route_notice(
    ctx: &OrchestratorCtx,
    session_id: &str,
    content: String,
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
            process_non_active(ctx, session_id, &content).await;
            return;
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
        };
        super::inbound::dispatch_turn(ctx, &key, synthetic).await;
    } else {
        // Non-active session — load a temporary context, process the turn,
        // persist the result. The user sees it when they switch back.
        process_non_active(ctx, session_id, &content).await;
    }
}

/// Process a delegation event for a non-active session.
///
/// Loads a temporary `SessionContext` (not registered in the table), runs
/// `process_turn` with `channel=None`, and drops the context when done.
/// The LLM's response is persisted to history so the user sees it on
/// `/switch` return.
async fn process_non_active(ctx: &OrchestratorCtx, session_id: &str, content: &str) {
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

    tokio::spawn(async move {
        let _guard = turn_tracker.track();
        let synthetic = ChannelInboundMessage {
            id: format!("delegation:{}", uuid::Uuid::new_v4()),
            sender: crate::channels::MessageSender::new("system".to_string()),
            receiver: crate::channels::MessageReceiver::new(String::new()),
            content: crate::channels::ChannelMessageContent::text(content_owned),
            timestamp: chrono::Utc::now().timestamp() as u64,
            interruption_scope_id: None,
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
                task_id: "t1".to_string(),
                session_id: sid,
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
                task_id: "t1".to_string(),
                session_id: sid,
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
                task_id: "t1".to_string(),
                session_id: sid,
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
                task_id: "t1".to_string(),
                session_id: sid,
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
                task_id: "t1".to_string(),
                session_id: sid,
                text: "working on it".to_string(),
                kind: MessageKind::Progress,
            }),
        )
        .await;
        let snap = sctx.suspension_snapshot().unwrap();
        assert!(snap.results.is_empty());
        assert_eq!(snap.pending, vec!["t1".to_string()]);
        assert_eq!(
            snap.progress_by_task.get("t1").unwrap(),
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
                task_id: "t1".to_string(),
                session_id: sid.clone(),
                text: "working on it".to_string(),
                kind: MessageKind::Progress,
            }),
        )
        .await;
        wake(
            &ctx,
            DelegationEvent::Completed {
                task_id: "t1".to_string(),
                session_id: sid,
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
                task_id: "ghost".to_string(),
                session_id: "no-such-session".to_string(),
                summary: "x".to_string(),
                duration_secs: 0,
                sent_message_count: 0,
            },
        )
        .await;
        wake(
            &ctx,
            DelegationEvent::Failed {
                task_id: "ghost".to_string(),
                session_id: "no-such-session".to_string(),
                error: "x".to_string(),
            },
        )
        .await;
        // no panic; nothing further to assert
    }
}
