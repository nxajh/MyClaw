//! Friend-management tools — RFC §4.2 (tool channel: the bot acts for the
//! user after understanding intent). Four tools share one `FriendToolsCtx`
//! bound to the `KnownUsersRegistry` + P4 `UserRegistry`; the daemon injects
//! the live `ChannelRegistry` for §4.3 framework-template notifications.
//!
//! P4 目标解析：`target` 入参为 `u/uid` 或邮箱（经 [`register::parse_target`]
//! 解析为 FQID user.id），`@昵称` 解析属第二波。显示一律实时渲染（昵称
//! 不落联系人快照）。
//!
//! These are **main-agent-only** tools: contacts are user-level state and
//! sub-agents never see them (`filter_turn_scoped_tools` drops the names in
//! sub-agent sessions).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::identity::known_users::rk_for;
use crate::identity::user_registry::parse_target;
use crate::api::tool::ToolContext;
use crate::identity::{ContactStatus, KnownUsersRegistry, RequestOutcome, UserMail, UserRegistry};
use crate::api::message::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};
use crate::ids::{DEFAULT_NAMESPACE, Fqid, TYPE_MSG};
use crate::providers::{Tool, ToolResult};

/// Shared context for the four friend tools.
pub struct FriendToolsCtx {
    known_users: Arc<KnownUsersRegistry>,
    /// P4 用户实体注册表（`u/uid` / 邮箱 → FQID 解析 + 显示名渲染）。
    user_registry: Arc<UserRegistry>,
    /// Live channel registry, injected by the daemon after the Orchestrator
    /// is assembled (peer notifications, RFC §4.3).
    channels: OnceLock<crate::api::channel_registry::ChannelRegistry>,
    /// Namespace for generated message FQIDs (`<ns>/msg/<uuidv7>`). Bound at
    /// construction from `[system] namespace`; `new()` defaults to
    /// `DEFAULT_NAMESPACE` (tests / single-agent).
    namespace: String,
}

impl FriendToolsCtx {
    pub fn new(known_users: Arc<KnownUsersRegistry>, user_registry: Arc<UserRegistry>) -> Self {
        Self::with_namespace(known_users, user_registry, DEFAULT_NAMESPACE)
    }

    /// Construct with an explicit namespace (daemon path — from `[system]
    /// namespace`). Test helpers use [`Self::new`] (default namespace).
    pub fn with_namespace(
        known_users: Arc<KnownUsersRegistry>,
        user_registry: Arc<UserRegistry>,
        namespace: &str,
    ) -> Self {
        Self {
            known_users,
            user_registry,
            channels: OnceLock::new(),
            namespace: namespace.to_string(),
        }
    }

    /// Install the live channel registry (set-once, called by the daemon).
    pub fn set_channels(&self, channels: crate::api::channel_registry::ChannelRegistry) {
        let _ = self.channels.set(channels);
    }

    fn channels(&self) -> Option<&crate::api::channel_registry::ChannelRegistry> {
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

    /// 当前用户的显示名（实时昵称）。
    fn self_display(&self, ctx: &ToolContext) -> String {
        let uid = self.known_users.resolve_uid(&ctx.owner);
        self.user_registry.display(&uid)
    }
}

/// 在联系人表里按 FQID 精确查找 peer 键（P4: 联系人键一律是 user.id）。
fn find_peer<'a>(contacts: &'a [(String, crate::identity::ContactEntry)], target: &str) -> Option<&'a str> {
    contacts
        .iter()
        .find(|(peer, _)| peer == target)
        .map(|(peer, _)| peer.as_str())
}

// ── friend_request ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FriendRequestArgs {
    /// Target user id (`u/uid`) or email (P4 第一波; @昵称 属第二波).
    target: String,
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
        "Send a friend request to a registered user by user id (u/uid) or email \
         (e.g. u/alice or alice@example.com). The framework notifies the \
         recipient once; you can only request registered users. If the \
         request is already pending or accepted, this is idempotent."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Target user id (u/uid) or email, e.g. u/alice or alice@example.com."
                }
            },
            "required": ["target"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        300
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
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
        let peer = match parse_target(&self.ctx.user_registry, &args.target) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };
        let owner = ctx.owner.clone();
        if peer == self.ctx.known_users.resolve_uid(&owner) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cannot add yourself as a friend".to_string()),
            });
        }
        match self.ctx.known_users.request_friend(&owner, &peer) {
            RequestOutcome::New => {
                let me = self.ctx.self_display(ctx);
                if let Some(peer_rk) = rk_for(&self.ctx.known_users, &peer) {
                    self.ctx
                        .notify_peer(
                            &peer_rk,
                            &format!("📩 {me} 请求与你建立联系。用 /friends 查看，或直接告诉我处理。"),
                        )
                        .await;
                }
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "friend request sent to {}",
                        self.ctx.user_registry.display(&peer)
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
struct FriendTargetArgs {
    /// Target user id (`u/uid`) or email.
    target: String,
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
        "Accept a pending friend request from a user by user id (u/uid) or email \
         (e.g. u/alice or alice@example.com). The requester is notified and \
         both sides can then exchange messages."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Requester user id (u/uid) or email, e.g. u/alice or alice@example.com."
                }
            },
            "required": ["target"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        300
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let args: FriendTargetArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("parameter error: {e}")),
                });
            }
        };
        let target = match parse_target(&self.ctx.user_registry, &args.target) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };
        let owner = ctx.owner.clone();
        let contacts = self.ctx.known_users.list_contacts(&owner);
        let Some(peer) = find_peer(&contacts, &target).map(str::to_string) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "no pending request from {}",
                    self.ctx.user_registry.display(&target)
                )),
            });
        };
        if !self.ctx.known_users.accept_friend(&owner, &peer) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "request from {} is not in an acceptable state",
                    self.ctx.user_registry.display(&peer)
                )),
            });
        }
        let me = self.ctx.self_display(ctx);
        let ack = format!("{me} 已接受你的好友请求，现在可以互发消息了");
        if let Some(peer_rk) = rk_for(&self.ctx.known_users, &peer) {
            self.ctx.notify_peer(&peer_rk, &ack).await;
        }
        self.ctx.known_users.push_user_mail(
            &peer,
            UserMail {
                msg_id: Fqid::new(&self.ctx.namespace, TYPE_MSG).to_string(),
                sender_user_id: self.ctx.known_users.resolve_uid(&owner),
                sender_nickname: me,
                text: ack,
                sent_at: chrono::Utc::now().timestamp_millis() as u64,
            },
        );
        Ok(ToolResult {
            success: true,
            output: format!(
                "accepted friend request from {}",
                self.ctx.user_registry.display(&peer)
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
        "Decline a pending friend request from a user by user id (u/uid) or email \
         (e.g. u/alice or alice@example.com). The requester is notified; \
         re-requests from the same pair are refused for 24h."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Requester user id (u/uid) or email, e.g. u/alice or alice@example.com."
                }
            },
            "required": ["target"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        300
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let args: FriendTargetArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("parameter error: {e}")),
                });
            }
        };
        let target = match parse_target(&self.ctx.user_registry, &args.target) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };
        let owner = ctx.owner.clone();
        let contacts = self.ctx.known_users.list_contacts(&owner);
        let Some(peer) = find_peer(&contacts, &target).map(str::to_string) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "no pending request from {}",
                    self.ctx.user_registry.display(&target)
                )),
            });
        };
        if !self.ctx.known_users.decline_friend(&owner, &peer) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "request from {} is not in a declinable state",
                    self.ctx.user_registry.display(&peer)
                )),
            });
        }
        let me = self.ctx.self_display(ctx);
        let ack = format!("{me} 拒绝了你的好友请求（24 小时内请勿重复发送）");
        if let Some(peer_rk) = rk_for(&self.ctx.known_users, &peer) {
            self.ctx.notify_peer(&peer_rk, &ack).await;
        }
        self.ctx.known_users.push_user_mail(
            &peer,
            UserMail {
                msg_id: Fqid::new(&self.ctx.namespace, TYPE_MSG).to_string(),
                sender_user_id: self.ctx.known_users.resolve_uid(&owner),
                sender_nickname: me,
                text: ack,
                sent_at: chrono::Utc::now().timestamp_millis() as u64,
            },
        );
        Ok(ToolResult {
            success: true,
            output: format!(
                "declined friend request from {}",
                self.ctx.user_registry.display(&peer)
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
         and established contacts with their display names and statuses."
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
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let owner = ctx.owner.clone();
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
                    if entry.direction == crate::identity::ContactDirection::In =>
                {
                    "pending-in"
                }
                ContactStatus::Pending => "pending-out",
                ContactStatus::Accepted => "accepted",
                ContactStatus::Declined => "declined",
                ContactStatus::Blocked => "blocked",
            };
            // P4 显示层: 实时渲染对方显示名（昵称不落快照）。
            // RFC §6 P2 会话发现: accepted 好友附在线/活跃状态。
            let mut line = format!("  {} ({})", self.ctx.user_registry.display(&peer), state);
            if entry.status == ContactStatus::Accepted {
                if let Some(ts) = self.ctx.known_users.last_seen_ms_of(&peer) {
                    line.push_str(&format!(" {}", KnownUsersRegistry::render_presence(ts)));
                }
            }
            lines.push(line);
        }
        Ok(ToolResult {
            success: true,
            output: lines.join("\n"),
            error: None,
        })
    }
}
