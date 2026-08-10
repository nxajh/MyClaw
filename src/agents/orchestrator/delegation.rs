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
    let (task_id, session_id, status, sent_message_count, content, synthetic_id) = match event {
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
    // progress). Lookup by hex session id works for both active (registered
    // context) and switched-away sessions; a session with no suspension
    // (or no registered context) simply skips — behavior unchanged.
    if let Some(status) = status {
        if let Some(registered) = ctx
            .sessions
            .registered_context_by_session_id(&session_id)
        {
            if let Some(snap) = registered.record_terminal(
                task_id.clone(),
                status,
                content.clone(),
                sent_message_count,
            ) {
                tracing::info!(
                    task_id = %task_id,
                    pending = snap.pending.len(),
                    collected = snap.results.len(),
                    "suspension updated on terminal event"
                );
            }
        }
    }

    // Resolve the session to get its routing key (owner).
    let session = match ctx.sessions.get_by_id(&session_id) {
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
            process_non_active(ctx, &session_id, &content).await;
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
        process_non_active(ctx, &session_id, &content).await;
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
