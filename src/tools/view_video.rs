//! `view_video` tool — lets a model inspect a video file by delegating to a
//! video-capable model in the chat routing chain.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, ChatResponse, ContentPart};
use crate::providers::provider_registry::ProviderRegistry;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

pub struct ViewVideoTool {
    providers: Arc<dyn ProviderRegistry>,
}

impl ViewVideoTool {
    pub fn new(providers: Arc<dyn ProviderRegistry>) -> Self {
        Self { providers }
    }
}

fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

fn infer_video_mime(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        "mkv" => Some("video/x-matroska"),
        _ => None,
    }
}

#[async_trait]
impl Tool for ViewVideoTool {
    fn name(&self) -> &str {
        "view_video"
    }

    fn description(&self) -> &str {
        "查看视频文件内容。当对话中出现 `[视频: sessions/.../files/xxx]` 标记时，调用本工具并传入 path 与具体问题。path 可以是 workspace-relative 路径或绝对路径。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "视频文件路径；相对路径按 workspace-relative 解释，绝对路径直接使用。" },
                "question": { "type": "string", "description": "你想从这段视频了解的具体问题，例如『总结视频内容』『视频里发生了什么？』『识别视频中的文字』。" }
            },
            "required": ["path", "question"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        2_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"].as_str().unwrap_or("").trim();
        if path.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("缺少 path 参数。".to_string()),
            });
        }
        let question = args["question"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("请详细描述这段视频的内容，包括主要事件、人物、场景、文字和声音。")
            .to_string();

        let abs = resolve_path(path);
        let meta = match std::fs::metadata(&abs) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("路径不是普通文件：{path}")),
                });
            }
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("无法访问视频文件 {path}：{e}")),
                });
            }
        };
        if meta.len() > 200 * 1024 * 1024 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("视频文件过大，当前 view_video 限制为 200MB。".to_string()),
            });
        }

        let Some((provider, model_id)) = self
            .providers
            .find_chat_model_with_modality(Modality::Video)
        else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("当前没有可用的视频模型，无法查看视频。".to_string()),
            });
        };

        let user_msg = ChatMessage {
            role: "user".into(),
            parts: vec![
                ContentPart::File {
                    path: path.to_string(),
                    mime_type: infer_video_mime(path).map(str::to_string),
                    name: Path::new(path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(str::to_string),
                    size_bytes: Some(meta.len()),
                },
                ContentPart::Text { text: question },
            ],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        };
        let messages = [user_msg];
        let req = ChatRequest {
            model: &model_id,
            messages: &messages,
            temperature: Some(0.3),
            max_tokens: Some(1536),
            thinking: None,
            stop: None,
            seed: None,
            tools: None,
            stream: true,
        };
        let stream = match provider.chat(req) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("视频模型调用失败：{e}")),
                });
            }
        };
        let text = match ChatResponse::from_stream(stream).await {
            Ok(resp) => resp.text,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("视频模型响应失败：{e}")),
                });
            }
        };
        if text.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("视频模型返回了空结果。".to_string()),
            });
        }
        Ok(ToolResult {
            success: true,
            output: text,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_path_against_current_dir() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            resolve_path("sessions/s/files/clip.mp4"),
            cwd.join("sessions/s/files/clip.mp4")
        );
    }

    #[test]
    fn infers_video_mime_from_extension() {
        assert_eq!(infer_video_mime("x.MP4"), Some("video/mp4"));
        assert_eq!(infer_video_mime("x.webm"), Some("video/webm"));
        assert_eq!(infer_video_mime("x.png"), None);
    }
}
