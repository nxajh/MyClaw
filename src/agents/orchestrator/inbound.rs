//! Inbound dispatch — the interceptor chain for user messages.
//!
//! A user message flows through an ordered chain of [`Interceptor`]s. Each one
//! either consumes the message ([`Flow::Stop`]) or passes it on, possibly
//! rewritten ([`Flow::Next`]). The terminal [`DispatchTurn`] always stops the
//! chain by spawning the actual turn.
//!
//! Order: ask-reply → callback (retry/abort) → gate → slash-command →
//! mention-preparse → dispatch-turn. The chain is the single source of truth
//! for "what happens to an inbound message before it reaches `process_turn`".
//!
//! Spooled-message replay (RFC inbound-spool §6.4) uses a `replay_chain` — the
//! same interceptors minus the terminal `DispatchTurn` — and drives
//! `process_turn` synchronously (in order, no spawn) instead of dispatching a
//! fresh turn; the `DispatchTurn` pre-hook drains Pending spool entries for
//! the routing key before dispatching a real user message (switch-back replay).

use std::sync::Arc;

use async_trait::async_trait;

use super::ctx::OrchestratorCtx;
use super::key::SessionKey;
use crate::agents::commands;
use crate::agents::user_messages::{MSG_ABORT_ACK, MSG_NO_PENDING_RETRY};
use crate::channels::{
    CallbackAction, Channel, ChannelInboundMessage, ChannelMessageContent, ChannelOutboundMessage,
    MessageReceiver,
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
fn chain() -> [&'static dyn Interceptor; 6] {
    [
        &AskReply,
        &Callback,
        &Gate,
        &SlashCommand,
        &MentionPreParse,
        &DispatchTurn,
    ]
}

/// Replay chain: the same interceptors minus the terminal `DispatchTurn`.
/// Replayed (spooled) messages must pass through the full interception
/// semantics (gate, callbacks, slash commands, ask-reply) but are driven by
/// [`replay_one_sync`] — synchronously, in order — and must never re-enter the
/// `DispatchTurn` pre-hook (no-reentry via the `replay:` id prefix).
fn replay_chain() -> [&'static dyn Interceptor; 5] {
    [&AskReply, &Callback, &Gate, &SlashCommand, &MentionPreParse]
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

    // Rate limit check + user registration (unified — was scattered in each
    // channel's internal KnownSenders/RateLimiter). Returns early if the
    // sender is rate-limited.
    let scope = if msg.receiver.id.starts_with("group:") {
        format!("group:{}", &msg.receiver.id[6..])
    } else {
        "c2c".to_string()
    };
    if !ctx
        .known_users
        .check_and_record(&account.0, &account.1, &msg.sender.id, &scope)
    {
        tracing::warn!(
            channel = %account.0,
            account = %account.1,
            sender = %msg.sender.id,
            "rate limited, dropping"
        );
        return;
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
                    // Retry continues the turn; clear the load-time incomplete
                    // flag so the rewritten message is treated as a fresh user
                    // message (not an open-tail continuation). process_turn
                    // also clears the flag at turn start.
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

// ── Gate（P4 调度层白名单）──────────────────────────────────────────────────

/// 未注册用户（routing_key 未绑定 User）可用的命令（RFC §2.3）。
const GATE_WHITELIST: &[&str] = &[
    "register",
    "email",
    "link",
    "link_confirm",
    "help",
    "whoami",
];

/// 未注册用户的引导文案（框架模板，零 LLM token）。
const GATE_PROMPT: &str = "👋 欢迎！首次使用请先创建身份：/register <邮箱> <uid>（uid 为 3–32 位小写字母/数字/下划线，如 alice）。已有身份可 /link u/uid 绑定当前渠道。用 /help 查看全部命令。";

/// P4 gate：未绑定 User 的 routing_key 只能使用白名单命令；其余入站消息
/// 拦截并回复引导文案（不进 agent loop，零 LLM 开销）。
struct Gate;

#[async_trait]
impl Interceptor for Gate {
    fn name(&self) -> &'static str {
        "gate"
    }
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        msg: ChannelInboundMessage,
    ) -> Flow {
        let registered = {
            let resolved = ctx.known_users.resolve_uid(&key.to_string());
            ctx.user_registry.is_user_id(&resolved)
        };
        if registered {
            return Flow::Next(msg);
        }
        // 未注册：白名单命令放行（SlashCommand 处理）。
        let content = msg.content.text.clone();
        if let Some((cmd, _)) = commands::parse_command(&content) {
            if GATE_WHITELIST.contains(&cmd) {
                return Flow::Next(msg);
            }
        }
        // 拦截：框架模板回复（零 token）。
        if let Some(ch) = ctx.channel(&key.account_key()) {
            let reply = ChannelOutboundMessage {
                receiver: MessageReceiver::new(msg.receiver.id.clone())
                    .with_reply_to(msg.id.clone()),
                content: ChannelMessageContent::text(GATE_PROMPT.to_string()),
                options: Default::default(),
            };
            if let Err(e) = ch.send_message(&reply).await {
                tracing::warn!(session = %key, err = %e, "gate: reply send failed");
            }
        }
        Flow::Stop
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
        let known_users_cmd = Arc::clone(&ctx.known_users);
        let user_registry_cmd = Arc::clone(&ctx.user_registry);
        let channels_cmd = ctx.channels.clone();
        let key_channel = key.channel.clone();
        let key_account = key.account.clone();
        tokio::spawn(async move {
            let _guard = turn_tracker.track();
            let cmd_ctx = commands::CommandContext {
                user_id: &sk,
                channel_type: &key_channel,
                account_id: &key_account,
                registry: &registry_cmd,
                session_manager: &sm_cmd,
                runtime: &runtime_cmd,
                session_ctx: session_ctx_cmd.as_ref(),
                known_users: &known_users_cmd,
                user_registry: &user_registry_cmd,
                channels: &channels_cmd,
                channel: channel_cmd.as_ref(),
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

// ── MentionPreParse（P4 第二波：入站 @提及 预解析） ──────────────────────────

/// RFC §2.2 入站 @提及 预解析：在 gate 之后、消息进 agent 上下文之前统一解析
/// 自由文本中的 `@昵称` / `@u/uid`，原位替换为 `<ref id="…"/>` 标签——agent 只
/// 见 id 标记、不见昵称（LLM 猜 id = 发错人风险，禁止）。
/// 解析失败（未找到 / 重名多命中）→ 框架模板回复（零 token），不进 agent。
/// 好友校验不在此层——「你们还不是好友」由 send_message 工具内 contacts 检查。
struct MentionPreParse;

#[async_trait]
impl Interceptor for MentionPreParse {
    fn name(&self) -> &'static str {
        "mention_preparse"
    }
    async fn handle(
        &self,
        ctx: &OrchestratorCtx,
        key: &SessionKey,
        msg: ChannelInboundMessage,
    ) -> Flow {
        // Gate 已放行注册用户；防御性跳过未注册（避免解析层报「未找到」）。
        let sk = key.to_string();
        let owner = ctx.known_users.resolve_uid(&sk);
        if !ctx.user_registry.is_user_id(&owner) {
            return Flow::Next(msg);
        }
        if !msg.content.text.contains('@') {
            return Flow::Next(msg);
        }
        let text = msg.content.text.clone();
        match crate::agents::mention::resolve_mentions(&text, &ctx.user_registry) {
            crate::agents::mention::MentionResolution::Resolved(new_text) => {
                let mut msg = msg;
                msg.content.text = new_text;
                Flow::Next(msg)
            }
            crate::agents::mention::MentionResolution::Failed(reply) => {
                if let Some(ch) = ctx.channel(&key.account_key()) {
                    let out = ChannelOutboundMessage {
                        receiver: MessageReceiver::new(msg.receiver.id.clone())
                            .with_reply_to(msg.id.clone()),
                        content: ChannelMessageContent::text(reply),
                        options: Default::default(),
                    };
                    if let Err(e) = ch.send_message(&out).await {
                        tracing::warn!(session = %key, err = %e, "mention: reply send failed");
                    }
                }
                Flow::Stop
            }
        }
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
        // RFC §6.4 switch-back replay: before dispatching a fresh user message,
        // drain any Pending spooled entries for this routing key (messages that
        // arrived while the session was inactive or crashed mid-turn stay in the
        // spool until the user comes back). Replayed messages carry `replay:`
        // ids and never re-enter this interceptor, so the prefix check is a
        // defensive no-reentry guard.
        if !msg.id.starts_with("replay:") {
            replay_pending_for_key(ctx, key).await;
        }
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
    dispatch_turn_spawn(ctx, key, msg);
}

/// Synchronous core of `dispatch_turn` — the body has NO awaits (it records
/// the inbound message and spawns the turn task). `drain_delegation_notices`
/// calls this directly instead of awaiting `dispatch_turn`: awaiting the
/// async entry here would make the spawned-turn future graph cyclic
/// (`dispatch_turn`'s body spawns the block that awaits the drain, and the
/// drain awaits `dispatch_turn`) and the `Send` proof at `tokio::spawn`
/// fails to close. A sync call keeps the graph acyclic.
///
/// Returns the spawned task's `JoinHandle` when a turn was actually spawned
/// (`None` when the message was queued or dropped instead). Issue #106:
/// `drain_delegation_notices` awaits this handle for each notice in turn —
/// `tokio::spawn` already type-erases the future (breaking the cyclic-type
/// concern above), so awaiting the returned handle costs nothing structurally
/// but lets the drain loop enforce strict one-at-a-time delivery order
/// instead of firing N independently-scheduled tasks that race the same
/// `turn_lock` in whatever order the runtime happens to poll them.
pub(super) fn dispatch_turn_spawn(
    ctx: &OrchestratorCtx,
    key: &SessionKey,
    msg: ChannelInboundMessage,
) -> Option<tokio::task::JoinHandle<()>> {
    let sk = key.to_string();

    let session_ctx = ctx.sessions.get_or_create_context(&sk);

    // 单 preview (2026-08-12): while the session is suspended on async
    // delegations, real user messages are queued (静默排队 — no ack) instead
    // of driving a fresh turn — the final resume turn drains them in order
    // (RFC turn-suspension §3.3 单 preview/排队段). Delegation notices
    // (silenced_override = Some) and slash commands (user-initiated, must not
    // stall behind a suspension) bypass the queue; ask-reply/callback/known
    // commands were already consumed by earlier interceptors. The
    // `has_notice_turns_in_flight()` arm keeps queueing while `pending` is
    // already empty but delegation notices are still queued behind
    // `turn_lock` — the suspension (and its preview) survives until the LAST
    // notice finishes, so the user message must not cut in between.
    if msg.silenced_override.is_none()
        && !msg.content.text.starts_with('/')
        && (session_ctx.has_pending_delegations() || session_ctx.has_notice_turns_in_flight())
    {
        session_ctx.enqueue_user_message(msg);
        return None;
    }

    // B12: store the full inbound message right before processing the turn inside
    // process_turn where turn_lock is held, to avoid appending or overwriting
    // history while a previous turn is still running.

    let channel = ctx.channel(&key.account_key())?;

    // Dispatch via SessionContext.process_turn — the canonical RFC v2 per-turn
    // entry point. Spawn on a background task so the event loop is not blocked
    // by the LLM round-trip. File attachments ride along on the
    // inbound message; Agent.run reads them from there.
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
    let known_users = ctx.known_users.clone();
    let user_registry = ctx.user_registry.clone();
    // 单 preview: the drained-queue re-dispatch re-runs the full chain, which
    // needs the whole ctx — cheap (all fields are Arc/Clone).
    let ctx = ctx.clone();
    // `key` is a borrow and cannot move into the 'static spawn closure; the
    // drained message is re-dispatched against the same account pair.
    let account = key.account_key();
    Some(tokio::spawn(async move {
        let _guard = turn_tracker.track();
        // RFC §3.5/§4.3: render per-turn injections — user-level mailbox
        // (cross-user messages; drained = 注入即消费, shown once) and pending
        // friend requests (re-rendered every turn while requests remain).
        // Stashed on the session; Agent::run injects them into the first
        // LLM request as <system-reminder> user messages (never persisted
        // to history).
        let mut injections = Vec::new();
        let mails = known_users.drain_user_mail(&sk);
        if !mails.is_empty() {
            injections.push(crate::agents::KnownUsersRegistry::render_user_mail_reminder(
                &mails,
            ));
        }
        let pending = known_users.pending_requests(&sk);
        if !pending.is_empty() {
            // P4 显示层: 对方显示名实时渲染（昵称不落快照）。
            let display = |peer: &str| user_registry.display(peer);
            injections
                .push(crate::agents::KnownUsersRegistry::render_pending_requests_reminder(
                    &pending,
                    display,
                ));
        }
        if !injections.is_empty() {
            session_ctx.stash_turn_injections(injections).await;
        }
        // Successful turns: process_turn does the final `channel.send_message(text)`
        // fallback internally. We only handle the error notice here.
        // P2: the synthetic notice id is the store's dedup/mark key — clone
        // before `msg` moves into process_turn.
        let notice_id = msg.id.clone();
        let result = session_ctx
            .process_turn(msg, Some(channel.clone()), runtime)
            .await;
        // P2 (2026-08-13, RFC delegation-notice-queue §5.3): the turn
        // persisted its content to session history (Ok) — mark the persisted
        // entry delivered so a restart does not re-deliver it. Only
        // `delegation*` synthetic ids are tracked; `recovery:` and channel
        // user-message ids are never persisted (mark returns false — no-op).
        // Err keeps the entry Pending (at-least-once re-delivery).
        if result.is_ok() && notice_id.starts_with("delegation") {
            if let Some(store) = &ctx.completion_queue {
                match store.mark_delivered(&notice_id) {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            notice_id = %notice_id,
                            err = %e,
                            "completion queue: mark delivered failed"
                        );
                    }
                }
            }
        }
        // P1 (2026-08-13, RFC delegation-notice-queue §4): after the turn
        // releases `turn_lock`, drain delegation notices that arrived while
        // it ran (completions whose `route_notice` saw a busy lock). Runs
        // BEFORE the queued-user-message drain below: `drain_delegation_notices`
        // bumps `notice_turns_in_flight` synchronously before spawning each
        // notice turn, so the user-message check sees the counter and keeps
        // queueing until the suspension sequence truly ends. Issue #106: this
        // `.await` now blocks until every drained notice's turn has fully
        // finished (`drain_delegation_notices` awaits each notice's dispatch
        // `JoinHandle` in turn, one at a time) — no longer "just take+spawn,
        // the turns run in the background"; that's what makes delivery FIFO
        // instead of racing on `turn_lock` in whatever order the runtime
        // happens to schedule them.
        if session_ctx.has_queued_delegation_notices() {
            super::delegation::drain_delegation_notices(&ctx, &session_ctx.session_id).await;
        }
        // 单 preview (2026-08-12): after a turn that ends OUTSIDE the
        // suspension sequence (not silenced, no re-delegation, suspension
        // already clear), drain ONE queued user message and re-dispatch it
        // through the full chain — serial, the next one is drained by the
        // next such turn (turn_lock serialization makes this safe; each
        // drain→dispatch→spawn returns, so no stack recursion).
        match &result {
            Ok(turn)
                if !turn.has_pending
                    && !session_ctx.has_pending_delegations()
                    && !session_ctx.has_notice_turns_in_flight() =>
            {
                if let Some(queued) = session_ctx.take_user_message() {
                    dispatch(&ctx, account.clone(), queued).await;
                }
            }
            _ => {}
        }
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
    }))
}

// ── spooled-message replay (RFC inbound-spool §6.4) ──────────────────────────

/// Replay ONE spooled message: run `replay_chain` (full interception
/// semantics — gate, callbacks, slash commands, ask-reply), then drive the
/// turn synchronously (no spawn) so replays are serial and ordered. Replay
/// only ever runs on paths with a live channel (startup Phase 2 /
/// switch-back hook), so a missing channel here is a transient anomaly — the
/// entry is still marked done by the caller, whose mark-after keeps
/// at-least-once (a replayed message already delivered by an interceptor must
/// not be delivered twice).
async fn replay_one_sync(ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelInboundMessage) {
    let mut msg = msg;
    for stage in replay_chain() {
        match stage.handle(ctx, key, msg).await {
            Flow::Stop => return,
            Flow::Next(m) => msg = m,
        }
    }
    let Some(channel) = ctx.channel(&key.account_key()) else {
        return;
    };
    let sk = key.to_string();
    let session_ctx = ctx.sessions.get_or_create_context(&sk);
    let runtime = ctx.runtime.clone();
    if let Err(e) = session_ctx.process_turn(msg, Some(channel), runtime).await {
        tracing::warn!(session = %sk, err = %e, "replay: process_turn failed");
    }
}

/// Replay all Pending spooled entries for one routing key, oldest first.
/// Called from the `DispatchTurn` pre-hook (switch-back) and from the startup
/// recovery task (Phase 2, after `run_recovery`). Each entry is marked done
/// AFTER its replay turn returns (mark-after keeps at-least-once: a crash
/// mid-replay leaves the entry Pending for the next opportunity).
pub(super) async fn replay_pending_for_key(ctx: &OrchestratorCtx, key: &SessionKey) {
    let Some(spool) = &ctx.inbound_spool else {
        return;
    };
    let pending = spool.pending_for(&key.channel, &key.account, &key.sender);
    if pending.is_empty() {
        return;
    }
    tracing::info!(
        session = %key.to_string(),
        count = pending.len(),
        "replaying pending inbound spool entries"
    );
    for entry in pending {
        let mut msg = entry.msg.into_runtime();
        // `replay:` prefix: dedup-key-safe (channel ids never collide) and the
        // DispatchTurn no-reentry guard — a replayed message never re-enters
        // the switch-back pre-hook.
        msg.id = format!("replay:{}", msg.id);
        replay_one_sync(ctx, key, msg).await;
        if let Err(e) = spool.mark_done(entry.seq) {
            tracing::warn!(
                seq = entry.seq,
                err = %e,
                "inbound spool: replay mark_done failed; entry stays Pending"
            );
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::test_support::{MockChannel, inbound_msg, test_ctx};
    use crate::agents::user_messages::MSG_TURN_FAILED;
    use crate::storage::InboundSpool;

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
                "gate",
                "slash_command",
                "mention_preparse",
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

    // ── CrashRecovery 已退役（RFC §6.4）：恢复由 run_recovery + 重放接管 ──

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

    // ── DispatchTurn: 挂起期间用户消息排队 (RFC §3.7 架构修正) ─────────────

    /// Poll until the mock channel has received at least one message — the
    /// spawned turn fails fast against the NullRegistry (provider bail) and
    /// sends the error notice. Fails if nothing arrives within 3s.
    async fn wait_for_sent(ch: &MockChannel) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while ch.sent.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "expected the spawned turn to fail fast and send an error notice"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn dispatch_turn_queues_user_message_while_suspended() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        let sctx = ctx.sessions.get_or_create_context(&k.to_string());
        sctx.add_pending_task("t1".to_string());

        dispatch_turn(&ctx, &k, inbound_msg("user1", "while suspended")).await;

        let queued = sctx.take_user_message().expect("message should be queued");
        assert_eq!(queued.content.text, "while suspended");
        // 静默排队:不发确认消息、不 spawn turn.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(ch.texts().is_empty(), "queuing must not send any message");
    }

    #[tokio::test]
    async fn dispatch_turn_queues_while_notice_in_flight_even_if_pending_empty() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        let sctx = ctx.sessions.get_or_create_context(&k.to_string());
        // 刷屏 bug 窗口 (2026-08-12): 无挂起任务(或 pending 已被 wake burst 清空),
        // 但 delegation-notice 轮仍在途(counter=1,排队在 turn_lock 之后)——用户
        // 消息必须仍静默排队,不得插队开新 turn/preview。
        sctx.bump_notice_turn();

        dispatch_turn(&ctx, &k, inbound_msg("user1", "queued while notice in flight")).await;

        let queued = sctx
            .take_user_message()
            .expect("message should queue while notices are in flight");
        assert_eq!(queued.content.text, "queued while notice in flight");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(ch.texts().is_empty(), "queuing must not send any message");
        // 清掉在途计数后队列恢复空(消息仍在队列,等待序列结束后 drain)。
        sctx.finish_notice_turn();
        assert!(!sctx.has_notice_turns_in_flight());
    }

    #[tokio::test]
    async fn dispatch_turn_queue_preserves_fifo_order() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        let sctx = ctx.sessions.get_or_create_context(&k.to_string());
        sctx.add_pending_task("t1".to_string());

        dispatch_turn(&ctx, &k, inbound_msg("user1", "first")).await;
        dispatch_turn(&ctx, &k, inbound_msg("user1", "second")).await;

        assert_eq!(sctx.take_user_message().unwrap().content.text, "first");
        assert_eq!(sctx.take_user_message().unwrap().content.text, "second");
        assert!(sctx.take_user_message().is_none());
    }

    #[tokio::test]
    async fn dispatch_turn_bypasses_queue_for_slash_command() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        let sctx = ctx.sessions.get_or_create_context(&k.to_string());
        sctx.add_pending_task("t1".to_string());

        dispatch_turn(&ctx, &k, inbound_msg("user1", "/status")).await;

        assert!(
            sctx.take_user_message().is_none(),
            "slash commands must not queue behind a suspension"
        );
        // The command drives a turn immediately (fails fast → error notice).
        wait_for_sent(&ch).await;
    }

    #[tokio::test]
    async fn dispatch_turn_bypasses_queue_for_delegation_notice() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        let sctx = ctx.sessions.get_or_create_context(&k.to_string());
        sctx.add_pending_task("t1".to_string());

        let mut notice = inbound_msg("user1", "[系统通知] 子代理已完成后台任务");
        notice.silenced_override = Some(true);
        dispatch_turn(&ctx, &k, notice).await;

        assert!(
            sctx.take_user_message().is_none(),
            "delegation notices must drive resume turns, not queue"
        );
        wait_for_sent(&ch).await;
    }

    #[tokio::test]
    async fn dispatch_turn_does_not_queue_when_not_suspended() {
        let ch = MockChannel::new();
        let ctx = with_channel(ch.clone());
        let k = key();
        let sctx = ctx.sessions.get_or_create_context(&k.to_string());

        dispatch_turn(&ctx, &k, inbound_msg("user1", "normal")).await;

        assert!(
            sctx.take_user_message().is_none(),
            "unsuspended sessions never queue"
        );
        wait_for_sent(&ch).await;
    }

    // ── spool replay (RFC inbound-spool §6.4) ──────────────────────────────

    /// Golden test: the replay chain is the inbound chain minus the terminal
    /// `DispatchTurn` — a replayed message must run full interception semantics
    /// (gate, callbacks, slash commands, ask-reply) but must never re-enter the
    /// switch-back pre-hook. Pin the order so reordering is a deliberate,
    /// reviewed change.
    #[test]
    fn replay_chain_order_is_pinned() {
        let names: Vec<&str> = replay_chain().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "ask_reply",
                "callback",
                "gate",
                "slash_command",
                "mention_preparse",
            ]
        );
    }

    /// A spooled inbound message with a DISTINCT id — the `inbound_msg` helper
    /// hardcodes "test-msg", which the spool dedupes by (channel, account, id).
    fn spool_msg(id: &str, content: &str) -> ChannelInboundMessage {
        ChannelInboundMessage {
            id: id.to_string(),
            sender: crate::channels::MessageSender::new("user1".to_string()),
            receiver: MessageReceiver::new("user1".to_string()),
            content: ChannelMessageContent::text(content.to_string()),
            timestamp: 0,
            interruption_scope_id: None,
            silenced_override: None,
            run_mode: Default::default(),
        }
    }

    /// Open a spool containing the given pre-crash Pending entries for
    /// `(tg, acc, user1)`. Entries are appended in one instance (dropped
    /// without mark_done), then the spool is reopened as the successor so
    /// `seq <= baseline` makes them replayable — the same shape as
    /// `recovery::replay_persisted_inbound_and_marks_done`. The `TempDir` is
    /// returned alongside because `mark_done` rewrites the entry file.
    fn spool_with_pending(entries: &[(&str, &str)]) -> (Arc<InboundSpool>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inbound_spool");
        {
            let spool = InboundSpool::open(dir.clone()).unwrap();
            for (id, text) in entries {
                spool.append("tg", "acc", &spool_msg(id, text)).unwrap();
            }
        }
        (
            Arc::new(InboundSpool::open(dir).unwrap()),
            tmp,
        )
    }

    /// The text parts of the session's user messages, in history order.
    async fn session_user_texts(ctx: &OrchestratorCtx, k: &SessionKey) -> Vec<String> {
        let sc = ctx.sessions.get_or_create_context(&k.to_string());
        let session = sc.session.lock().await;
        session
            .history
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| match p {
                        crate::providers::ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    /// Register `tg:acc:user1` with a real user id so Gate lets it through
    /// (an unregistered user is stopped at Gate with GATE_PROMPT and never
    /// reaches process_turn — see `dispatch_turn_prehook_drains_pending_*`).
    fn register_user(ctx: &mut OrchestratorCtx, k: &SessionKey) {
        let resolver = Arc::new(crate::agents::UserResolver::new());
        resolver.set(k.to_string(), "myclaw/u/test1".to_string());
        ctx.known_users = Arc::new(
            crate::agents::KnownUsersRegistry::in_memory()
                .with_resolver(Arc::clone(&resolver)),
        );
    }

    #[tokio::test]
    async fn replay_pending_for_key_serial_and_ordered() {
        let ch = MockChannel::new();
        let (spool, _tmp) = spool_with_pending(&[
            ("m1", "first"),
            ("m2", "second"),
            ("m3", "third"),
        ]);
        let mut ctx = with_channel(ch.clone());
        register_user(&mut ctx, &key());
        ctx.inbound_spool = Some(Arc::clone(&spool));

        let k = key();
        replay_pending_for_key(&ctx, &k).await;

        // All entries drained and marked Done (mark-after per replay turn).
        assert!(spool.pending().is_empty());
        assert_eq!(spool.len(), 0);
        // Registered user → Gate passes everything through, so every entry
        // reaches process_turn; each turn fails fast at provider resolution
        // (NullRegistry) and sends exactly one MSG_TURN_FAILED notice.
        let texts = ch.texts();
        assert_eq!(texts.len(), 3, "each replayed turn sends one failure notice");
        assert!(
            texts.iter().all(|t| t.as_str() == MSG_TURN_FAILED),
            "expected only failure notices, got: {:?}",
            texts
        );
        // History holds the replayed user texts in spool order (oldest
        // first). process_turn prepends a <system-reminder> to the content,
        // so assert on the content tail, not exact equality.
        let users = session_user_texts(&ctx, &k).await;
        assert_eq!(users.len(), 3, "all three replays must reach process_turn");
        for (i, expected) in ["first", "second", "third"].iter().enumerate() {
            assert!(
                users[i].ends_with(expected),
                "user msg {} should end with {:?}, got {:?}",
                i,
                expected,
                users[i]
            );
        }
    }

    #[tokio::test]
    async fn dispatch_turn_prehook_drains_pending_before_dispatch() {
        let ch = MockChannel::new();
        let (spool, _tmp) = spool_with_pending(&[
            ("m1", "hi one"),
            ("m2", "hi two"),
            ("m3", "hi three"),
        ]);
        let mut ctx = with_channel(ch.clone());
        // Unregistered user (default test_ctx) — Gate stops each replayed
        // entry, which still proves the pre-hook drained and marked them.
        ctx.inbound_spool = Some(Arc::clone(&spool));

        DispatchTurn
            .handle(&ctx, &key(), inbound_msg("user1", "live message"))
            .await;

        assert!(
            spool.pending().is_empty(),
            "the pre-hook must drain Pending entries before dispatching the live message"
        );
        assert_eq!(spool.len(), 0, "drained entries must be marked Done");
        // Each of the 3 replayed entries hit Gate (unregistered → GATE_PROMPT),
        // then the live dispatch spawns a turn that fails fast against the
        // NullRegistry → error notice: 4 sends total.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while ch.texts().len() < 4 {
            assert!(
                std::time::Instant::now() < deadline,
                "expected 3 gate prompts + 1 live error notice, got {}",
                ch.texts().len()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let texts = ch.texts();
        assert_eq!(
            texts.iter().filter(|t| t.as_str() == GATE_PROMPT).count(),
            3,
            "each replayed entry must be stopped by Gate, not dispatched"
        );
    }

    #[tokio::test]
    async fn dispatch_turn_prehook_skips_replay_prefix() {
        let ch = MockChannel::new();
        let (spool, _tmp) = spool_with_pending(&[("m1", "hi one")]);
        let mut ctx = with_channel(ch.clone());
        ctx.inbound_spool = Some(Arc::clone(&spool));

        let mut msg = inbound_msg("user1", "already replayed");
        msg.id = "replay:test-msg".to_string();
        DispatchTurn.handle(&ctx, &key(), msg).await;

        assert_eq!(
            spool.pending().len(),
            1,
            "the pre-hook must skip `replay:`-prefixed messages (no-reentry guard)"
        );
        // The live dispatch still happens (fails fast → error notice).
        wait_for_sent(&ch).await;
    }
}
