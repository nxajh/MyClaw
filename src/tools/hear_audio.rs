//! `hear_audio` tool — lets a model inspect an audio file by delegating to an
//! audio-capable model in the chat routing chain.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, ChatResponse, ContentPart};
use crate::providers::provider_registry::ProviderRegistry;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

pub struct HearAudioTool {
    providers: Arc<dyn ProviderRegistry>,
}

impl HearAudioTool {
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

fn infer_audio_mime(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "ogg" => Some("audio/ogg"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "flac" => Some("audio/flac"),
        "m4a" => Some("audio/m4a"),
        _ => None,
    }
}

#[async_trait]
impl Tool for HearAudioTool {
    fn name(&self) -> &str {
        "hear_audio"
    }

    fn description(&self) -> &str {
        "听取语音/音频文件内容。当对话中出现 `[语音: sessions/.../files/xxx]` 标记时，调用本工具并传入 path。path 可以是 workspace-relative 路径或绝对路径。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "音频文件路径；相对路径按 workspace-relative 解释，绝对路径直接使用。" },
                "question": { "type": "string", "description": "你想从这段语音了解的问题，例如『用户说了什么？』『翻译成英文』。留空则转写全文。" }
            },
            "required": ["path"]
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
            .unwrap_or("请逐字转写这段语音的内容。")
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
                    error: Some(format!("无法访问音频文件 {path}：{e}")),
                });
            }
        };
        if meta.len() > 50 * 1024 * 1024 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("音频文件过大，当前 hear_audio 限制为 50MB。".to_string()),
            });
        }

        let Some((provider, model_id)) = self
            .providers
            .find_chat_model_with_modality(Modality::Audio)
        else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("当前没有可用的语音模型，无法听取语音。".to_string()),
            });
        };

        let user_msg = ChatMessage {
            role: "user".into(),
            parts: vec![
                ContentPart::File {
                    path: path.to_string(),
                    mime_type: infer_audio_mime(path).map(str::to_string),
                    name: std::path::Path::new(path)
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
            temperature: Some(0.2),
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
                    error: Some(format!("语音模型调用失败：{e}")),
                });
            }
        };
        let text = match ChatResponse::from_stream(stream).await {
            Ok(resp) => resp.text,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("语音模型响应失败：{e}")),
                });
            }
        };
        if text.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("语音模型返回了空结果。".to_string()),
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
            resolve_path("sessions/s/files/voice.ogg"),
            cwd.join("sessions/s/files/voice.ogg")
        );
    }

    #[test]
    fn infers_audio_mime_from_extension() {
        assert_eq!(infer_audio_mime("x.OGG"), Some("audio/ogg"));
        assert_eq!(infer_audio_mime("x.mp3"), Some("audio/mpeg"));
        assert_eq!(infer_audio_mime("x.png"), None);
    }
}
