//! `hear_audio` tool — lets a model inspect an audio file by delegating to an
//! audio-capable model in the chat routing chain.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest, ChatResponse, ContentPart};
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
            temperature: Some(0.2),
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
                    "hear_audio: provider.chat() failed"
                );
                if is_http_4xx && i + 1 < total {
                    tracing::info!(
                        model = %model_id,
                        next_model = %candidates[i + 1].1,
                        "hear_audio: HTTP 4xx, falling back to next model"
                    );
                    continue;
                }
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("audio model call failed: {e}")),
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
                    "hear_audio: stream collection failed"
                );
                if is_http_4xx && i + 1 < total {
                    tracing::info!(
                        model = %model_id,
                        next_model = %candidates[i + 1].1,
                        "hear_audio: HTTP 4xx, falling back to next model"
                    );
                    continue;
                }
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("audio model response failed: {e}")),
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
                "hear_audio: empty result"
            );
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("audio model returned empty result".to_string()),
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
            "hear_audio: success"
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
        error: Some("no audio models available".to_string()),
    })
}

#[async_trait]
impl Tool for HearAudioTool {
    fn name(&self) -> &str {
        "hear_audio"
    }

    fn description(&self) -> &str {
        "Listen to voice/audio file content. When the conversation contains a `[voice: sessions/.../files/xxx]` marker, call this tool with the path. Path can be workspace-relative, absolute, or a URL (http/https)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Audio file path or URL. Relative paths are interpreted as workspace-relative; absolute paths are used directly; URLs (http/https) are downloaded automatically." },
                "question": { "type": "string", "description": "The question you want answered about this audio, e.g. 'what did the user say?', 'translate to English'. Leave empty for full transcription." }
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
                error: Some("missing 'path' parameter".to_string()),
            });
        }
        let question = args["question"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Transcribe this audio verbatim.")
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
                    error: Some(format!("cannot access audio file {path}: {e}")),
                });
            }
        };
        if meta.len() > 50 * 1024 * 1024 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("audio file too large, hear_audio limit is 50MB".to_string()),
            });
        }

        let candidates = self
            .providers
            .find_all_chat_models_with_modality(Modality::Audio);
        if candidates.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("no audio models available, cannot transcribe".to_string()),
            });
        }

        let file_size_mb = meta.len() as f64 / (1024.0 * 1024.0);
        tracing::info!(
            path = %abs.display(),
            size_mb = %format!("{:.1}", file_size_mb),
            candidates = candidates.len(),
            models = ?candidates.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>(),
            question = %question,
            "hear_audio: starting analysis"
        );

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
            model: None,
        };
        let messages = [user_msg];

        try_with_fallback(&candidates, &messages, &abs.display().to_string(), file_size_mb).await
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
