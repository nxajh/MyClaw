//! `send_message` tool — send text and/or local files to the user, or
//! route messages between parent and sub agents (RFC agent-messaging §3).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::agents::session::Session;
use crate::agents::{AgentMail, AgentMessage, AgentMessenger, KnownUsersRegistry};
use crate::channels::{
    ChannelFile, ChannelFileMeta, ChannelMessageContent, ChannelOutboundMessage, LocalFileBody,
    MessageReceiver, SendOptions,
};
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
    /// `@nick` → contacts check → recipient's user-level mailbox). Set by
    /// the daemon; `None` in tests/single-agent mode makes `@nick` targets
    /// error out clearly.
    known_users: Arc<OnceLock<Arc<KnownUsersRegistry>>>,
}

impl Default for SendMessageTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SendMessageTool {
    pub fn new() -> Self {
        Self {
            messenger: Arc::new(OnceLock::new()),
            known_users: Arc::new(OnceLock::new()),
        }
    }

    /// Install the agent-to-agent message bus (called by the daemon after
    /// the `DelegationCoordinator` exists; set-once).
    pub fn set_messenger(&self, messenger: Arc<dyn AgentMessenger>) {
        let _ = self.messenger.set(messenger);
    }

    /// Install the known-users registry (called by the daemon after the
    /// registry is loaded; set-once). Enables `recipient=@nick` cross-user
    /// delivery (RFC §3.5).
    pub fn set_known_users(&self, known_users: Arc<KnownUsersRegistry>) {
        let _ = self.known_users.set(known_users);
    }

    /// Agent-to-agent delivery (RFC agent-messaging §3).
    ///
    /// - Sub-agent context: the only legal target is the parent main agent
    ///   (`recipient` omitted or "parent"); any other value errors (§3.3 —
    ///   the sub-agent sees no other address).
    /// - Main context: `recipient` must be the task_id of a running async
    ///   sub-agent; delivery goes through the coordinator's mailbox.
    ///
    /// Agent messages are text-only (P0 scope — no file transfer).
    async fn execute_agent_message(
        &self,
        args: &SendMessageArgs,
        text: &str,
        is_sub_agent: bool,
        session: &Session,
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
            let Some(task_id) = session.sub_agent_task_id.clone() else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("sub-agent identity missing — cannot message parent".to_string()),
                });
            };
            let Some(parent_session_id) = session.parent_session_id.clone() else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("parent session missing — cannot message parent".to_string()),
                });
            };
            let event = AgentMessage {
                msg_id: uuid::Uuid::new_v4().to_string(),
                sender_name: session.agent_name.clone(),
                task_id,
                session_id: parent_session_id,
                text: text.to_string(),
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

        // Main agent context: `recipient` = sub-agent task_id.
        let task_id = match args.recipient.as_deref() {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "recipient required in the main agent context (a sub-agent task_id)"
                            .to_string(),
                    ),
                });
            }
        };
        let mail = AgentMail {
            msg_id: uuid::Uuid::new_v4().to_string(),
            sender_name: "主 agent".to_string(),
            text: text.to_string(),
            timestamp: chrono::Utc::now().timestamp() as u64,
        };
        match messenger.send_to_sub_agent(&task_id, mail) {
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

    /// Cross-user delivery (RFC §3.5): `recipient=@nick` from the main agent
    /// context. Resolves the nick (own contacts first, then known users,
    /// with explicit disambiguation on duplicates), checks the delivery
    /// verdict against the recipient's contacts table (§4.1), and on
    /// `Allowed` writes into the recipient's **user-level mailbox** —
    /// delivered on the recipient's next user interaction (注入即消费), not
    /// a wake-up. Text-only (P1 scope — no file transfer). Returns an Ack
    /// to the sender, mirroring §3.5's "已投递(Ack)".
    async fn execute_cross_user(
        &self,
        args: &SendMessageArgs,
        text: &str,
        recipient: &str,
        session: &Session,
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

        let owner = session.owner.clone();
        let nick = recipient.trim_start_matches('@');
        let peer =
            match crate::agents::commands::friends::resolve_nick_for(known_users, &owner, nick) {
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
                error: Some("cannot message yourself".to_string()),
            });
        }
        match known_users.delivery_verdict(&owner, &peer) {
            crate::agents::DeliveryVerdict::Allowed => {
                known_users.push_user_mail(
                    &peer,
                    crate::agents::UserMail {
                        msg_id: uuid::Uuid::new_v4().to_string(),
                        sender_user_id: owner.clone(),
                        sender_nickname: KnownUsersRegistry::nick_of(&owner),
                        text: text.to_string(),
                        sent_at: chrono::Utc::now().timestamp_millis() as u64,
                    },
                );
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "message delivered to {} — shown on their next interaction",
                        KnownUsersRegistry::nick_of(&peer)
                    ),
                    error: None,
                })
            }
            crate::agents::DeliveryVerdict::Blocked => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("the recipient {} has blocked you", recipient)),
            }),
            // RFC §4.2 工具通道: 未建立关系 → 返回"发送好友请求?"引导,由 bot
            // 确认后调 friend_request 工具;框架不自动发请求。
            crate::agents::DeliveryVerdict::NotFriends => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "you are not friends with {recipient} yet — ask the user whether to send a friend request (use the friend_request tool)"
                )),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct SendMessageArgs {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Vec<SendMessageFileArg>,
    /// Optional agent-to-agent target. Parent context: a sub-agent task_id.
    /// Sub-agent context: omitted or "parent".
    #[serde(default)]
    recipient: Option<String>,
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
        "Send a message to the current user or to another agent. Supports plain text, text with local files, or multiple local files. \
         File parameters only accept local paths (not URLs); paths are resolved by the tool and not exposed to the channel. \
         Optional `recipient`: in the main agent context, pass a sub-agent task_id (from agent_delegate mode=async) to message that sub-agent; \
         in a sub-agent context, omit it or use \"parent\" to message the main agent. Agent-to-agent messages are text-only (32K chars max)."
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
                    "description": "Optional agent-to-agent target. Main agent context: a sub-agent's task_id (from agent_delegate mode=async). Sub-agent context: omit or \"parent\" to send to the main agent."
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
        session: &Session,
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
        // agent targets a running async sub-agent by task_id.
        let is_sub_agent = session.parent_session_id.is_some();
        // RFC §3.5: cross-user path first — `recipient=@nick` in the main
        // agent context targets another user through the friend contacts
        // table (delivered to their user-level mailbox, not to a session).
        if !is_sub_agent {
            if let Some(r) = args.recipient.as_deref() {
                if r.trim_start().starts_with('@') {
                    return self.execute_cross_user(&args, &text, r, session).await;
                }
            }
        }
        if is_sub_agent || args.recipient.is_some() {
            return self
                .execute_agent_message(&args, &text, is_sub_agent, session)
                .await;
        }

        let channel = match session.channel.as_ref() {
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

        let reply_target = match session.reply_target() {
            Some(rt) => rt.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("send_message requires an active receiver".to_string()),
                });
            }
        };

        if !args.files.is_empty() && !channel.capabilities().supports_file_send {
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
        if let Some(ref last_msg) = session.last_message {
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

        match channel.send_message(&message).await {
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
    use crate::agents::session::Session;

    /// Records agent-to-agent deliveries; always succeeds.
    #[derive(Default)]
    struct MockMessenger {
        to_sub: std::sync::Mutex<Vec<(String, AgentMail)>>,
        to_parent: std::sync::Mutex<Vec<AgentMessage>>,
    }

    #[async_trait]
    impl AgentMessenger for MockMessenger {
        fn send_to_sub_agent(&self, task_id: &str, mail: AgentMail) -> Result<(), String> {
            self.to_sub.lock().unwrap().push((task_id.to_string(), mail));
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

    fn sub_agent_session() -> Session {
        let mut session = Session::new("sub_session".into());
        session.parent_session_id = Some("parent_session".into());
        session.sub_agent_task_id = Some("del_1".into());
        session.agent_name = "coder".into();
        session
    }

    #[tokio::test]
    async fn main_sends_to_sub_agent_by_task_id() {
        let mock = Arc::new(MockMessenger::default());
        let tool = tool_with_messenger(&mock);
        let session = Session::new("main".into());

        let r = tool
            .execute(
                serde_json::json!({"text": "继续调研", "recipient": "del_123"}),
                &session,
            )
            .await
            .unwrap();
        assert!(r.success, "{}", err_of(&r));

        let delivered = mock.to_sub.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "del_123");
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
        assert_eq!(sent[0].task_id, "del_1");
        assert_eq!(sent[0].session_id, "parent_session");
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
                serde_json::json!({"text": "hi", "recipient": "del_999"}),
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
        let session = Session::new("main".into());

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "recipient": "del_1"}),
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

    // ── P1 cross-user delivery (RFC §3.5) ────────────────────────────────────

    const ALICE: &str = "qqbot:xiaoer:alice";
    const BOB: &str = "qqbot:xiaoer:bob";

    fn registered_friends() -> Arc<KnownUsersRegistry> {
        let reg = Arc::new(KnownUsersRegistry::in_memory());
        reg.record("qqbot", "xiaoer", "alice", "default");
        reg.record("qqbot", "xiaoer", "bob", "default");
        reg.request_friend(ALICE, BOB);
        assert!(reg.accept_friend(BOB, ALICE));
        reg
    }

    fn alice_session() -> Session {
        let mut session = Session::new("s_alice".into());
        session.owner = ALICE.to_string();
        session
    }

    #[tokio::test]
    async fn cross_user_delivers_to_friend_mailbox() {
        let reg = registered_friends();
        let tool = SendMessageTool::new();
        tool.set_known_users(Arc::clone(&reg));
        let session = alice_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "你好 bob", "recipient": "@bob"}),
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
        assert_eq!(mails[0].sender_user_id, ALICE);
        assert_eq!(mails[0].sender_nickname, "@alice");
        assert!(reg.drain_user_mail(BOB).is_empty());
    }

    #[tokio::test]
    async fn cross_user_rejected_when_not_friends() {
        // Registered users but no relationship → NotFriends → guidance error
        // (RFC §4.2: 引导发送好友请求, 框架不自动发)。
        let reg = Arc::new(KnownUsersRegistry::in_memory());
        reg.record("qqbot", "xiaoer", "alice", "default");
        reg.record("qqbot", "xiaoer", "bob", "default");
        let tool = SendMessageTool::new();
        tool.set_known_users(Arc::clone(&reg));
        let session = alice_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "recipient": "@bob"}),
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
        let reg = registered_friends();
        reg.block_friend(BOB, ALICE);
        let tool = SendMessageTool::new();
        tool.set_known_users(Arc::clone(&reg));
        let session = alice_session();

        let r = tool
            .execute(
                serde_json::json!({"text": "hi", "recipient": "@bob"}),
                &session,
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(err_of(&r).contains("blocked"), "{}", err_of(&r));
        assert!(reg.drain_user_mail(BOB).is_empty());
    }
}
