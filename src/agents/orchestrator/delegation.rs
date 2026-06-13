//! Delegation wakes.
//!
//! A sub-agent completing (or failing) a background task is a *system* event,
//! not a user message. We synthesize a system-note `ChannelMessage` and drive
//! it straight into the parent session via [`inbound::dispatch_turn`], skipping
//! the user-message interceptors (ask-reply / callback / crash-recovery /
//! slash-command) that could never apply to a system note — and, deliberately,
//! the scheduler "last user message" bookkeeping that a real inbound would do.

use super::ctx::OrchestratorCtx;
use super::key::SessionKey;
use crate::agents::DelegationEvent;
use crate::channels::ChannelInboundMessage;

/// Wake the parent agent on a `DelegationEvent` (sub-agent completion/failure).
pub(super) async fn wake(ctx: &OrchestratorCtx, event: DelegationEvent) {
    let (task_id, parent_session_id, reply_target, content) = match event {
        DelegationEvent::Completed {
            task_id,
            parent_session_id,
            reply_target,
            summary,
            duration_secs,
        } => {
            tracing::info!(task_id = %task_id, duration_secs, "delegation completed, waking main agent");
            let content = format!(
                "[系统通知] 子代理已完成后台任务 (task_id: {}, 耗时: {}s)，结果如下：\n{}",
                task_id, duration_secs, summary
            );
            (task_id, parent_session_id, reply_target, content)
        }
        DelegationEvent::Failed {
            task_id,
            parent_session_id,
            reply_target,
            error,
        } => {
            tracing::warn!(task_id = %task_id, "delegation failed, waking main agent");
            let content = format!(
                "[系统通知] 子代理后台任务失败 (task_id: {})，错误：\n{}",
                task_id, error
            );
            (task_id, parent_session_id, reply_target, content)
        }
    };

    // The parent session key is `channel:account:sender`; routing the turn uses
    // it directly (no re-parsing out of a faked message field).
    let key = match SessionKey::parse(&parent_session_id) {
        Some(k) => k,
        None => {
            tracing::warn!(parent = %parent_session_id, "invalid session key in delegation event");
            return;
        }
    };
    if ctx.channel(&key.account_key()).is_none() {
        tracing::warn!(parent = %parent_session_id, "channel for delegation event not found");
        return;
    }

    let synthetic = ChannelInboundMessage {
        id: format!("delegation:{}", task_id),
        sender: crate::channels::MessageSender::new(key.sender.clone()),
        receiver: crate::channels::MessageReceiver::new(reply_target),
        content: crate::channels::ChannelMessageContent::text(content),
        timestamp: chrono::Utc::now().timestamp() as u64,
        interruption_scope_id: None,
    };
    super::inbound::dispatch_turn(ctx, &key, synthetic).await;
}
