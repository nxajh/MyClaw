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
    BTN_ABORT, BTN_RETRY, MSG_ABORT_ACK, MSG_INCOMPLETE_TURN, MSG_NO_PENDING_RETRY,
};
use crate::channels::{
    CallbackAction, Channel, ChannelInboundMessage, ChannelMessageContent, ChannelOutboundMessage,
    InlineButton, MessageReceiver,
};
use tracing::error;

/// Outcome of an interceptor.
#[allow(clippy::large_enum_variant)]
enum Flow {
    /// Message handled (replied to and/or spawned). Stop the chain.
    Stop,
    /// Pass the (possibly rewritten) message to the next interceptor.
    Next(ChannelInboundMessage),
}

#[async_trait]
trait Interceptor: Send + Sync {
    /// Stable identifier — used by the chain-order test.
    #[cfg_attr(not(test), allow(dead_code))]
    fn name(&self) -> &'static str;
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        msg: ChannelInboundMessage,
    ) -> Flow;
}

/// The ordered interceptor chain. Terminal stage (`DispatchTurn`) must be last.
fn chain() -> [&'static dyn Interceptor; 5] {
    [
        &AskReply,
        &Callback,
        &CrashRecovery,
        &SlashCommand,
        &DispatchTurn,
    ]
}

/// Run the inbound chain for one user message.
pub(super) async fn dispatch(
    ctx: &OrchestratorCtx,
    account: (String, String),
    msg: ChannelInboundMessage,
) {
    // Tell the Scheduler about this user message so cron / heartbeat jobs with
    // `target = "last"` know where to deliver their output. No-op when the
    // scheduler is disabled. This is a user-message-only side effect, so it
    // lives in the runner, not in the chain (delegation wakes skip it).
    if let Some(ref scheduler) = ctx.scheduler {
        let channel_key = format!("{}:{}", account.0, account.1);
        scheduler
            .record_user_message(&channel_key, &msg.receiver.id)
            .await;
    }

    let key = SessionKey::new(&account.0, &account.1, &msg.sender.id);
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
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        msg: ChannelInboundMessage,
    ) -> Flow {
        let session_id = ctx.sessions.get_or_create(&key.to_string()).id.clone();
        if ctx.ask.fulfill(&session_id, msg.clone()) {
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
/// `pending_retry` + `incomplete_turn`, close any open tool/user tail in history,
/// persist the closed form, and ack inline.
struct Callback;

#[async_trait]
impl Interceptor for Callback {
    fn name(&self) -> &'static str {
        "callback"
    }
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        mut msg: ChannelInboundMessage,
    ) -> Flow {
        let is_retry = match CallbackAction::parse(&msg.content.text) {
            Some(CallbackAction::Retry { .. }) => true,
            Some(CallbackAction::Abort { .. }) => false,
            _ => return Flow::Next(msg),
        };

        let reply_target = msg.receiver.id.clone();
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
                    // Retry continues the incomplete turn; clear the load-time
                    // flag so CrashRecovery does not re-prompt if the spawn is
                    // delayed. The actual recovery path re-reads history.
                    if let Ok(mut session) = session_ctx.session.try_lock() {
                        session.incomplete_turn = false;
                    }
                    // Rewrite content and fall through to dispatch.
                    msg.content.text = user_msg;
                    Flow::Next(msg)
                }
                None => {
                    send_text(&channel, &reply_target, MSG_NO_PENDING_RETRY).await;
                    Flow::Stop
                }
            }
        } else {
            // Abort must fully close the incomplete turn: drop orphan trailing
            // user, or fill cancelled tool results + assistant placeholder so
            // the next real user message is not stacked onto an open tool round.
            if let Ok(mut session) = session_ctx.session.try_lock() {
                let before_len = session.history.len();
                let removed = session.close_incomplete_turn_on_abort();
                session.incomplete_turn = false;
                if let Some(hook) = session.persist.clone() {
                    if removed > 0 {
                        // Tail user dropped — truncate persisted history.
                        hook.truncate_messages(&session.id, before_len - removed);
                    } else if session.history.len() > before_len {
                        // Closure messages appended — persist each new one.
                        let new_from = before_len;
                        for idx in new_from..session.history.len() {
                            if let Some(id) =
                                hook.persist_message(&session.id, &session.history[idx])
                            {
                                if let Some(slot) = session.message_ids.get_mut(idx) {
                                    *slot = id;
                                }
                            }
                        }
                    }
                }
            }
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
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        msg: ChannelInboundMessage,
    ) -> Flow {
        let sk = key.to_string();
        let session_ctx = ctx.session_context_for(&sk);
        if let Ok(mut session) = session_ctx.session.try_lock() {
            // Re-check history shape: a stale incomplete_turn flag (or a
            // session repaired offline) must not block a completed turn.
            let still_incomplete = session.incomplete_turn
                && super::history_has_incomplete_turn(&session.history);
            if session.incomplete_turn && !still_incomplete {
                session.incomplete_turn = false;
            }
            if still_incomplete {
                session.incomplete_turn = false;

                let last_user_msg = session
                    .history
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.text_content().to_string())
                    .unwrap_or_default();
                *session_ctx.pending_retry.lock().await = Some(last_user_msg);
                drop(session);

                let channel = match ctx.channel(&key.account_key()) {
                    Some(c) => c,
                    None => return Flow::Stop,
                };
                let message = ChannelOutboundMessage {
                    receiver: MessageReceiver::new(msg.receiver.id.clone()).with_reply_to(
                        msg.receiver
                            .reply_to_message_id
                            .clone()
                            .unwrap_or_else(|| msg.id.clone()),
                    ),
                    content: retry_abort_content(MSG_INCOMPLETE_TURN, &sk),
                    options: Default::default(),
                };
                if let Err(e) = channel.send_message(&message).await {
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
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        msg: ChannelInboundMessage,
    ) -> Flow {
        let content = msg.content.text.clone();
        let Some((cmd, cmd_args)) = commands::parse_command(&content) else {
            return Flow::Next(msg);
        };
        if !commands::is_known_command(cmd) {
            return Flow::Next(msg);
        }

        let sk = key.to_string();
        let cmd_owned = cmd.to_string();
        let cmd_args_owned = cmd_args.to_string();
        let session_ctx_cmd = ctx.sessions.get_context(&sk);
        let registry_cmd = Arc::clone(&ctx.runtime.providers);
        let sm_cmd = ctx.sessions.clone();
        let runtime_cmd = ctx.runtime.clone();
        let channel_cmd = ctx.channel(&key.account_key());
        let rt_cmd = msg.receiver.id.clone();
        let msg_id_cmd = msg
            .receiver
            .reply_to_message_id
            .clone()
            .unwrap_or_else(|| msg.id.clone());

        let turn_tracker = ctx.turn_tracker.clone();
        tokio::spawn(async move {
            let _guard = turn_tracker.track();
            let cmd_ctx = commands::CommandContext {
                user_id: &sk,
                registry: &registry_cmd,
                session_manager: &sm_cmd,
                runtime: &runtime_cmd,
                session_ctx: session_ctx_cmd.as_ref(),
            };
            if let Some(response) = commands::dispatch(&cmd_owned, &cmd_args_owned, cmd_ctx).await {
                if let Some(channel) = channel_cmd {
                    let message = ChannelOutboundMessage {
                        receiver: MessageReceiver::new(rt_cmd).with_reply_to(msg_id_cmd.clone()),
                        content: ChannelMessageContent::text(response),
                        options: Default::default(),
                    };
                    if let Err(e) = channel.send_message(&message).await {
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
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        msg: ChannelInboundMessage,
    ) -> Flow {
        dispatch_turn(ctx, key, msg).await;
        Flow::Stop
    }
}

/// Record + persist the inbound message, then spawn the turn. Shared by the
/// terminal `DispatchTurn` interceptor and by delegation wakes (a delegation
/// completion is a system note that should drive a turn directly, without
/// re-running the user-message interceptors above).
pub(super) async fn dispatch_turn(
    ctx: &OrchestratorCtx,
    key: &SessionKey,
    msg: ChannelInboundMessage,
) {
    let sk = key.to_string();

    // B12: store the full inbound message right before processing the turn inside
    // process_turn where turn_lock is held, to avoid appending or overwriting
    // history while a previous turn is still running.

    let channel = match ctx.channel(&key.account_key()) {
        Some(c) => c,
        None => return,
    };

    // Dispatch via SessionContext.process_turn — the canonical RFC v2 per-turn
    // entry point. Spawn on a background task so the event loop is not blocked
    // by the LLM round-trip. File attachments ride along on the
    // inbound message; Agent.run reads them from there.
    let session_ctx = ctx.sessions.get_or_create_context(&sk);
    let runtime = ctx.runtime.clone();
    let reply_target = msg.receiver.id.clone();
    // Capture passive-reply routing before msg is moved into process_turn.
    let passive_reply_id = msg
        .receiver
        .reply_to_message_id
        .clone()
        .unwrap_or_else(|| msg.id.clone());
    let inbound_thread_id = msg.receiver.thread_id.clone();

    let turn_tracker = ctx.turn_tracker.clone();
    tokio::spawn(async move {
        let _guard = turn_tracker.track();
        // Successful turns: process_turn does the final `channel.send_message(text)`
        // fallback internally. We only handle the error notice here.
        let result = session_ctx
            .process_turn(msg, Some(channel.clone()), runtime)
            .await;
        if let Err(ref e) = result {
            let text = crate::agents::user_messages::user_facing_error_message(e);
            let receiver = {
                let mut r =
                    MessageReceiver::new(reply_target).with_reply_to(passive_reply_id.clone());
                if let Some(tid) = inbound_thread_id.clone() {
                    r = r.with_thread(tid);
                }
                r
            };
            let message = ChannelOutboundMessage {
                receiver,
                content: ChannelMessageContent::text(text),
                options: Default::default(),
            };
            let _ = channel.send_message(&message).await;
        }
    });
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn send_text(channel: &Arc<dyn Channel>, reply_target: &str, text: &str) {
    let message = ChannelOutboundMessage {
        receiver: MessageReceiver::new(reply_target),
        content: ChannelMessageContent::text(text),
        options: Default::default(),
    };
    let _ = channel.send_message(&message).await;
}

/// Build the **Retry / Abort** inline buttons prompt content.
///
/// The callback data carries a 32-char prefix of the session key so it fits
/// within Telegram's 64-byte limit.
pub(super) fn retry_abort_content(content: impl Into<String>, sk: &str) -> ChannelMessageContent {
    let sk_prefix: String = sk.chars().take(32).collect();
    ChannelMessageContent {
        text: content.into(),
        files: vec![],
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::test_support::{MockChannel, inbound_msg, test_ctx};

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

    fn key() -> SessionKey {
        SessionKey::new("tg", "acc", "user1")
    }

    fn with_channel(ch: Arc<MockChannel>) -> OrchestratorCtx {
        let dyn_ch: Arc<dyn Channel> = ch;
        test_ctx(vec![(("tg".into(), "acc".into()), dyn_ch)])
    }

    fn next_content(flow: Flow) -> Option<String> {
        match flow {
            Flow::Next(m) => Some(m.content.text),
            Flow::Stop => None,
        }
    }

    // ── AskReply ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ask_reply_passes_through_when_no_pending_ask() {
        let ctx = test_ctx(vec![]);
        let flow = AskReply
            .handle(&ctx, &key(), inbound_msg("user1", "hi"))
            .await;
        assert_eq!(next_content(flow).as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn ask_reply_consumes_inbound_when_ask_pending() {
        let ctx = test_ctx(vec![]);
        let k = key();
        let session_id = ctx.sessions.get_or_create(&k.to_string()).id.clone();

        // Register an outstanding ask for this session.
        let router = ctx.ask.clone();
        let sid = session_id.clone();
        let waiter = tokio::spawn(async move { router.wait_for_reply(&sid).await });
        tokio::task::yield_now().await;

        let flow = AskReply
            .handle(&ctx, &k, inbound_msg("user1", "the answer"))
            .await;
        assert!(
            matches!(flow, Flow::Stop),
            "pending ask should consume the inbound"
        );
        assert_eq!(waiter.await.unwrap().unwrap().content.text, "the answer");
    }

    // ── Callback (retry / abort) ──────────────────────────────────────────

    #[tokio::test]
    async fn callback_non_callback_passes_through() {
        let ctx = test_ctx(vec![]);
        let flow = Callback
            .handle(&ctx, &key(), inbound_msg("user1", "ordinary text"))
            .await;
        assert_eq!(next_content(flow).as_deref(), Some("ordinary text"));
    }

    #[tokio::test]
    async fn callback_abort_acks_and_stops() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let content = CallbackAction::Abort {
            session_key_prefix: "x".into(),
        }
        .serialize();
        let flow = Callback
            .handle(&ctx, &key(), inbound_msg("user1", &content))
            .await;
        assert!(matches!(flow, Flow::Stop));
        assert!(ch.texts().iter().any(|t| t == MSG_ABORT_ACK));
    }

    #[tokio::test]
    async fn callback_retry_with_pending_rewrites_content() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        *ctx.session_context_for(&k.to_string())
            .pending_retry
            .lock()
            .await = Some("original question".into());

        let content = CallbackAction::Retry {
            session_key_prefix: "x".into(),
        }
        .serialize();
        let flow = Callback
            .handle(&ctx, &k, inbound_msg("user1", &content))
            .await;
        // Retry falls through with the saved text substituted in.
        assert_eq!(next_content(flow).as_deref(), Some("original question"));
    }

    #[tokio::test]
    async fn callback_retry_without_pending_notifies_and_stops() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let content = CallbackAction::Retry {
            session_key_prefix: "x".into(),
        }
        .serialize();
        let flow = Callback
            .handle(&ctx, &key(), inbound_msg("user1", &content))
            .await;
        assert!(matches!(flow, Flow::Stop));
        assert!(ch.texts().iter().any(|t| t == MSG_NO_PENDING_RETRY));
    }

    // ── CrashRecovery ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn crash_recovery_passes_through_when_turn_complete() {
        let ctx = test_ctx(vec![]);
        let flow = CrashRecovery
            .handle(&ctx, &key(), inbound_msg("user1", "hi"))
            .await;
        assert_eq!(next_content(flow).as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn crash_recovery_prompts_with_buttons_when_incomplete() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        {
            let sc = ctx.session_context_for(&k.to_string());
            let mut session = sc.session.lock().await;
            session.incomplete_turn = true;
            // History must actually look incomplete; a bare flag is no longer enough.
            session.add_user("orphaned question".into());
        }

        let flow = CrashRecovery
            .handle(&ctx, &k, inbound_msg("user1", "hi"))
            .await;
        assert!(matches!(flow, Flow::Stop));
        // The incomplete-turn prompt is an Interactive payload (retry + abort).
        let sent = ch.sent.lock().unwrap();
        assert!(sent.iter().any(|m| m.content.buttons.len() == 2));
    }

    #[tokio::test]
    async fn crash_recovery_clears_stale_flag_when_history_complete() {
        let ctx = test_ctx(vec![]);
        let k = key();
        {
            let sc = ctx.session_context_for(&k.to_string());
            let mut session = sc.session.lock().await;
            session.incomplete_turn = true;
            session.add_user("hi".into());
            session.add_assistant("hello".into());
        }

        let flow = CrashRecovery
            .handle(&ctx, &k, inbound_msg("user1", "next"))
            .await;
        assert_eq!(next_content(flow).as_deref(), Some("next"));
        let sc = ctx.session_context_for(&k.to_string());
        assert!(!sc.session.lock().await.incomplete_turn);
    }

    #[tokio::test]
    async fn callback_abort_closes_trailing_user() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        {
            let sc = ctx.session_context_for(&k.to_string());
            let mut session = sc.session.lock().await;
            session.incomplete_turn = true;
            session.add_user("orphaned".into());
        }
        let content = CallbackAction::Abort {
            session_key_prefix: "x".into(),
        }
        .serialize();
        let flow = Callback
            .handle(&ctx, &k, inbound_msg("user1", &content))
            .await;
        assert!(matches!(flow, Flow::Stop));
        let sc = ctx.session_context_for(&k.to_string());
        let session = sc.session.lock().await;
        assert!(!session.incomplete_turn);
        assert!(session.history.is_empty());
        assert!(ch.texts().iter().any(|t| t == MSG_ABORT_ACK));
    }

    // ── retry_abort_content helper ─────────────────────────────────────────

    #[test]
    fn retry_abort_content_truncates_long_session_key() {
        let long_sk = "telegram:account:".to_string() + &"u".repeat(100);
        let content = retry_abort_content("turn interrupted", &long_sk);
        let buttons = content.buttons;
        assert_eq!(buttons.len(), 2);
        // Callback data embeds a <=32-char session-key prefix (Telegram's
        // 64-byte callback_data limit).
        for b in &buttons {
            let action = CallbackAction::parse(&b.callback_data).expect("parseable callback");
            let prefix = match action {
                CallbackAction::Retry { session_key_prefix } => session_key_prefix,
                CallbackAction::Abort { session_key_prefix } => session_key_prefix,
                _ => panic!("expected retry/abort"),
            };
            assert_eq!(prefix.chars().count(), 32);
        }
    }
}
