//! Inbound dispatch — the interceptor chain for user messages.
//!
//! A user message flows through an ordered chain of [`Interceptor`]s. Each one
//! either consumes the message ([`Flow::Stop`]) or passes it on, possibly
//! rewritten ([`Flow::Next`]). The terminal [`DispatchTurn`] always stops the
//! chain by spawning the actual turn.
//!
//! Order: ask-reply → callback (retry/abort) → crash-recovery prompt →
//! slash-command → dispatch-turn. The chain is the single source of truth for
//! "what happens to an inbound message before it reaches `process_turn`".

use std::sync::Arc;

use async_trait::async_trait;

use super::ctx::OrchestratorCtx;
use super::key::SessionKey;
use crate::agents::commands;
use crate::agents::user_messages::{
    BTN_ABORT, BTN_RETRY, MSG_ABORT_ACK, MSG_INCOMPLETE_TURN, MSG_NO_PENDING_RETRY, MSG_TURN_FAILED,
};
use crate::channels::{
    CallbackAction, Channel, ChannelMessage, InlineButton, MessagePayload, SendTarget,
};
use tracing::error;

/// Outcome of an interceptor.
#[allow(clippy::large_enum_variant)]
enum Flow {
    /// Message handled (replied to and/or spawned). Stop the chain.
    Stop,
    /// Pass the (possibly rewritten) message to the next interceptor.
    Next(ChannelMessage),
}

#[async_trait]
trait Interceptor: Send + Sync {
    /// Stable identifier — used by the chain-order test.
    #[cfg_attr(not(test), allow(dead_code))]
    fn name(&self) -> &'static str;
    async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) -> Flow;
}

/// The ordered interceptor chain. Terminal stage (`DispatchTurn`) must be last.
fn chain() -> [&'static dyn Interceptor; 5] {
    [&AskReply, &Callback, &CrashRecovery, &SlashCommand, &DispatchTurn]
}

/// Run the inbound chain for one user message.
pub(super) async fn dispatch(
    ctx: &OrchestratorCtx,
    account: (String, String),
    msg: ChannelMessage,
) {
    // Tell the Scheduler about this user message so cron / heartbeat jobs with
    // `target = "last"` know where to deliver their output. No-op when the
    // scheduler is disabled. This is a user-message-only side effect, so it
    // lives in the runner, not in the chain (delegation wakes skip it).
    if let Some(ref scheduler) = ctx.scheduler {
        let channel_key = format!("{}:{}", account.0, account.1);
        scheduler.record_user_message(&channel_key, &msg.reply_target).await;
    }

    let key = SessionKey::new(&account.0, &account.1, &msg.sender);
    let mut msg = msg;
    for stage in chain() {
        match stage.handle(ctx, &key, msg).await {
            Flow::Stop => return,
            Flow::Next(m) => msg = m,
        }
    }
}

// ── AskReply ────────────────────────────────────────────────────────────────

/// RFC v2 §三.B: check the AskRouter first (indexed by session.id, registered
/// by AskUserTool). If it fulfilled an outstanding ask, the inbound message is
/// consumed and no fresh turn is spawned.
struct AskReply;

#[async_trait]
impl Interceptor for AskReply {
    fn name(&self) -> &'static str {
        "ask_reply"
    }
    async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) -> Flow {
        let session_id = ctx.session_manager.get_or_create(&key.to_string()).id.clone();
        if ctx.ask_router.fulfill(&session_id, msg.clone()) {
            tracing::debug!(session = %session_id, "ask_router fulfilled pending ask, consuming inbound");
            Flow::Stop
        } else {
            Flow::Next(msg)
        }
    }
}

// ── Callback (retry / abort) ──────────────────────────────────────────────────

/// Handle a retry/abort callback from an EmptyResponse prompt (RFC §11 Phase 5
/// structured `CallbackAction`). For retry, extract the saved text and rewrite
/// `msg.content`, then fall through to the standard dispatch. For abort, clear
/// `pending_retry` and ack inline.
struct Callback;

#[async_trait]
impl Interceptor for Callback {
    fn name(&self) -> &'static str {
        "callback"
    }
    async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, mut msg: ChannelMessage) -> Flow {
        let is_retry = match CallbackAction::parse(&msg.content) {
            Some(CallbackAction::Retry { .. }) => true,
            Some(CallbackAction::Abort { .. }) => false,
            _ => return Flow::Next(msg),
        };

        let reply_target = msg.reply_target.clone();
        let channel = match ctx.channel(&key.account_key()) {
            Some(c) => c,
            None => return Flow::Stop,
        };

        let session_ctx = ctx.session_context_for(&key.to_string());
        let pending = if is_retry {
            session_ctx.pending_retry.lock().await.take()
        } else {
            *session_ctx.pending_retry.lock().await = None;
            None
        };

        if is_retry {
            match pending {
                Some(user_msg) => {
                    // Rewrite content and fall through to dispatch.
                    msg.content = user_msg;
                    Flow::Next(msg)
                }
                None => {
                    send_text(&channel, &reply_target, MSG_NO_PENDING_RETRY).await;
                    Flow::Stop
                }
            }
        } else {
            send_text(&channel, &reply_target, MSG_ABORT_ACK).await;
            Flow::Stop
        }
    }
}

// ── CrashRecovery ─────────────────────────────────────────────────────────────

/// An incomplete turn loaded from a previous crash/SIGKILL: stash the orphaned
/// user message on `pending_retry` and prompt the user with retry/abort buttons.
struct CrashRecovery;

#[async_trait]
impl Interceptor for CrashRecovery {
    fn name(&self) -> &'static str {
        "crash_recovery"
    }
    async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) -> Flow {
        let sk = key.to_string();
        let session_ctx = ctx.session_context_for(&sk);
        if let Ok(mut session) = session_ctx.session.try_lock() {
            if session.incomplete_turn {
                session.incomplete_turn = false;

                let last_user_msg = session
                    .history
                    .last()
                    .filter(|m| m.role == "user")
                    .map(|m| m.text_content().to_string())
                    .unwrap_or_default();
                *session_ctx.pending_retry.lock().await = Some(last_user_msg);
                drop(session);

                let channel = match ctx.channel(&key.account_key()) {
                    Some(c) => c,
                    None => return Flow::Stop,
                };
                let (target, payload) = retry_abort_prompt(
                    MSG_INCOMPLETE_TURN,
                    &sk,
                    msg.reply_target.clone(),
                    Some(msg.id.clone()),
                );
                if let Err(e) = channel.send_payload(&target, &payload).await {
                    error!(session = %sk, err = %e, "failed to send incomplete-turn prompt");
                }
                return Flow::Stop;
            }
        }
        Flow::Next(msg)
    }
}

// ── SlashCommand ──────────────────────────────────────────────────────────────

/// Intercept slash commands before they reach the agent loop. The dispatch runs
/// on a spawned task so the event loop is not blocked.
struct SlashCommand;

#[async_trait]
impl Interceptor for SlashCommand {
    fn name(&self) -> &'static str {
        "slash_command"
    }
    async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) -> Flow {
        let content = msg.content.clone();
        let Some((cmd, cmd_args)) = commands::parse_command(&content) else {
            return Flow::Next(msg);
        };
        if !commands::is_known_command(cmd) {
            return Flow::Next(msg);
        }

        let sk = key.to_string();
        let cmd_owned = cmd.to_string();
        let cmd_args_owned = cmd_args.to_string();
        let session_ctx_cmd = ctx.session_manager.get_context(&sk);
        let registry_cmd = Arc::clone(&ctx.agent_runtime.providers);
        let sm_cmd = ctx.session_manager.clone();
        let runtime_cmd = ctx.agent_runtime.clone();
        let channel_cmd = ctx.channel(&key.account_key());
        let rt_cmd = msg.reply_target.clone();
        let rid_cmd = Some(msg.id.clone());

        tokio::spawn(async move {
            let cmd_ctx = commands::CommandContext {
                user_id: &sk,
                registry: &registry_cmd,
                session_manager: &sm_cmd,
                runtime: &runtime_cmd,
                session_ctx: session_ctx_cmd.as_ref(),
            };
            if let Some(response) = commands::dispatch(&cmd_owned, &cmd_args_owned, cmd_ctx).await {
                if let Some(channel) = channel_cmd {
                    let mut target = SendTarget::new(rt_cmd);
                    target.thread_id = rid_cmd;
                    if let Err(e) = channel.send_payload(&target, &MessagePayload::text(response)).await {
                        error!(session = %sk, err = %e, "command response send failed");
                    }
                }
            }
        });
        Flow::Stop
    }
}

// ── DispatchTurn (terminal) ───────────────────────────────────────────────────

/// Terminal interceptor: record the inbound message, persist it, and spawn the
/// turn via `SessionContext::process_turn`.
struct DispatchTurn;

#[async_trait]
impl Interceptor for DispatchTurn {
    fn name(&self) -> &'static str {
        "dispatch_turn"
    }
    async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) -> Flow {
        dispatch_turn(ctx, key, msg).await;
        Flow::Stop
    }
}

/// Record + persist the inbound message, then spawn the turn. Shared by the
/// terminal `DispatchTurn` interceptor and by delegation wakes (a delegation
/// completion is a system note that should drive a turn directly, without
/// re-running the user-message interceptors above).
pub(super) async fn dispatch_turn(ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) {
    let sk = key.to_string();

    // B12: store the full inbound ChannelMessage on the session.
    {
        let mut session = ctx.session_manager.get_or_create(&sk);
        session.record_inbound(msg.clone());
    }
    if let Err(e) = ctx.session_manager.backend().save_last_message(&sk, &msg) {
        tracing::warn!(session = %sk, err = %e, "failed to persist last_message");
    }

    let channel = match ctx.channel(&key.account_key()) {
        Some(c) => c,
        None => return,
    };

    // Dispatch via SessionContext.process_turn — the canonical RFC v2 per-turn
    // entry point. Spawn on a background task so the event loop is not blocked
    // by the LLM round-trip. image_urls / image_base64 ride along on
    // `session.last_message` (recorded above); Agent.run reads them from there.
    let session_ctx = ctx.session_manager.get_or_create_context(&sk);
    let runtime = ctx.agent_runtime.clone();
    let reply_target = msg.reply_target.clone();

    tokio::spawn(async move {
        // Successful turns: process_turn does the fallback `channel.send(text)`
        // internally per RFC §三.B. We only handle the error notice here.
        let result = session_ctx.process_turn(msg, Some(channel.clone()), runtime).await;
        if result.is_err() {
            let target = SendTarget::new(reply_target);
            let _ = channel.send_payload(&target, &MessagePayload::text(MSG_TURN_FAILED)).await;
        }
    });
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn send_text(channel: &Arc<dyn Channel>, reply_target: &str, text: &str) {
    let target = SendTarget::new(reply_target);
    let _ = channel.send_payload(&target, &MessagePayload::text(text)).await;
}

/// Build a `(SendTarget, MessagePayload::Interactive)` pair for the
/// **Retry / Abort** inline buttons prompt (RFC §6.2 / Phase 2).
///
/// The callback data carries a 32-char prefix of the session key so it fits
/// within Telegram's 64-byte limit.
pub(super) fn retry_abort_prompt(
    content: impl Into<String>,
    sk: &str,
    reply_target: impl Into<String>,
    thread_ts: Option<String>,
) -> (SendTarget, MessagePayload) {
    let sk_prefix: String = sk.chars().take(32).collect();
    let mut target = SendTarget::new(reply_target);
    target.thread_id = thread_ts;
    let payload = MessagePayload::Interactive {
        text: content.into(),
        buttons: vec![
            InlineButton {
                label: BTN_RETRY.to_string(),
                callback_data: CallbackAction::Retry {
                    session_key_prefix: sk_prefix.clone(),
                }
                .serialize(),
            },
            InlineButton {
                label: BTN_ABORT.to_string(),
                callback_data: CallbackAction::Abort {
                    session_key_prefix: sk_prefix,
                }
                .serialize(),
            },
        ],
    };
    (target, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden test: the inbound chain order is load-bearing (ask-reply must run
    /// before callback before dispatch, etc.). Pin it so reordering is a
    /// deliberate, reviewed change.
    #[test]
    fn chain_order_is_pinned() {
        let names: Vec<&str> = chain().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "ask_reply",
                "callback",
                "crash_recovery",
                "slash_command",
                "dispatch_turn",
            ]
        );
    }
}
