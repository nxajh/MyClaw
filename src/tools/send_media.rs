//! `send_media` tool — send a local file (image/document/audio/video) to the user.
//!
//! The tool reads a file from disk, validates it, and sends it through the
//! active channel via `Channel::send_payload(MessagePayload::Media)`.
//! This mirrors the `ask_user` pattern: the tool accesses `session.channel`
//! directly and sends immediately during the turn.

use async_trait::async_trait;
use std::path::Path;

use crate::agents::session::Session;
use crate::channels::message::{MediaSource, MessagePayload, SendTarget};
use crate::providers::{Tool, ToolResult};
use serde_json::json;

const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB

pub struct SendMediaTool;

impl SendMediaTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SendMediaTool {
    fn name(&self) -> &str {
        "send_media"
    }

    fn description(&self) -> &str {
        "将本地文件发送给用户。支持图片、文档、音频、视频等。\
         调用后文件会立即发送到当前对话。\
         仅支持本地文件路径，不支持 URL。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要发送的本地文件路径。"
                },
                "caption": {
                    "type": "string",
                    "description": "可选，文件/图片的说明文字。"
                }
            },
            "required": ["path"]
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
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;

        let caption = args["caption"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(String::from);

        let path = Path::new(path_str);

        // Pre-flight checks
        if !path.exists() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("文件不存在: {path_str}")),
            });
        }
        if !path.is_file() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("不是文件: {path_str}")),
            });
        }

        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("无法读取文件信息: {e}")),
                });
            }
        };

        let file_size = metadata.len();
        if file_size > MAX_FILE_SIZE {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "文件过大: {} bytes (上限 {} bytes)",
                    file_size, MAX_FILE_SIZE
                )),
            });
        }

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("读取文件失败: {e}")),
                });
            }
        };

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        let mime_type = infer_mime(&file_name, &data);

        // Channel and reply target
        let channel = match session.channel.as_ref() {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "send_media requires an active channel (not available in sub-agent/scheduled paths)".to_string(),
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
                    error: Some("send_media requires an active reply_target".to_string()),
                });
            }
        };

        let mut target = SendTarget::new(&reply_target);
        // Carry the original inbound message ID as a passive reply reference
        // so QQ Bot doesn't consume the active message quota.
        if let Some(ref last_msg) = session.last_message {
            target = target.with_thread(&last_msg.id);
        }
        let payload = MessagePayload::Media {
            source: MediaSource::Inline {
                data,
                mime_type: Some(mime_type),
                file_name: Some(file_name.clone()),
            },
            caption,
        };

        match channel.send_payload(&target, &payload).await {
            Ok(_) => Ok(ToolResult {
                success: true,
                output: format!("已发送文件: {} ({} bytes)", file_name, file_size),
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

/// Infer MIME type from file extension and magic bytes.
fn infer_mime(file_name: &str, data: &[u8]) -> String {
    // Try extension first
    let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => return "image/png".to_string(),
        "jpg" | "jpeg" => return "image/jpeg".to_string(),
        "gif" => return "image/gif".to_string(),
        "webp" => return "image/webp".to_string(),
        "mp4" => return "video/mp4".to_string(),
        "mp3" => return "audio/mpeg".to_string(),
        "wav" => return "audio/wav".to_string(),
        "ogg" => return "audio/ogg".to_string(),
        "flac" => return "audio/flac".to_string(),
        "pdf" => return "application/pdf".to_string(),
        "zip" => return "application/zip".to_string(),
        "txt" => return "text/plain".to_string(),
        "json" => return "application/json".to_string(),
        "csv" => return "text/csv".to_string(),
        _ => {}
    }

    // Fallback: magic bytes
    if data.len() >= 4 {
        if &data[0..4] == b"\x89PNG" {
            return "image/png".to_string();
        }
        if &data[0..2] == b"\xff\xd8" {
            return "image/jpeg".to_string();
        }
        if &data[0..4] == b"GIF8" {
            return "image/gif".to_string();
        }
        if &data[0..4] == b"RIFF" && data.len() >= 12 && &data[8..12] == b"WAVE" {
            return "audio/wav".to_string();
        }
        if &data[0..3] == b"ID3" || (&data[0..2] == b"\xff\xf3" || &data[0..2] == b"\xff\xf2") {
            return "audio/mpeg".to_string();
        }
        if &data[0..5] == b"%PDF-" {
            return "application/pdf".to_string();
        }
    }

    "application/octet-stream".to_string()
}
