//! Friend-management tools — RFC §4.2 (tool channel: the bot acts for the
//! user after understanding intent). Four tools share one `FriendToolsCtx`
//! bound to the `KnownUsersRegistry`; the daemon injects the live
//! `ChannelRegistry` for §4.3 framework-template notifications.
//!
//! These are **main-agent-only** tools: contacts are user-level state and
//! sub-agents never see them (`filter_turn_scoped_tools` drops the names in
//! sub-agent sessions).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::agents::session::Session;
use crate::agents::{ContactStatus, KnownUsersRegistry, RequestOutcome, UserMail};
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};
use crate::providers::{Tool, ToolResult};

/// Shared context for the four friend tools.
pub struct FriendToolsCtx {
    known_users: Arc<KnownUsersRegistry>,
    /// Live channel registry, injected by the daemon after the Orchestrator
    /// is assembled (peer notifications, RFC §4.3).
    channels: OnceLock<crate::agents::ChannelRegistry>,
}

impl FriendToolsCtx {
    pub fn new(known_users: Arc<KnownUsersRegistry>) -> Self {
        Self {
            known_users,
            channels: OnceLock::new(),
        }
    }

    /// Install the live channel registry (set-once, called by the daemon).
    pub fn set_channels(&self, channels: crate::agents::ChannelRegistry) {
        let _ = self.channels.set(channels);
    }

    fn channels(&self) -> Option<&crate::agents::ChannelRegistry> {
        self.channels.get()
    }

    /// Best-effort framework-template push to the peer's channel (RFC §4.3).
    /// Silently skipped when the peer's channel is not live.
    async fn notify_peer(&self, peer_rk: &str, text: &str) {
        let Some(channels) = self.channels() else {
            return;
        };
        let mut it = peer_rk.splitn(3, ':');
        let (channel, account, user_id) = (
            it.next().unwrap_or(""),
            it.next().unwrap_or(""),
            it.next().unwrap_or(""),
        );
        let Some(ch) = channels.get(&(channel.to_string(), account.to_string())) else {
            return;
        };
        let message = ChannelOutboundMessage {
            receiver: MessageReceiver::new(user_id),
            content: ChannelMessageContent::text(text.to_string()),
            options: Default::default(),
        };
        if let Err(e) = ch.send_message(&message).await {
            warn!(peer = %peer_rk, err = %e, "friend tool: peer channel send failed");
        }
    }
}

// ── friend_request ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FriendRequestArgs {
    /// Target nickname, with or without the leading '@'.
    nick: String,
}

pub struct FriendRequestTool {
    ctx: Arc<FriendToolsCtx>,
}

impl FriendRequestTool {
    pub fn new(ctx: Arc<FriendToolsCtx>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for FriendRequestTool {
    fn name(&self) -> &str {
        "friend_request"
    }

    fn description(&self) -> &str {
        "Send a friend request to a user by nickname (e.g. @alice). The framework notifies \
         the recipient once; you can only request users who have interacted with this bot. \
         If the request is already pending or accepted, this is idempotent."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "nick": {
                    "type": "string",
                    "description": "Target nickname, with or without the leading '@'."
                }
            },
            "required": ["nick"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        300
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let args: FriendRequestArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("parameter error: {e}")),
                });
            }
        };
        let nick = args.nick.trim().trim_start_matches('@');
        if nick.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("friend_request requires a nick".to_string()),
            });
        }
        let owner = session.owner.clone();
        let peer = match crate::agents::commands::friends::resolve_nick_for(
            &self.ctx.known_users,
            &owner,
            nick,
        ) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };
        if peer == owner {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cannot add yourself as a friend".to_string()),
            });
        }
        match self.ctx.known_users.request_friend(&owner, &peer) {
            RequestOutcome::New => {
                self.ctx
                    .notify_peer(
                        &peer,
                        &format!(
                            "📩 {} 请求与你建立联系。用 /friends 查看，或直接告诉我处理。",
                            KnownUsersRegistry::nick_of(&owner)
                        ),
                    )
                    .await;
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "friend request sent to {}",
                        KnownUsersRegistry::nick_of(&peer)
                    ),
                    error: None,
                })
            }
            RequestOutcome::AlreadyPending => Ok(ToolResult {
                success: true,
                output: "request already pending; the recipient was not notified again".to_string(),
                error: None,
            }),
            RequestOutcome::AlreadyAccepted => Ok(ToolResult {
                success: true,
                output: "already friends — you can send messages directly".to_string(),
                error: None,
            }),
            RequestOutcome::BlockedByPeer => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "the recipient has blocked you; the request cannot be sent".to_string(),
                ),
            }),
            RequestOutcome::DeclinedTooSoon => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "the recipient declined within the last 24h; please wait before re-requesting"
                        .to_string(),
                ),
            }),
        }
    }
}

// ── friend_accept ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FriendNickArgs {
    nick: String,
}

pub struct FriendAcceptTool {
    ctx: Arc<FriendToolsCtx>,
}

impl FriendAcceptTool {
    pub fn new(ctx: Arc<FriendToolsCtx>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for FriendAcceptTool {
    fn name(&self) -> &str {
        "friend_accept"
    }

    fn description(&self) -> &str {
        "Accept a pending friend request from a user by nickname (e.g. @alice). \
         The requester is notified and both sides can then exchange messages."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "nick": {
                    "type": "string",
                    "description": "Requester nickname, with or without the leading '@'."
                }
            },
            "required": ["nick"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        300
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let args: FriendNickArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("parameter error: {e}")),
                });
            }
        };
        let nick = args.nick.trim().trim_start_matches('@');
        let owner = session.owner.clone();
        let Some((peer, _)) = self.ctx.known_users.resolve_contact_by_nick(&owner, nick) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("no pending request from @{nick}")),
            });
        };
        if !self.ctx.known_users.accept_friend(&owner, &peer) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "request from @{nick} is not in an acceptable state"
                )),
            });
        }
        let ack = format!(
            "{} 已接受你的好友请求，现在可以互发消息了",
            KnownUsersRegistry::nick_of(&owner)
        );
        self.ctx.notify_peer(&peer, &ack).await;
        self.ctx.known_users.push_user_mail(
            &peer,
            UserMail {
                msg_id: uuid::Uuid::new_v4().to_string(),
                sender_user_id: owner.clone(),
                sender_nickname: KnownUsersRegistry::nick_of(&owner),
                text: ack,
                sent_at: chrono::Utc::now().timestamp_millis() as u64,
            },
        );
        Ok(ToolResult {
            success: true,
            output: format!(
                "accepted friend request from {}",
                KnownUsersRegistry::nick_of(&peer)
            ),
            error: None,
        })
    }
}

// ── friend_decline ──────────────────────────────────────────────────────────

pub struct FriendDeclineTool {
    ctx: Arc<FriendToolsCtx>,
}

impl FriendDeclineTool {
    pub fn new(ctx: Arc<FriendToolsCtx>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for FriendDeclineTool {
    fn name(&self) -> &str {
        "friend_decline"
    }

    fn description(&self) -> &str {
        "Decline a pending friend request from a user by nickname (e.g. @alice). \
         The requester is notified; re-requests from the same pair are refused for 24h."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "nick": {
                    "type": "string",
                    "description": "Requester nickname, with or without the leading '@'."
                }
            },
            "required": ["nick"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        300
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let args: FriendNickArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("parameter error: {e}")),
                });
            }
        };
        let nick = args.nick.trim().trim_start_matches('@');
        let owner = session.owner.clone();
        let Some((peer, _)) = self.ctx.known_users.resolve_contact_by_nick(&owner, nick) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("no pending request from @{nick}")),
            });
        };
        if !self.ctx.known_users.decline_friend(&owner, &peer) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("request from @{nick} is not in a declinable state")),
            });
        }
        let ack = format!(
            "{} 拒绝了你的好友请求（24 小时内请勿重复发送）",
            KnownUsersRegistry::nick_of(&owner)
        );
        self.ctx.notify_peer(&peer, &ack).await;
        self.ctx.known_users.push_user_mail(
            &peer,
            UserMail {
                msg_id: uuid::Uuid::new_v4().to_string(),
                sender_user_id: owner.clone(),
                sender_nickname: KnownUsersRegistry::nick_of(&owner),
                text: ack,
                sent_at: chrono::Utc::now().timestamp_millis() as u64,
            },
        );
        Ok(ToolResult {
            success: true,
            output: format!(
                "declined friend request from {}",
                KnownUsersRegistry::nick_of(&peer)
            ),
            error: None,
        })
    }
}

// ── friend_list ─────────────────────────────────────────────────────────────

pub struct FriendListTool {
    ctx: Arc<FriendToolsCtx>,
}

impl FriendListTool {
    pub fn new(ctx: Arc<FriendToolsCtx>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for FriendListTool {
    fn name(&self) -> &str {
        "friend_list"
    }

    fn description(&self) -> &str {
        "List the current user's friend relationships: pending inbound requests \
         and established contacts with their nicknames and statuses."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    fn max_output_tokens(&self) -> usize {
        500
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let owner = session.owner.clone();
        let contacts = self.ctx.known_users.list_contacts(&owner);
        if contacts.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "no friend relationships yet".to_string(),
                error: None,
            });
        }
        let mut lines = vec![format!("contacts ({} total):", contacts.len())];
        for (peer, entry) in contacts {
            let state = match entry.status {
                ContactStatus::Pending
                    if entry.direction == crate::agents::ContactDirection::In =>
                {
                    "pending-in"
                }
                ContactStatus::Pending => "pending-out",
                ContactStatus::Accepted => "accepted",
                ContactStatus::Declined => "declined",
                ContactStatus::Blocked => "blocked",
            };
            lines.push(format!("  {} {} ({})", entry.nickname, state, peer));
        }
        Ok(ToolResult {
            success: true,
            output: lines.join("\n"),
            error: None,
        })
    }
}
