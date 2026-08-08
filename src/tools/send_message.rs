//! `send_message` tool — send text and/or local files to the user, or
//! route messages between parent and sub agents (RFC agent-messaging §3).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::agents::session::Session;
use crate::agents::{AgentMail, AgentMessenger, DelegationEvent};
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
        }
    }

    /// Install the agent-to-agent message bus (called by the daemon after
    /// the `DelegationCoordinator` exists; set-once).
    pub fn set_messenger(&self, messenger: Arc<dyn AgentMessenger>) {
        let _ = self.messenger.set(messenger);
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
            let event = DelegationEvent::Message {
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

        let text = args.text.unwrap_or_default();
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
        to_parent: std::sync::Mutex<Vec<DelegationEvent::Message>>,
    }

    #[async_trait]
    impl AgentMessenger for MockMessenger {
        fn send_to_sub_agent(&self, task_id: &str, mail: AgentMail) -> Result<(), String> {
            self.to_sub.lock().unwrap().push((task_id.to_string(), mail));
            Ok(())
        }

        async fn send_to_parent(&self, event: DelegationEvent::Message) -> bool {
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
}
