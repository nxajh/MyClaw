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
        "向当前对话发送消息。支持纯文本、文本加本地文件、或多个本地文件。\
         文件参数只接受本地路径，不支持 URL；路径只由工具解析，不会暴露给 channel。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "要发送给用户的文本。可单独发送，也可作为文件说明。"
                },
                "files": {
                    "type": "array",
                    "description": "要发送的本地文件列表。仅支持本地路径，不支持 URL。",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "本地文件路径。"
                            },
                            "name": {
                                "type": "string",
                                "description": "可选，发送时显示的文件名。"
                            },
                            "mime_type": {
                                "type": "string",
                                "description": "可选，文件 MIME 类型；未提供时由工具推断。"
                            }
                        },
                        "required": ["path"]
                    }
                }
            },
            "anyOf": [
                { "required": ["text"] },
                { "required": ["files"] }
            ]
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
                    error: Some(format!("参数错误: {e}")),
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
            receiver.reply_to_message_id = Some(last_msg.id.clone());
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
                    "已发送消息。".to_string()
                } else {
                    format!(
                        "已发送消息，包含 {} 个文件：{}。",
                        file_names.len(),
                        file_names.join(", ")
                    )
                },
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("发送失败: {e}")),
            }),
        }
    }
}

async fn prepare_file(file_arg: SendMessageFileArg) -> anyhow::Result<ChannelFile> {
    let path = PathBuf::from(&file_arg.path);
    if !path.exists() {
        anyhow::bail!("文件不存在: {}", file_arg.path);
    }
    if !path.is_file() {
        anyhow::bail!("不是文件: {}", file_arg.path);
    }

    let metadata = tokio::fs::metadata(&path).await?;
    let file_size = metadata.len();
    if file_size > MAX_FILE_SIZE {
        anyhow::bail!(
            "文件过大: {} bytes (上限 {} bytes)",
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
        },
        body: Arc::new(LocalFileBody::new(path)),
    })
}

async fn infer_mime(file_name: &str, path: &Path) -> String {
    crate::providers::media::infer_mime(file_name, path).await
}
