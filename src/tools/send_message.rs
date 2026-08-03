//! `send_message` tool — send text and/or local files to the user.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::session::Session;
use crate::channels::{
    ChannelFile, ChannelFileMeta, ChannelMessageContent, ChannelOutboundMessage, LocalFileBody,
    MessageReceiver, SendOptions,
};
use crate::providers::{Tool, ToolResult};

const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB

pub struct SendMessageTool;

impl Default for SendMessageTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SendMessageTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct SendMessageArgs {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Vec<SendMessageFileArg>,
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
        "Send a message to the current conversation. Supports plain text, text with local files, or multiple local files. \
         File parameters only accept local paths (not URLs). Paths are resolved by the tool and not exposed to the channel."
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
        if text.trim().is_empty() && args.files.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("send_message requires text or files".to_string()),
            });
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
