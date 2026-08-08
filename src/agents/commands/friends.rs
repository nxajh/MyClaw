//! `/friends` slash commands — RFC §4.2 (user channel, deterministic, bypasses LLM).
//!
//! Seven commands share one contacts table (`KnownUsersRegistry`):
//! `friends`, `friend_request`, `friend_accept`, `friend_decline`,
//! `friend_block`, `friend_unblock`, `friend_remove`.
//!
//! P4 目标解析：命令入参为 `u/uid` 或邮箱（经 [`register::parse_target`] 解析
//! 为 FQID user.id），`@昵称` 解析属第二波。显示一律实时渲染（昵称不落
//! 联系人快照，经 [`UserRegistry::display`]）。
//!
//! Authorization stays at the framework layer: the registry enforces the
//! §4.1 state machine (24h decline cooldown, block, pending idempotency),
//! and these handlers only translate user intent into registry calls plus
//! the §4.3 framework-template notifications.

use tracing::warn;

use crate::agents::commands::register::parse_target;
use crate::agents::commands::CommandContext;
use crate::agents::{ContactStatus, KnownUsersRegistry, RequestOutcome};
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};

/// Split a routing key into (channel, account, user_id).
fn split_rk(rk: &str) -> (&str, &str, &str) {
    let mut it = rk.splitn(3, ':');
    let channel = it.next().unwrap_or("");
    let account = it.next().unwrap_or("");
    let user_id = it.next().unwrap_or("");
    (channel, account, user_id)
}

/// 把折叠身份（user.id FQID）映射到可投递的 routing_key（RFC §4.3 通知用）。
///
/// 优先 resolver 里绑定的渠道（`/register`、`/link` 都会绑定）；其次在
/// 登记簿里找 resolve 到该身份的 rk。找不到（目标无任何渠道）返回 None。
pub(crate) fn rk_for(known_users: &KnownUsersRegistry, user_id: &str) -> Option<String> {
    if let Some(resolver) = known_users.resolver() {
        let keys = resolver.routing_keys_for(user_id);
        if let Some(k) = keys.first() {
            return Some(k.clone());
        }
    }
    for u in known_users.all_users() {
        let rk = format!("{}:{}:{}", u.channel, u.account, u.user_id);
        if known_users.resolve_uid(&rk) == user_id {
            return Some(rk);
        }
    }
    None
}

/// 当前用户的显示名（实时昵称，RFC §2.2 显示层）。
fn self_display(ctx: &CommandContext<'_>) -> String {
    let self_uid = ctx.known_users.resolve_uid(ctx.user_id);
    ctx.user_registry.display(&self_uid)
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

/// 在联系人表里按 FQID 精确查找 peer 键（P4: 联系人键一律是 user.id）。
fn find_peer<'a>(contacts: &'a [(String, crate::agents::ContactEntry)], target: &str) -> Option<&'a str> {
    contacts
        .iter()
        .find(|(peer, _)| peer == target)
        .map(|(peer, _)| peer.as_str())
}

/// `/friends` — list pending requests and established contacts.
pub fn cmd_friends(ctx: CommandContext<'_>) -> String {
    let contacts = ctx.known_users.list_contacts(ctx.user_id);
    if contacts.is_empty() {
        return "👥 还没有任何好友关系。用 /friend_request u/uid 或邮箱发起请求。".to_string();
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
            lines.push(format!("  {}（发送于 {}）", ctx.user_registry.display(peer), when));
        }
        lines.push("  用 /friend_accept u/uid 接受，或 /friend_decline u/uid 拒绝。".to_string());
    }
    if !accepted.is_empty() {
        lines.push("\n✅ **已建立**".to_string());
        for (peer, _entry) in &accepted {
            // RFC §6 P2 会话发现: 好友的在线/活跃状态（last_seen）。
            let presence = match ctx.known_users.last_seen_ms_of(peer) {
                Some(ts) => crate::agents::KnownUsersRegistry::render_presence(ts),
                None => "⚪ 离线".to_string(),
            };
            lines.push(format!("  {}（{presence}）", ctx.user_registry.display(peer)));
        }
    }
    if !blocked.is_empty() {
        lines.push("\n🚫 **已拉黑**".to_string());
        for (peer, _entry) in &blocked {
            lines.push(format!("  {}（{}）", ctx.user_registry.display(peer), peer));
        }
    }
    lines.join("\n")
}

/// `/friend_request u/uid 或邮箱` — initiate a friend request.
pub async fn cmd_friend_request(args: &str, ctx: CommandContext<'_>) -> String {
    let peer = match parse_target(ctx.user_registry, args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let self_uid = ctx.known_users.resolve_uid(ctx.user_id);
    if peer == self_uid {
        return "不能添加自己为好友".to_string();
    }
    match ctx.known_users.request_friend(ctx.user_id, &peer) {
        RequestOutcome::New => {
            let me = self_display(&ctx);
            if let Some(peer_rk) = rk_for(ctx.known_users, &peer) {
                notify_peer(
                    &ctx,
                    &peer_rk,
                    &format!("📩 {me} 请求与你建立联系。用 /friends 查看，或直接告诉我处理。"),
                )
                .await;
            }
            format!(
                "已向 {} 发送好友请求，等待对方处理",
                ctx.user_registry.display(&peer)
            )
        }
        RequestOutcome::AlreadyPending => "请求已发送，等待对方处理（不重复通知）".to_string(),
        RequestOutcome::AlreadyAccepted => "你们已是好友，可以直接互发消息".to_string(),
        RequestOutcome::BlockedByPeer => "对方已拉黑你，无法发送好友请求".to_string(),
        RequestOutcome::DeclinedTooSoon => "对方在 24 小时内拒绝过你的请求，请稍后再试".to_string(),
    }
}

/// `/friend_accept u/uid 或邮箱` — accept a pending inbound request.
pub async fn cmd_friend_accept(args: &str, ctx: CommandContext<'_>) -> String {
    let target = match parse_target(ctx.user_registry, args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let contacts = ctx.known_users.list_contacts(ctx.user_id);
    let Some(peer) = find_peer(&contacts, &target).map(str::to_string) else {
        return format!(
            "没有来自 {} 的待处理请求",
            ctx.user_registry.display(&target)
        );
    };
    if !ctx.known_users.accept_friend(ctx.user_id, &peer) {
        return format!("{} 的请求不在可接受状态", ctx.user_registry.display(&peer));
    }
    // §4.3 回执:框架模板通知发起方 + 注入上下文（写进发起方用户级 mailbox）。
    let me = self_display(&ctx);
    let ack = format!("{me} 已接受你的好友请求，现在可以互发消息了");
    if let Some(peer_rk) = rk_for(ctx.known_users, &peer) {
        let _ = notify_peer(&ctx, &peer_rk, &ack).await;
    }
    ctx.known_users.push_user_mail(
        &peer,
        crate::agents::UserMail {
            msg_id: crate::ids::Fqid::new(ctx.user_registry.namespace(), crate::ids::TYPE_MSG)
                .to_string(),
            sender_user_id: ctx.known_users.resolve_uid(ctx.user_id),
            sender_nickname: me,
            text: ack,
            sent_at: chrono::Utc::now().timestamp_millis() as u64,
        },
    );
    format!(
        "已接受 {} 的好友请求",
        ctx.user_registry.display(&peer)
    )
}

/// `/friend_decline u/uid 或邮箱` — decline a pending inbound request.
pub async fn cmd_friend_decline(args: &str, ctx: CommandContext<'_>) -> String {
    let target = match parse_target(ctx.user_registry, args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let contacts = ctx.known_users.list_contacts(ctx.user_id);
    let Some(peer) = find_peer(&contacts, &target).map(str::to_string) else {
        return format!(
            "没有来自 {} 的待处理请求",
            ctx.user_registry.display(&target)
        );
    };
    if !ctx.known_users.decline_friend(ctx.user_id, &peer) {
        return format!("{} 的请求不在可拒绝状态", ctx.user_registry.display(&peer));
    }
    let me = self_display(&ctx);
    let ack = format!("{me} 拒绝了你的好友请求（24 小时内请勿重复发送）");
    if let Some(peer_rk) = rk_for(ctx.known_users, &peer) {
        let _ = notify_peer(&ctx, &peer_rk, &ack).await;
    }
    ctx.known_users.push_user_mail(
        &peer,
        crate::agents::UserMail {
            msg_id: crate::ids::Fqid::new(ctx.user_registry.namespace(), crate::ids::TYPE_MSG)
                .to_string(),
            sender_user_id: ctx.known_users.resolve_uid(ctx.user_id),
            sender_nickname: me,
            text: ack,
            sent_at: chrono::Utc::now().timestamp_millis() as u64,
        },
    );
    format!(
        "已拒绝 {} 的好友请求",
        ctx.user_registry.display(&peer)
    )
}

/// `/friend_block u/uid 或邮箱` — block a user (owner-side only; user-only action).
pub fn cmd_friend_block(args: &str, ctx: CommandContext<'_>) -> String {
    let peer = match parse_target(ctx.user_registry, args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if peer == ctx.known_users.resolve_uid(ctx.user_id) {
        return "不能拉黑自己".to_string();
    }
    ctx.known_users.block_friend(ctx.user_id, &peer);
    format!(
        "已拉黑 {}，双向投递均已阻断",
        ctx.user_registry.display(&peer)
    )
}

/// `/friend_unblock u/uid 或邮箱` — unblock (returns to no-relationship).
pub fn cmd_friend_unblock(args: &str, ctx: CommandContext<'_>) -> String {
    let target = match parse_target(ctx.user_registry, args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let contacts = ctx.known_users.list_contacts(ctx.user_id);
    let Some(peer) = find_peer(&contacts, &target).map(str::to_string) else {
        return format!("没有拉黑 {}", ctx.user_registry.display(&target));
    };
    if !ctx.known_users.unblock_friend(ctx.user_id, &peer) {
        return format!("{} 不在拉黑状态", ctx.user_registry.display(&peer));
    }
    format!(
        "已解除对 {} 的拉黑（回到无关系，需重新发起请求）",
        ctx.user_registry.display(&peer)
    )
}

/// `/friend_remove u/uid 或邮箱` — remove an established relationship.
pub fn cmd_friend_remove(args: &str, ctx: CommandContext<'_>) -> String {
    let target = match parse_target(ctx.user_registry, args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let contacts = ctx.known_users.list_contacts(ctx.user_id);
    let Some(peer) = find_peer(&contacts, &target).map(str::to_string) else {
        return format!("没有与 {} 的好友关系", ctx.user_registry.display(&target));
    };
    if !ctx.known_users.remove_friend(ctx.user_id, &peer) {
        return format!("与 {} 未建立好友关系", ctx.user_registry.display(&peer));
    }
    format!(
        "已解除与 {} 的好友关系（需重新发起请求）",
        ctx.user_registry.display(&peer)
    )
}
