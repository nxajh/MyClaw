//! `hear_audio` tool — lets a model inspect an audio file by delegating to an
//! audio-capable model in the chat routing chain.

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use crate::api::tool::ToolContext;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{
    ChatMessage, ChatProvider, ChatRequest, ChatResponse, ContentPart,
};
use crate::providers::provider_registry::ProviderRegistry;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

pub struct HearAudioTool {
    providers: Arc<dyn ProviderRegistry>,
    data_dir: std::path::PathBuf,
}

impl HearAudioTool {
    pub fn new(providers: Arc<dyn ProviderRegistry>, data_dir: std::path::PathBuf) -> Self {
        Self {
            providers,
            data_dir,
        }
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
        "Listen to voice/audio file content. When the conversation contains an `[audio: sessions/.../files/xxx]` marker, call this tool with the path exactly as it appears in the marker. Only use it for audio files; do not use it for image or video. Path can be a `sessions/...` marker path, workspace-relative, absolute, or a URL (http/https)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Audio file path or URL. A `sessions/<id>/files/...` marker path (copy it verbatim from the `[audio: ...]` marker) resolves against the data directory; other relative paths are workspace-relative; absolute paths are used directly; URLs (http/https) are downloaded automatically." },
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
        _ctx: &ToolContext,
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

        let abs = crate::tools::media_download::resolve_path_or_url(path, &self.data_dir)
            .await
            .map_err(|e| anyhow::anyhow!("cannot resolve path/URL '{path}': {e}"))?;
        if crate::config::is_path_protected(&abs) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("path '{path}' is protected and cannot be read")),
            });
        }
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
                    error: Some(format!(
                        "cannot access audio file {path} (resolved to {}): {e}",
                        abs.display()
                    )),
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
        if crate::providers::modality_from_mime(infer_audio_mime(path), path)
            != crate::providers::FileModality::Audio
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("hear_audio only accepts audio files: {path}")),
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
                    // Pass the already-resolved absolute path, not the raw
                    // marker path: provider rendering re-resolves whatever
                    // path lands here (see `providers::media::resolve_path`),
                    // so passing `abs` avoids a redundant second resolve and
                    // stays correct even if the global data dir was never
                    // initialized (see issue #82).
                    path: abs.display().to_string(),
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
    async fn resolves_session_media_marker_against_data_dir() {
        let data_dir = std::path::PathBuf::from("/home/user/.myclaw");
        let result = crate::tools::media_download::resolve_path_or_url(
            "sessions/s/files/voice.ogg",
            &data_dir,
        )
        .await
        .unwrap();
        assert_eq!(result, data_dir.join("sessions/s/files/voice.ogg"));
    }

    #[tokio::test]
    async fn resolves_non_session_relative_path_against_current_dir() {
        let cwd = std::env::current_dir().unwrap();
        let data_dir = std::path::PathBuf::from("/home/user/.myclaw");
        let result = crate::tools::media_download::resolve_path_or_url("voice.ogg", &data_dir)
            .await
            .unwrap();
        assert_eq!(result, cwd.join("voice.ogg"));
    }

    #[test]
    fn infers_audio_mime_from_extension() {
        assert_eq!(infer_audio_mime("x.OGG"), Some("audio/ogg"));
        assert_eq!(infer_audio_mime("x.mp3"), Some("audio/mpeg"));
        assert_eq!(infer_audio_mime("x.png"), None);
    }

    /// Errors on every call — proves `execute()` never reaches provider
    /// dispatch when the protected-path check short-circuits first.
    struct NullRegistry;
    #[rustfmt::skip]
    impl ProviderRegistry for NullRegistry {
        fn get_chat_provider(&self, _c: crate::providers::Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> { anyhow::bail!("test stub") }
        fn get_chat_provider_with_hint(&self, _c: crate::providers::Capability, _h: Option<&str>) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> { anyhow::bail!("test stub") }
        fn get_chat_fallback_chain(&self, _c: crate::providers::Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>> { anyhow::bail!("test stub") }
        fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::EmbeddingProvider>, String)> { anyhow::bail!("test stub") }
        fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::ImageGenerationProvider>, String)> { anyhow::bail!("test stub") }
        fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::TtsProvider>, String)> { anyhow::bail!("test stub") }
        fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::VideoGenerationProvider>, String)> { anyhow::bail!("test stub") }
        fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::SearchProvider>, String)> { anyhow::bail!("test stub") }
        fn get_search_fallback_chain(&self) -> anyhow::Result<Vec<crate::providers::provider_registry::SearchFallbackEntry>> { anyhow::bail!("test stub") }
        fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::SttProvider>, String)> { anyhow::bail!("test stub") }
        fn get_chat_model_config(&self, _m: &str) -> anyhow::Result<&crate::providers::capability::ChatModelConfig> { anyhow::bail!("test stub") }
        fn get_chat_provider_by_model(&self, _m: &str) -> Option<(Arc<dyn ChatProvider>, String)> { panic!("provider dispatch reached — protected-path check did not short-circuit") }
        fn get_chat_provider_id_by_model(&self, _m: &str) -> Option<String> { None }
        fn get_chat_media_policy(&self, _m: &str) -> Option<crate::providers::MediaPolicy> { None }
        fn get_chat_routing_models(&self) -> Vec<String> { vec!["stub-model".to_string()] }
        fn get_all_provider_summaries(&self) -> Vec<crate::providers::provider_registry::ProviderSummary> { Vec::new() }
    }

    #[tokio::test]
    async fn execute_rejects_protected_path_before_touching_providers() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
        let protected = format!("{home}/.ssh/id_rsa");
        let tool = HearAudioTool::new(Arc::new(NullRegistry), std::path::PathBuf::from("/data"));
        let result = tool
            .execute(
                json!({"path": protected}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("is protected and cannot be read"),
            "got: {:?}",
            result.error
        );
    }
}
