//! `view_video` tool — lets a model inspect a video file by delegating to a
//! video-capable model in the chat routing chain.

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{
    ChatMessage, ChatProvider, ChatRequest, ChatResponse, ContentPart,
};
use crate::providers::provider_registry::ProviderRegistry;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

/// Video analysis needs more time than generic tools: base64-encoding a
/// multi-MB file, uploading it, and waiting for the model to reason over
/// frames/audio can easily exceed the default 180 s.
const VIDEO_TIMEOUT_SECS: u64 = 300;

pub struct ViewVideoTool {
    providers: Arc<dyn ProviderRegistry>,
}

impl ViewVideoTool {
    pub fn new(providers: Arc<dyn ProviderRegistry>) -> Self {
        Self { providers }
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

/// Try each candidate in order. On HTTP 4xx, fall back to the next model.
async fn try_with_fallback(
    candidates: &[(Arc<dyn ChatProvider>, String)],
    messages: &[ChatMessage],
    file_path: &str,
    file_size_mb: f64,
) -> anyhow::Result<ToolResult> {
    let total = candidates.len();
    for (i, (provider, model_id)) in candidates.iter().enumerate() {
        let req = ChatRequest {
            model: model_id,
            messages,
            temperature: Some(0.3),
            max_tokens: None,
            thinking: None,
            stop: None,
            seed: None,
            tools: None,
            stream: true,
        };
        let t0 = std::time::Instant::now();
        let stream = match provider.chat(req) {
            Ok(s) => s,
            Err(e) => {
                let err_str = e.to_string();
                let is_http_4xx = err_str.contains("HTTP 4");
                tracing::warn!(
                    path = %file_path,
                    size_mb = %format!("{:.1}", file_size_mb),
                    model = %model_id,
                    attempt = i + 1,
                    total,
                    elapsed_ms = t0.elapsed().as_millis(),
                    err = %e,
                    "view_video: provider.chat() failed"
                );
                if is_http_4xx && i + 1 < total {
                    tracing::info!(
                        model = %model_id,
                        next_model = %candidates[i + 1].1,
                        "view_video: HTTP 4xx, falling back to next model"
                    );
                    continue;
                }
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("video model call failed: {e}")),
                });
            }
        };
        let resp = match ChatResponse::from_stream(stream).await {
            Ok(resp) => resp,
            Err(e) => {
                let err_str = e.to_string();
                let is_http_4xx = err_str.contains("HTTP 4");
                tracing::warn!(
                    path = %file_path,
                    size_mb = %format!("{:.1}", file_size_mb),
                    model = %model_id,
                    attempt = i + 1,
                    total,
                    elapsed_ms = t0.elapsed().as_millis(),
                    err = %e,
                    "view_video: stream collection failed"
                );
                if is_http_4xx && i + 1 < total {
                    tracing::info!(
                        model = %model_id,
                        next_model = %candidates[i + 1].1,
                        "view_video: HTTP 4xx, falling back to next model"
                    );
                    continue;
                }
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("video model response failed: {e}")),
                });
            }
        };

        let elapsed_ms = t0.elapsed().as_millis();
        let input_tokens = resp.usage.as_ref().and_then(|u| u.input_tokens);
        let output_tokens = resp.usage.as_ref().and_then(|u| u.output_tokens);

        if resp.text.trim().is_empty() {
            tracing::warn!(
                path = %file_path,
                size_mb = %format!("{:.1}", file_size_mb),
                model = %model_id,
                attempt = i + 1,
                total,
                elapsed_ms,
                stop_reason = ?resp.stop_reason,
                input_tokens,
                output_tokens,
                has_reasoning = resp.reasoning_content.is_some(),
                reasoning_len = resp.reasoning_content.as_ref().map(|r| r.len()).unwrap_or(0),
                "view_video: empty result — model returned no text content"
            );
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("video model returned empty result".to_string()),
            });
        }

        tracing::info!(
            path = %file_path,
            size_mb = %format!("{:.1}", file_size_mb),
            model = %model_id,
            attempt = i + 1,
            total,
            elapsed_ms,
            stop_reason = ?resp.stop_reason,
            input_tokens,
            output_tokens,
            text_len = resp.text.len(),
            "view_video: success"
        );
        return Ok(ToolResult {
            success: true,
            output: resp.text,
            error: None,
        });
    }

    Ok(ToolResult {
        success: false,
        output: String::new(),
        error: Some("no video models available".to_string()),
    })
}

#[async_trait]
impl Tool for ViewVideoTool {
    fn name(&self) -> &str {
        "view_video"
    }

    fn description(&self) -> &str {
        "View video file content. When the conversation contains a `[视频: sessions/.../files/xxx]` or `[video: sessions/.../files/xxx]` marker, call this tool with the path and a specific question. Only use it for video files; do not use it for image or audio. Path can be workspace-relative, absolute, or a URL (http/https)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Video file path or URL. Relative paths are interpreted as workspace-relative; absolute paths are used directly; URLs (http/https) are downloaded automatically." },
                "question": { "type": "string", "description": "The specific question you want answered about this video, e.g. 'summarize the video', 'what happened?', 'identify text in the video'." }
            },
            "required": ["path", "question"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        2_000
    }

    fn preferred_timeout_secs(&self) -> Option<u64> {
        Some(VIDEO_TIMEOUT_SECS)
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
                error: Some("missing 'path' parameter".to_string()),
            });
        }
        let question = args["question"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Describe the video content in detail, including main events, people, scenes, text, and audio.")
            .to_string();

        let abs = crate::tools::media_download::resolve_path_or_url(path)
            .await
            .map_err(|e| anyhow::anyhow!("cannot resolve path/URL '{path}': {e}"))?;
        let meta = match std::fs::metadata(&abs) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("path is not a regular file: {path}")),
                });
            }
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("cannot access video file {path}: {e}")),
                });
            }
        };
        if meta.len() > 200 * 1024 * 1024 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("video file too large, view_video limit is 200MB".to_string()),
            });
        }
        if crate::providers::modality_from_mime(infer_video_mime(path), path)
            != crate::providers::FileModality::Video
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("view_video only accepts video files: {path}")),
            });
        }

        let candidates = self
            .providers
            .find_all_chat_models_with_modality(Modality::Video);
        if candidates.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("no video models available, cannot view video".to_string()),
            });
        }

        let file_size_mb = meta.len() as f64 / (1024.0 * 1024.0);
        tracing::info!(
            path = %abs.display(),
            size_mb = %format!("{:.1}", file_size_mb),
            candidates = candidates.len(),
            models = ?candidates.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>(),
            question = %question,
            "view_video: starting analysis"
        );

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
            model: None,
            usage: None,
        };
        let messages = [user_msg];

        try_with_fallback(
            &candidates,
            &messages,
            &abs.display().to_string(),
            file_size_mb,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_relative_path_against_current_dir() {
        let cwd = std::env::current_dir().unwrap();
        let result = crate::tools::media_download::resolve_path_or_url("sessions/s/files/clip.mp4")
            .await
            .unwrap();
        assert_eq!(result, cwd.join("sessions/s/files/clip.mp4"));
    }

    #[test]
    fn infers_video_mime_from_extension() {
        assert_eq!(infer_video_mime("x.MP4"), Some("video/mp4"));
        assert_eq!(infer_video_mime("x.webm"), Some("video/webm"));
        assert_eq!(infer_video_mime("x.png"), None);
    }
}
