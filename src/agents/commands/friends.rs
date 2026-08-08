//! `/friends` slash commands — RFC §4.2 (user channel, deterministic, bypasses LLM).
//!
//! Seven commands share one contacts table (`KnownUsersRegistry`):
//! `friends`, `friend_request`, `friend_accept`, `friend_decline`,
//! `friend_block`, `friend_unblock`, `friend_remove`.
//!
//! Authorization stays at the framework layer: the registry enforces the
//! §4.1 state machine (24h decline cooldown, block, pending idempotency),
//! and these handlers only translate user intent into registry calls plus
//! the §4.3 framework-template notifications.

use std::sync::Arc;

use tracing::warn;

use crate::agents::commands::CommandContext;
use crate::agents::{ContactStatus, KnownUsersRegistry, RequestOutcome};
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};

/// Parse a `@nick` argument into the bare nick (strip the leading `@`).
fn parse_nick(args: &str) -> Option<&str> {
    let nick = args.trim().trim_start_matches('@');
    if nick.is_empty() { None } else { Some(nick) }
}

/// Split a routing key into (channel, account, user_id).
fn split_rk(rk: &str) -> (&str, &str, &str) {
    let mut it = rk.splitn(3, ':');
    let channel = it.next().unwrap_or("");
    let account = it.next().unwrap_or("");
    let user_id = it.next().unwrap_or("");
    (channel, account, user_id)
}

/// Resolve `@nick` → peer user_id (routing key).
///
/// Resolution chain (RFC §2): first within existing relationships (you
/// cannot @ a stranger; nicknames are stored per relationship so they are
/// unambiguous there), then among known users. Multiple known users with the
/// same nick → explicit disambiguation error (P1 acceptance: "同名不同
/// user_id 解析正确" — never silently pick one).
fn resolve_peer(ctx: &CommandContext<'_>, nick: &str) -> Result<String, String> {
    resolve_nick_for(ctx.known_users, ctx.user_id, nick)
}

/// Best-effort framework-template push to the peer's channel (RFC §4.3:
/// notification goes straight to the user, zero LLM tokens). Returns
/// `true` when the peer's channel was live and the send succeeded — used
/// by `/link` to detect "target channel unreachable, code not delivered".
pub(crate) async fn notify_peer(ctx: &CommandContext<'_>, peer_rk: &str, text: &str) -> bool {
    let (channel, account, user_id) = split_rk(peer_rk);
    let Some(ch) = ctx
        .channels
        .get(&(channel.to_string(), account.to_string()))
    else {
        return false;
    };
    let message = ChannelOutboundMessage {
        receiver: MessageReceiver::new(user_id),
        content: ChannelMessageContent::text(text.to_string()),
        options: Default::default(),
    };
    match ch.send_message(&message).await {
        Ok(_) => true,
        Err(e) => {
            warn!(peer = %peer_rk, err = %e, "notify: peer channel send failed");
            false
        }
    }
}

/// `/friends` — list pending requests and established contacts.
pub fn cmd_friends(ctx: CommandContext<'_>) -> String {
    let contacts = ctx.known_users.list_contacts(ctx.user_id);
    if contacts.is_empty() {
        return "👥 还没有任何好友关系。用 /friend_request @昵称 发起请求。".to_string();
    }

    let mut pending: Vec<(&str, &crate::agents::ContactEntry)> = Vec::new();
    let mut accepted: Vec<(&str, &crate::agents::ContactEntry)> = Vec::new();
    let mut blocked: Vec<(&str, &crate::agents::ContactEntry)> = Vec::new();
    for (peer, entry) in &contacts {
        match entry.status {
            ContactStatus::Pending if entry.direction == crate::agents::ContactDirection::In => {
                pending.push((peer, entry));
            }
            ContactStatus::Accepted => accepted.push((peer, entry)),
            ContactStatus::Blocked => blocked.push((peer, entry)),
            _ => {}
        }
    }

    let mut lines = vec![format!("👥 **好友**（{} 条关系）", contacts.len())];
    if !pending.is_empty() {
        lines.push("\n📩 **待处理请求**".to_string());
        for (peer, entry) in &pending {
            let when = crate::agents::commands::info::format_ts(entry.requested_at);
            lines.push(format!("  {}（{}，发送于 {}）", entry.nickname, peer, when));
        }
        lines.push("  用 /friend_accept @昵称 接受，或 /friend_decline @昵称 拒绝。".to_string());
    }
    if !accepted.is_empty() {
        lines.push("\n✅ **已建立**".to_string());
        for (peer, entry) in &accepted {
            // RFC §6 P2 会话发现: 好友的在线/活跃状态（last_seen）。
            let presence = match ctx.known_users.last_seen_ms_of(peer) {
                Some(ts) => crate::agents::KnownUsersRegistry::render_presence(ts),
                None => "⚪ 离线".to_string(),
            };
            lines.push(format!("  {}（{presence}）", entry.nickname));
        }
    }
    if !blocked.is_empty() {
        lines.push("\n🚫 **已拉黑**".to_string());
        for (peer, entry) in &blocked {
            lines.push(format!("  {}（{}）", entry.nickname, peer));
        }
    }
    lines.join("\n")
}

/// `/friend_request @昵称` — initiate a friend request.
pub async fn cmd_friend_request(args: &str, ctx: CommandContext<'_>) -> String {
    let Some(nick) = parse_nick(args) else {
        return "用法: /friend_request @昵称".to_string();
    };
    let peer = match resolve_peer(&ctx, nick) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if peer == ctx.user_id {
        return "不能添加自己为好友".to_string();
    }
    match ctx.known_users.request_friend(ctx.user_id, &peer) {
        RequestOutcome::New => {
            notify_peer(
                &ctx,
                &peer,
                &format!(
                    "📩 {} 请求与你建立联系。用 /friends 查看，或直接告诉我处理。",
                    KnownUsersRegistry::nick_of(ctx.user_id)
                ),
            )
            .await;
            format!(
                "已向 {} 发送好友请求，等待对方处理",
                KnownUsersRegistry::nick_of(&peer)
            )
        }
        RequestOutcome::AlreadyPending => "请求已发送，等待对方处理（不重复通知）".to_string(),
        RequestOutcome::AlreadyAccepted => "你们已是好友，可以直接互发消息".to_string(),
        RequestOutcome::BlockedByPeer => "对方已拉黑你，无法发送好友请求".to_string(),
        RequestOutcome::DeclinedTooSoon => "对方在 24 小时内拒绝过你的请求，请稍后再试".to_string(),
    }
}

/// `/friend_accept @昵称` — accept a pending inbound request.
pub async fn cmd_friend_accept(args: &str, ctx: CommandContext<'_>) -> String {
    let Some(nick) = parse_nick(args) else {
        return "用法: /friend_accept @昵称".to_string();
    };
    let Some((peer, _)) = ctx.known_users.resolve_contact_by_nick(ctx.user_id, nick) else {
        return format!("没有来自 @{nick} 的待处理请求");
    };
    if !ctx.known_users.accept_friend(ctx.user_id, &peer) {
        return format!("@{nick} 的请求不在可接受状态");
    }
    // §4.3 回执:框架模板通知发起方 + 注入上下文（写进发起方用户级 mailbox）。
    let ack = format!(
        "{} 已接受你的好友请求，现在可以互发消息了",
        KnownUsersRegistry::nick_of(ctx.user_id)
    );
    let _ = notify_peer(&ctx, &peer, &ack).await;
    ctx.known_users.push_user_mail(
        &peer,
        crate::agents::UserMail {
            msg_id: uuid::Uuid::new_v4().to_string(),
            sender_user_id: ctx.known_users.resolve_uid(ctx.user_id),
            sender_nickname: KnownUsersRegistry::nick_of(ctx.user_id),
            text: ack,
            sent_at: chrono::Utc::now().timestamp_millis() as u64,
        },
    );
    format!("已接受 {} 的好友请求", KnownUsersRegistry::nick_of(&peer))
}

/// `/friend_decline @昵称` — decline a pending inbound request.
pub async fn cmd_friend_decline(args: &str, ctx: CommandContext<'_>) -> String {
    let Some(nick) = parse_nick(args) else {
        return "用法: /friend_decline @昵称".to_string();
    };
    let Some((peer, _)) = ctx.known_users.resolve_contact_by_nick(ctx.user_id, nick) else {
        return format!("没有来自 @{nick} 的待处理请求");
    };
    if !ctx.known_users.decline_friend(ctx.user_id, &peer) {
        return format!("@{nick} 的请求不在可拒绝状态");
    }
    let ack = format!(
        "{} 拒绝了你的好友请求（24 小时内请勿重复发送）",
        KnownUsersRegistry::nick_of(ctx.user_id)
    );
    let _ = notify_peer(&ctx, &peer, &ack).await;
    ctx.known_users.push_user_mail(
        &peer,
        crate::agents::UserMail {
            msg_id: uuid::Uuid::new_v4().to_string(),
            sender_user_id: ctx.known_users.resolve_uid(ctx.user_id),
            sender_nickname: KnownUsersRegistry::nick_of(ctx.user_id),
            text: ack,
            sent_at: chrono::Utc::now().timestamp_millis() as u64,
        },
    );
    format!("已拒绝 {} 的好友请求", KnownUsersRegistry::nick_of(&peer))
}

/// `/friend_block @昵称` — block a user (owner-side only; user-only action).
pub fn cmd_friend_block(args: &str, ctx: CommandContext<'_>) -> String {
    let Some(nick) = parse_nick(args) else {
        return "用法: /friend_block @昵称".to_string();
    };
    let peer = match resolve_peer(&ctx, nick) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if peer == ctx.user_id {
        return "不能拉黑自己".to_string();
    }
    ctx.known_users.block_friend(ctx.user_id, &peer);
    format!(
        "已拉黑 {}，双向投递均已阻断",
        KnownUsersRegistry::nick_of(&peer)
    )
}

/// `/friend_unblock @昵称` — unblock (returns to no-relationship).
pub fn cmd_friend_unblock(args: &str, ctx: CommandContext<'_>) -> String {
    let Some(nick) = parse_nick(args) else {
        return "用法: /friend_unblock @昵称".to_string();
    };
    let Some((peer, _)) = ctx.known_users.resolve_contact_by_nick(ctx.user_id, nick) else {
        return format!("没有拉黑 @{nick}");
    };
    if !ctx.known_users.unblock_friend(ctx.user_id, &peer) {
        return format!("@{nick} 不在拉黑状态");
    }
    format!(
        "已解除对 {} 的拉黑（回到无关系，需重新发起请求）",
        KnownUsersRegistry::nick_of(&peer)
    )
}

/// `/friend_remove @昵称` — remove an established relationship.
pub fn cmd_friend_remove(args: &str, ctx: CommandContext<'_>) -> String {
    let Some(nick) = parse_nick(args) else {
        return "用法: /friend_remove @昵称".to_string();
    };
    let Some((peer, _)) = ctx.known_users.resolve_contact_by_nick(ctx.user_id, nick) else {
        return format!("没有与 @{nick} 的好友关系");
    };
    if !ctx.known_users.remove_friend(ctx.user_id, &peer) {
        return format!("与 @{nick} 未建立好友关系");
    }
    format!(
        "已解除与 {} 的好友关系（需重新发起请求）",
        KnownUsersRegistry::nick_of(&peer)
    )
}

/// Shared helper for friend tools to resolve a nick (kept pub(crate) for
/// `src/tools/friends.rs` reuse).
pub(crate) fn resolve_nick_for(
    known_users: &Arc<KnownUsersRegistry>,
    owner: &str,
    nick: &str,
) -> Result<String, String> {
    let nick = nick.trim_start_matches('@');
    if let Some((peer, _)) = known_users.resolve_contact_by_nick(owner, nick) {
        return Ok(peer);
    }
    let matches = known_users.find_users_by_nick(nick);
    match matches.len() {
        0 => Err(format!(
            "未找到用户 @{nick}（对方尚未与本 bot 互动过，无法发起好友请求）"
        )),
        1 => {
            let m = &matches[0];
            Ok(format!("{}:{}:{}", m.channel, m.account, m.user_id))
        }
        n => {
            let list = matches
                .iter()
                .map(|m| format!("{}:{}:{}", m.channel, m.account, m.user_id))
                .collect::<Vec<_>>()
                .join("、");
            Err(format!(
                "@{nick} 有 {n} 个同名用户（{list}），请先与其中一位建立好友关系，之后将按关系解析"
            ))
        }
    }
}
