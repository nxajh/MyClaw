//! `view_image` tool — lets a text-only primary model inspect an image file by
//! delegating to a vision-capable model in the chat routing chain.

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest, ChatResponse, ContentPart};
use crate::providers::provider_registry::ProviderRegistry;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

pub struct ViewImageTool {
    providers: Arc<dyn ProviderRegistry>,
}

impl ViewImageTool {
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

fn infer_image_mime(path: &str) -> Option<&'static str> {
    match std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Try each candidate in order. On HTTP 4xx (model doesn't actually support
/// the request), log a warning and fall back to the next model. Only bail on
/// a successful response (including empty-text) or a non-HTTP error (network,
/// timeout, etc.).
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
                    "view_image: provider.chat() failed"
                );
                if is_http_4xx && i + 1 < total {
                    tracing::info!(
                        model = %model_id,
                        next_model = %candidates[i + 1].1,
                        "view_image: HTTP 4xx, falling back to next model"
                    );
                    continue;
                }
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("视觉模型调用失败：{e}")),
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
                    "view_image: stream collection failed"
                );
                if is_http_4xx && i + 1 < total {
                    tracing::info!(
                        model = %model_id,
                        next_model = %candidates[i + 1].1,
                        "view_image: HTTP 4xx, falling back to next model"
                    );
                    continue;
                }
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("视觉模型响应失败：{e}")),
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
                "view_image: empty result"
            );
            // Empty result from this model — don't try others, same root cause.
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("视觉模型返回了空结果。".to_string()),
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
            "view_image: success"
        );
        return Ok(ToolResult {
            success: true,
            output: resp.text,
            error: None,
        });
    }

    // All candidates exhausted (shouldn't reach here unless all fail with 4xx).
    Ok(ToolResult {
        success: false,
        output: String::new(),
        error: Some("所有视觉模型均不可用。".to_string()),
    })
}

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "查看图片文件内容。当对话中出现 `[图片: sessions/.../files/xxx]` 标记时，调用本工具并传入 path 与具体问题。path 可以是 workspace-relative 路径或绝对路径。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "图片文件路径；相对路径按 workspace-relative 解释，绝对路径直接使用。" },
                "question": { "type": "string", "description": "你想从这张图片了解的具体问题，例如『图里有几个人？』『识别图中的文字』。" }
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
            .unwrap_or("请详细描述这张图片的内容，包括其中的文字、物体和场景。")
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
                    error: Some(format!("无法访问图片文件 {path}：{e}")),
                });
            }
        };
        if meta.len() > 25 * 1024 * 1024 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("图片文件过大，当前 view_image 限制为 25MB。".to_string()),
            });
        }

        let candidates = self
            .providers
            .find_all_chat_models_with_modality(Modality::Image);
        if candidates.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("当前没有可用的视觉模型，无法查看图片。".to_string()),
            });
        }

        let file_size_mb = meta.len() as f64 / (1024.0 * 1024.0);
        tracing::info!(
            path = %abs.display(),
            size_mb = %format!("{:.1}", file_size_mb),
            candidates = candidates.len(),
            models = ?candidates.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>(),
            question = %question,
            "view_image: starting analysis"
        );

        let user_msg = ChatMessage {
            role: "user".into(),
            parts: vec![
                ContentPart::File {
                    path: path.to_string(),
                    mime_type: infer_image_mime(path).map(str::to_string),
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
            resolve_path("sessions/s/files/photo.png"),
            cwd.join("sessions/s/files/photo.png")
        );
    }

    #[test]
    fn infers_image_mime_from_extension() {
        assert_eq!(infer_image_mime("x.JPG"), Some("image/jpeg"));
        assert_eq!(infer_image_mime("x.webp"), Some("image/webp"));
        assert_eq!(infer_image_mime("x.pdf"), None);
    }
}
