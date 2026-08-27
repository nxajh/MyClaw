//! `send_message` tool — send text and/or local files to the user, or
//! route messages between parent and sub agents (RFC agent-messaging §3).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::api::tool::ToolContext;
use crate::api::agent_mail::{AgentMail, AgentMessage, AgentMessenger, MessageKind};
use crate::identity::{KnownUsersRegistry, UserRegistry};
use crate::api::message::{
    ChannelFile, ChannelFileMeta, ChannelMessageContent, ChannelOutboundMessage, LocalFileBody,
    MessageReceiver, SendOptions,
};
use crate::ids::{DEFAULT_NAMESPACE, Fqid, TYPE_MSG};
use crate::providers::{Tool, ToolResult};

const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
/// Hard cap on a single message's text length (RFC agent-messaging §3.7).
/// Sending side errors out — never silently truncates or splits. Longer
/// content goes through the `files` channel instead.
const MAX_TEXT_CHARS: usize = 32 * 1024;

pub struct SendMessageTool {
    /// Agent-to-agent message bus. Set by the daemon in multi-agent mode;
    /// never set (and unused) in single-agent deployments.
    messenger: Arc<OnceLock<Arc<dyn AgentMessenger>>>,
    /// Known-users registry for cross-user delivery (RFC §3.5: recipient
    /// `u/uid`/邮箱 → contacts check → recipient's user-level mailbox). Set by
    /// the daemon; `None` in tests/single-agent mode makes cross-user targets
    /// error out clearly.
    known_users: Arc<OnceLock<Arc<KnownUsersRegistry>>>,
    /// P4 用户实体注册表（uid/email/username）——cross-user recipient 解析
    /// （`u/uid` / 邮箱 → FQID）与发送者显示名渲染共用。
    user_registry: Arc<OnceLock<Arc<UserRegistry>>>,
    /// Namespace for generated message FQIDs (`<ns>/msg/<uuidv7>`). Bound at
    /// construction from `[system] namespace`; `new()` defaults to
    /// `DEFAULT_NAMESPACE` (tests / single-agent).
    namespace: String,
}

impl Default for SendMessageTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SendMessageTool {
    pub fn new() -> Self {
        Self::with_namespace(DEFAULT_NAMESPACE)
    }

    /// Construct with an explicit namespace (daemon path — from `[system]
    /// namespace`). Test helpers use [`Self::new`] (default namespace).
    pub fn with_namespace(namespace: &str) -> Self {
        Self {
            messenger: Arc::new(OnceLock::new()),
            known_users: Arc::new(OnceLock::new()),
            user_registry: Arc::new(OnceLock::new()),
            namespace: namespace.to_string(),
        }
    }

    /// Install the agent-to-agent message bus (called by the daemon after
    /// the `DelegationCoordinator` exists; set-once).
    pub fn set_messenger(&self, messenger: Arc<dyn AgentMessenger>) {
        let _ = self.messenger.set(messenger);
    }

    /// Install the known-users registry (called by the daemon after the
    /// registry is loaded; set-once). Enables `recipient=u/uid`/邮箱 cross-user
    /// delivery (RFC §3.5).
    pub fn set_known_users(&self, known_users: Arc<KnownUsersRegistry>) {
        let _ = self.known_users.set(known_users);
    }

    /// Install the P4 user registry (called by the daemon after it is
    /// assembled; set-once). Enables `u/uid` / 邮箱 → FQID resolution for
    /// cross-user recipients and real-time sender display names.
    pub fn set_user_registry(&self, user_registry: Arc<UserRegistry>) {
        let _ = self.user_registry.set(user_registry);
    }

    /// Agent-to-agent delivery (RFC agent-messaging §3).
    ///
    /// - Sub-agent context: the only legal target is the parent main agent
    ///   (`recipient` omitted or "parent"); any other value errors (§3.3 —
    ///   the sub-agent sees no other address).
    /// - Main context: `recipient` must be the session id of a running async
    ///   sub-agent; delivery goes through the coordinator's mailbox.
    ///
    /// Agent messages are text-only (P0 scope — no file transfer).
    async fn execute_agent_message(
        &self,
        args: &SendMessageArgs,
        text: &str,
        is_sub_agent: bool,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        if !args.files.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "agent-to-agent messages support text only (file transfer is not yet supported)"
                        .to_string(),
                ),
            });
        }
        if text.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("agent-to-agent message requires text".to_string()),
            });
        }
        let Some(messenger) = self.messenger.get() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "agent messaging is not available in this deployment (single-agent mode)"
                        .to_string(),
                ),
            });
        };

        if is_sub_agent {
            // §3.3: the sub-agent knows exactly one address — its parent.
            if let Some(r) = args.recipient.as_deref() {
                if r != "parent" {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "invalid recipient '{}': a sub-agent can only message its parent (omit recipient or use \"parent\")",
                            r
                        )),
                    });
                }
            }
            // §3.3: the sub-agent's identity is its own session id — always
            // present, so no Option dance (unlike the parent link, which a
            // malformed session could lack).
            let Some(parent_session_id) = ctx.parent_session_id.clone() else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("parent session missing — cannot message parent".to_string()),
                });
            };
            let event = AgentMessage {
                msg_id: Fqid::new(&self.namespace, TYPE_MSG).to_string(),
                sender_name: ctx.agent_name.clone(),
                sub_session_id: ctx.session_id.clone(),
                parent_session_id,
                text: text.to_string(),
                kind: match args.kind.as_deref() {
                    Some("progress") => MessageKind::Progress,
                    _ => MessageKind::Final,
                },
            };
            return match messenger.send_to_parent(event).await {
                true => Ok(ToolResult {
                    success: true,
                    output: "message sent to parent agent".to_string(),
                    error: None,
                }),
                false => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("message delivery failed: parent agent is not reachable".to_string()),
                }),
            };
        }

        // Main agent context: `recipient` = sub-agent session id.
        let sub_session_id = match args.recipient.as_deref() {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "recipient required in the main agent context (a sub-agent session id)"
                            .to_string(),
                    ),
                });
            }
        };
        let mail = AgentMail {
            msg_id: Fqid::new(&self.namespace, TYPE_MSG).to_string(),
            sender_name: "主 agent".to_string(),
            text: text.to_string(),
            timestamp: chrono::Utc::now().timestamp() as u64,
        };
        match messenger.send_to_sub_agent(&sub_session_id, mail) {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: "message sent to sub-agent".to_string(),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e),
            }),
        }
    }

    /// Cross-user delivery (RFC §3.5): `recipient=u/uid` 或邮箱 from the main
    /// agent context. Resolves the target to a FQID user.id via the P4 user
    /// registry, checks the delivery verdict against the recipient's contacts
    /// table (§4.1), and on `Allowed` writes into the recipient's **user-level
    /// mailbox** — delivered on the recipient's next user interaction
    /// (注入即消费), not a wake-up. Text-only (P1 scope — no file transfer).
    /// Returns an Ack to the sender, mirroring §3.5's "已投递(Ack)".
    async fn execute_cross_user(
        &self,
        args: &SendMessageArgs,
        text: &str,
        recipient: &str,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        if !args.files.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "cross-user messages support text only (file transfer is not yet supported)"
                        .to_string(),
                ),
            });
        }
        if text.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cross-user message requires text".to_string()),
            });
        }
        let Some(known_users) = self.known_users.get() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "cross-user messaging is not available in this deployment".to_string(),
                ),
            });
        };
        let Some(user_registry) = self.user_registry.get() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "cross-user messaging is not available in this deployment".to_string(),
                ),
            });
        };

        let owner = ctx.owner.clone();
        // P3/P4 身份折叠: 发送者经 resolver 归一到 FQID user.id。
        let owner_uid = known_users.resolve_uid(&owner);
        let peer = match crate::commands::register::parse_target(user_registry, recipient)
        {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };
        if peer == owner_uid {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cannot message yourself".to_string()),
            });
        }
        match known_users.delivery_verdict(&owner_uid, &peer) {
            crate::identity::DeliveryVerdict::Allowed => {
                known_users.push_user_mail(
                    &peer,
                    crate::identity::UserMail {
                        msg_id: Fqid::new(&self.namespace, TYPE_MSG).to_string(),
                        sender_user_id: owner_uid.clone(),
                        sender_nickname: user_registry.display(&owner_uid),
                        text: text.to_string(),
                        sent_at: chrono::Utc::now().timestamp_millis() as u64,
                    },
                );
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "message delivered to {} — shown on their next interaction",
                        user_registry.display(&peer)
                    ),
                    error: None,
                })
            }
            crate::identity::DeliveryVerdict::Blocked => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("the recipient {} has blocked you", recipient)),
            }),
            // RFC §4.2 工具通道: 未建立关系 → 返回"发送好友请求?"引导,由 bot
            // 确认后调 friend_request 工具;框架不自动发请求。
            crate::identity::DeliveryVerdict::NotFriends => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "you are not friends with {recipient} yet — ask the user whether to send a friend request (use the friend_request tool)"
                )),
            }),
        }
    }
}

/// 判断 recipient 是否走跨用户路径（P4 第一波: `u/uid` 或邮箱；`@昵称`
/// 也归此路径并在解析层给出明确的"第二波"报错）。
fn is_cross_user_target(r: &str) -> bool {
    let t = r.trim_start();
    t.starts_with('@') || t.starts_with("u/") || t.contains('@')
}

#[derive(Debug, Deserialize)]
struct SendMessageArgs {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Vec<SendMessageFileArg>,
    /// Optional agent-to-agent target. Parent context: a sub-agent session id.
    /// Sub-agent context: omitted or "parent".
    #[serde(default)]
    recipient: Option<String>,
    /// Optional message kind for sub-agent → parent messages
    /// (turn-suspension RFC §2.3). "progress" → `Progress` (mid-flight
    /// report, never injected into the parent context); omitted or any
    /// other value → `Final` (today's behavior — wakes/injects).
    /// Ignored for user and cross-user delivery.
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendMessageFileArg {
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a message to the current user, to a friend, or to another agent. Supports plain text, text with local files, or multiple local files. \
         File parameters only accept local paths (not URLs); paths are resolved by the tool and not exposed to the channel. \
         Optional `recipient`: a friend's user id (u/uid) or email to send a cross-user message; in the main agent context also a sub-agent session id (from agent_delegate mode=async) to message that sub-agent; \
         in a sub-agent context, omit it or use \"parent\" to message the main agent. Agent-to-agent and cross-user messages are text-only (32K chars max)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Text to send to the user. Can be sent alone or as a description for files."
                },
                "files": {
                    "type": "array",
                    "description": "List of local files to send. Only local paths are supported (not URLs).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Local file path."
                            },
                            "name": {
                                "type": "string",
                                "description": "Optional display name for the file when sent."
                            },
                            "mime_type": {
                                "type": "string",
                                "description": "Optional file MIME type; inferred by the tool if not provided."
                            }
                        },
                        "required": ["path"]
                    }
                },
                "recipient": {
                    "type": "string",
                    "description": "Optional target. Cross-user (main agent context): a friend's user id (u/uid) or email. Agent-to-agent: main agent context passes a sub-agent session id (from agent_delegate mode=async); sub-agent context omits it or uses \"parent\" to send to the main agent."
                },
                "kind": {
                    "type": "string",
                    "description": "Optional message kind for sub-agent → parent messages: \"progress\" (mid-flight report, never injected into the parent context) or omitted/\"final\" (default; wakes and injects). Ignored for user and cross-user delivery."
                }
            }
        })
    }

    fn max_output_tokens(&self) -> usize {
        500
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let args: SendMessageArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("parameter error: {e}")),
                });
            }
        };

        let text = args.text.clone().unwrap_or_default();
        // RFC agent-messaging §3.7: hard 32K chars cap on a single message —
        // clear error on the sending side, never truncate or split. Longer
        // content goes through the `files` channel instead.
        if text.chars().count() > MAX_TEXT_CHARS {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "message too long: {} chars (limit {}) — longer content goes through the files channel",
                    text.chars().count(),
                    MAX_TEXT_CHARS
                )),
            });
        }
        if text.trim().is_empty() && args.files.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("send_message requires text or files".to_string()),
            });
        }

        // RFC agent-messaging §3.1/§3.3: agent-to-agent path. A sub-agent's
        // only legal target is its parent ("parent" or omitted); the main
        // agent targets a running async sub-agent by session id.
        let is_sub_agent = ctx.parent_session_id.is_some();
        // RFC §3.5: cross-user path first — `recipient=u/uid`/邮箱（`@昵称`
        // 第二波）in the main agent context targets another user through the
        // friend contacts table (delivered to their user-level mailbox, not
        // to a session).
        if !is_sub_agent {
            if let Some(r) = args.recipient.as_deref() {
                if is_cross_user_target(r) {
                    return self.execute_cross_user(&args, &text, r, ctx).await;
                }
            }
        }
        if is_sub_agent || args.recipient.is_some() {
            return self
                .execute_agent_message(&args, &text, is_sub_agent, ctx)
                .await;
        }

        // RFC channel-role-split §1.2: resolve via the live registry. On a
        // headless (scheduled) turn this may still be Some when the routing
        // key maps to a real channel — sending an intermediate notice from a
        // background turn is a legitimate capability (the coupling that used
        // to disable it was the bug, cf. RFC §0).
        let channel = match ctx.channel.clone() {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "send_message requires an active channel (not available in sub-agent/scheduled paths)".to_string(),
                    ),
                });
            }
        };

        let reply_target = match ctx.reply_target.as_deref() {
            Some(rt) => rt.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("send_message requires an active receiver".to_string()),
                });
            }
        };

        if !args.files.is_empty() && !channel.supports_file_send() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("current channel does not support file sending".to_string()),
            });
        }

        let mut files = Vec::with_capacity(args.files.len());
        let mut file_names = Vec::with_capacity(args.files.len());
        for file_arg in args.files {
            let file = prepare_file(file_arg).await?;
            file_names.push(file.meta.file_name.clone());
            files.push(file);
        }

        let mut receiver = MessageReceiver::new(reply_target);
        if let Some(ref last_msg) = ctx.last_message {
            // Prefer the inbound receiver's reply_to_message_id (set by
            // QQBot INTERACTION_CREATE for button callbacks) over the
            // generic message id (used by C2C/group passive replies).
            receiver.reply_to_message_id = Some(
                last_msg
                    .receiver
                    .reply_to_message_id
                    .clone()
                    .unwrap_or_else(|| last_msg.id.clone()),
            );
            receiver.thread_id = last_msg.receiver.thread_id.clone();
        }

        let message = ChannelOutboundMessage {
            receiver,
            content: ChannelMessageContent {
                text,
                files,
                buttons: vec![],
            },
            options: SendOptions::default(),
        };

        match channel.send_outbound_message(&message).await {
            Ok(_) => Ok(ToolResult {
                success: true,
                output: if file_names.is_empty() {
                    "message sent".to_string()
                } else {
                    format!(
                        "message sent with {} file(s): {}",
                        file_names.len(),
                        file_names.join(", ")
                    )
                },
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("send failed: {e}")),
            }),
        }
    }
}

async fn prepare_file(file_arg: SendMessageFileArg) -> anyhow::Result<ChannelFile> {
    let path = PathBuf::from(&file_arg.path);
    if !path.exists() {
        anyhow::bail!("file not found: {}", file_arg.path);
    }
    if !path.is_file() {
        anyhow::bail!("not a file: {}", file_arg.path);
    }

    let metadata = tokio::fs::metadata(&path).await?;
    let file_size = metadata.len();
    if file_size > MAX_FILE_SIZE {
        anyhow::bail!(
            "file too large: {} bytes (limit {} bytes)",
            file_size,
            MAX_FILE_SIZE
        );
    }

    let file_name = file_arg.name.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string())
    });
    let mime_type = match file_arg.mime_type {
        Some(mime) => Some(mime),
        None => Some(infer_mime(&file_name, &path).await),
    };

    Ok(ChannelFile {
        meta: ChannelFileMeta {
            file_name,
            mime_type,
            size_bytes: Some(file_size),
            source_url: None,
        },
        body: Arc::new(LocalFileBody::new(path)),
    })
}

async fn infer_mime(file_name: &str, path: &Path) -> String {
    crate::providers::media::infer_mime(file_name, path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tool::ToolContext;

    /// Records agent-to-agent deliveries; always succeeds.
    #[derive(Default)]
    struct MockMessenger {
        to_sub: std::sync::Mutex<Vec<(String, AgentMail)>>,
        to_parent: std::sync::Mutex<Vec<AgentMessage>>,
    }

    #[async_trait]
    impl AgentMessenger for MockMessenger {
        fn send_to_sub_agent(&self, sub_session_id: &str, mail: AgentMail) -> Result<(), String> {
            self.to_sub
                .lock()
                .unwrap()
                .push((sub_session_id.to_string(), mail));
            Ok(())
        }

        async fn send_to_parent(&self, event: AgentMessage) -> bool {
            self.to_parent.lock().unwrap().push(event);
            true
        }
    }

    fn tool_with_messenger(mock: &Arc<MockMessenger>) -> SendMessageTool {
        let tool = SendMessageTool::new();
        tool.set_messenger(Arc::clone(mock) as Arc<dyn AgentMessenger>);
        tool
    }

    fn err_of(r: &ToolResult) -> String {
        r.error.clone().unwrap_or_default()
    }

    fn sub_agent_session() -> ToolContext {
        ToolContext {
            owner: "test".to_string(),
            session_id: "sub_session".to_string(),
            parent_session_id: Some("parent_session".to_string()),
            agent_name: "coder".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn main_sends_to_sub_agent_by_session_id() {
        let mock = Arc::new(MockMessenger::default());
        let tool = tool_with_messenger(&mock);
        let session = ToolContext {
            owner: "test".to_string(),
            session_id: "main".to_string(),
            agent_name: "main".to_string(),
            ..Default::default()
        };

        let r = tool
            .execute(
                serde_json::json!({"text": "继续调研", "recipient": "myclaw/s/mock-123"}),
                &session,
            )
            .await
            .unwrap();
        assert!(r.success, "{}", err_of(&r));

        let delivered = mock.to_sub.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "myclaw/s/mock-123");
        assert_eq!(delivered[0].1.text, "继续调研");
        assert_eq!(delivered[0].1.sender_name, "主 agent");
    }

    #[tokio::test]
    async fn sub_agent_sends_to_parent_with_omitted_or_parent_recipient() {
        let mock = Arc::new(MockMessenger::default());
        let tool = tool_with_messenger(&mock);
        let session = sub_agent_session();

        let r = tool
            .execute(serde_json::json!({"text": "搞定了"}), &session)
            .await
            .unwrap();
        assert!(r.success, "{}", err_of(&r));
        let r2 = tool
            .execute(
                serde_json::json!({"text": "再问一下", "recipient": "parent"}),
                &session,
            )
            .await
            .unwrap();
        assert!(r2.success, "{}", err_of(&r2));

        let sent = mock.to_parent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].sender_name, "coder");
        assert_eq!(sent[0].sub_session_id, "sub_session");
        assert_eq!(sent[0].parent_session_id, "parent_session");
        assert_eq!(sent[0].text, "搞定了");
        assert_eq!(sent[1].text, "再问一下");
    }

    #[tokio::test]
    async fn sub_agent_rejects_foreign_recipient() {
        let mock = Arc::new(MockMessenger::default());
        let tool = tool_with_messenger(&mock);
        let session = sub_agent_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "recipient": "myclaw/s/mock-999"}),
                &session,
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(err_of(&r).contains("invalid recipient"));
        assert!(mock.to_parent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn agent_messages_reject_files() {
        let mock = Arc::new(MockMessenger::default());
        let tool = tool_with_messenger(&mock);
        let session = sub_agent_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "files": [{"path": "/tmp/x"}]}),
                &session,
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(err_of(&r).contains("text only"));
        assert!(mock.to_parent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recipient_without_messenger_errors() {
        // Single-agent mode: no messenger installed → clear error.
        let tool = SendMessageTool::new();
        let session = ToolContext {
            owner: "test".to_string(),
            session_id: "main".to_string(),
            agent_name: "main".to_string(),
            ..Default::default()
        };

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "recipient": "myclaw/t/mock-1"}),
                &session,
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(err_of(&r).contains("single-agent"));
    }

    #[tokio::test]
    async fn over_32k_rejected_before_delivery() {
        let mock = Arc::new(MockMessenger::default());
        let tool = tool_with_messenger(&mock);
        let session = sub_agent_session();
        let long = "x".repeat(MAX_TEXT_CHARS + 1);

        let r = tool
            .execute(serde_json::json!({"text": long}), &session)
            .await
            .unwrap();
        assert!(!r.success);
        assert!(err_of(&r).contains("too long"));
        assert!(mock.to_parent.lock().unwrap().is_empty());
    }

    #[test]
    fn schema_includes_recipient() {
        let tool = SendMessageTool::new();
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["recipient"].is_object());
    }

    // ── P1/P4 cross-user delivery (RFC §3.5) ───────────────────────────────

    const ALICE: &str = "qqbot:xiaoer:alice";
    const BOB: &str = "qqbot:xiaoer:bob";

    /// 注册后取真实 user.id（uid 为系统分配 uuidv7，测试不假设具体值）。
    fn user_id_of(users: &UserRegistry, username: &str) -> String {
        users
            .find_by_username(username)
            .unwrap()
            .user_id(users.namespace())
    }

    /// 登记 alice/bob 两个 User（FQID）+ 绑定各自 rk（P4 前置：gate 依赖
    /// rk → FQID 绑定，联系人键折叠到 FQID）。
    fn registered_users() -> (Arc<KnownUsersRegistry>, Arc<UserRegistry>) {
        let resolver = Arc::new(crate::identity::UserResolver::new());
        let reg = Arc::new(KnownUsersRegistry::in_memory().with_resolver(resolver));
        reg.record("qqbot", "xiaoer", "alice", "default");
        reg.record("qqbot", "xiaoer", "bob", "default");
        let users = Arc::new(UserRegistry::in_memory());
        let alice = users.register("alice@example.com", "alice").unwrap();
        let bob = users.register("bob@example.com", "bob").unwrap();
        reg.resolver()
            .unwrap()
            .set(ALICE, &alice.user_id(users.namespace()));
        reg.resolver()
            .unwrap()
            .set(BOB, &bob.user_id(users.namespace()));
        (reg, users)
    }

    fn registered_friends() -> (Arc<KnownUsersRegistry>, Arc<UserRegistry>) {
        let (reg, users) = registered_users();
        reg.request_friend(ALICE, BOB);
        assert!(reg.accept_friend(BOB, ALICE));
        (reg, users)
    }

    fn tool_for(reg: &Arc<KnownUsersRegistry>, users: &Arc<UserRegistry>) -> SendMessageTool {
        let tool = SendMessageTool::new();
        tool.set_known_users(Arc::clone(reg));
        tool.set_user_registry(Arc::clone(users));
        tool
    }

    fn alice_session() -> ToolContext {
        ToolContext {
            owner: ALICE.to_string(),
            session_id: "s_alice".to_string(),
            agent_name: "main".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn cross_user_delivers_to_friend_mailbox() {
        let (reg, users) = registered_friends();
        let tool = tool_for(&reg, &users);
        let session = alice_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "你好 bob", "recipient": "u/bob"}),
                &session,
            )
            .await
            .unwrap();
        assert!(r.success, "{}", err_of(&r));
        assert!(r.output.contains("delivered"), "{}", r.output);

        // Recipient's user-level mailbox got the mail; drain is once-only.
        let mails = reg.drain_user_mail(BOB);
        assert_eq!(mails.len(), 1);
        assert_eq!(mails[0].text, "你好 bob");
        assert_eq!(mails[0].sender_user_id, user_id_of(&users, "alice"));
        assert_eq!(mails[0].sender_nickname, "@alice");
        assert!(reg.drain_user_mail(BOB).is_empty());
    }

    #[tokio::test]
    async fn cross_user_rejected_when_not_friends() {
        // Registered users but no relationship → NotFriends → guidance error
        // (RFC §4.2: 引导发送好友请求, 框架不自动发)。
        let (reg, users) = registered_users();
        let tool = tool_for(&reg, &users);
        let session = alice_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "recipient": "u/bob"}),
                &session,
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(err_of(&r).contains("not friends"), "{}", err_of(&r));
        assert!(err_of(&r).contains("friend_request"), "{}", err_of(&r));
        assert!(reg.drain_user_mail(BOB).is_empty());
    }

    #[tokio::test]
    async fn cross_user_rejected_when_blocked() {
        // Friends first, then bob blocks alice → delivery blocked both ways.
        let (reg, users) = registered_friends();
        reg.block_friend(BOB, ALICE);
        let tool = tool_for(&reg, &users);
        let session = alice_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "recipient": "u/bob"}),
                &session,
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(err_of(&r).contains("blocked"), "{}", err_of(&r));
        assert!(reg.drain_user_mail(BOB).is_empty());
    }

    #[tokio::test]
    async fn cross_user_reply_loop_back_to_sender() {
        // RFC §6 P2 回复转发闭环: bob 收到后回复 → 同链反向 → alice 的用户级
        // mailbox → alice 下一条用户消息注入。双向各收 1 条。
        let (reg, users) = registered_friends();
        let tool = tool_for(&reg, &users);
        let alice_session = alice_session();
        let bob_session = ToolContext {
            owner: BOB.to_string(),
            session_id: "s_bob".to_string(),
            agent_name: "main".to_string(),
            ..Default::default()
        };

        // alice → bob
        let r = tool
            .execute(
                serde_json::json!({"text": "在吗", "recipient": "u/bob"}),
                &alice_session,
            )
            .await
            .unwrap();
        assert!(r.success, "{}", err_of(&r));
        assert_eq!(reg.drain_user_mail(BOB).len(), 1);

        // bob 回复 → alice
        let r = tool
            .execute(
                serde_json::json!({"text": "在的，什么事", "recipient": "u/alice"}),
                &bob_session,
            )
            .await
            .unwrap();
        assert!(r.success, "{}", err_of(&r));
        let mails = reg.drain_user_mail(ALICE);
        assert_eq!(mails.len(), 1);
        assert_eq!(mails[0].text, "在的，什么事");
        assert_eq!(mails[0].sender_user_id, user_id_of(&users, "bob"));
        assert_eq!(mails[0].sender_nickname, "@bob");
    }

    #[tokio::test]
    async fn cross_user_folds_linked_identity() {
        // P3/P4 身份绑定: alice 把 telegram 渠道绑定到 FQID 后，从新渠道
        // 发消息走同一好友关系；sender 与 mailbox 都按"人"折叠。
        let (reg, users) = registered_friends();
        reg.resolver()
            .unwrap()
            .set("telegram:default:alice_tg", user_id_of(&users, "alice"));
        let tool = tool_for(&reg, &users);
        let session = ToolContext {
            owner: "telegram:default:alice_tg".to_string(),
            session_id: "s_tg".to_string(),
            agent_name: "main".to_string(),
            ..Default::default()
        };

        let r = tool
            .execute(
                serde_json::json!({"text": "hi from tg", "recipient": "u/bob"}),
                &session,
            )
            .await
            .unwrap();
        assert!(r.success, "{}", err_of(&r));
        let mails = reg.drain_user_mail(BOB);
        assert_eq!(mails.len(), 1);
        // 发送者身份折叠: 存 alice 身份而非 telegram rk。
        assert_eq!(mails[0].sender_user_id, user_id_of(&users, "alice"));
        assert_eq!(mails[0].sender_nickname, "@alice");
    }
}
