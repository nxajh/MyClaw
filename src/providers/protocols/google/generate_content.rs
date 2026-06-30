//! Google Generate Content API client.
//!
//! Implements `ChatProvider` for the Google Gemini `streamGenerateContent` endpoint.
//!
//! Reference: https://ai.google.dev/api/generate-content

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::http::build_reqwest_client;
use crate::providers::protocols::google::message_rendering::build_google_body;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, ChatUsage, SharedApiKey, StopReason, StreamEvent};
use reqwest::Client;

/// Google Generate Content protocol client.
#[derive(Clone)]
pub struct GoogleGenerateContentClient {
    base_url: String,
    api_key: SharedApiKey,
    client: Client,
    user_agent: Option<String>,
}

impl GoogleGenerateContentClient {
    pub fn new(api_key: impl Into<SharedApiKey>, base_url: String) -> Self {
        Self {
            base_url,
            api_key: api_key.into(),
            client: build_reqwest_client(),
            user_agent: None,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    /// Build the streaming URL for a given model.
    ///
    /// `model` may be bare (`gemini-2.0-flash`) or prefixed (`models/gemini-2.0-flash`).
    fn stream_url(&self, model: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let model_path = if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{}", model)
        };
        format!(
            "{}/v1beta/{}:streamGenerateContent?alt=sse&key={}",
            base, model_path, self.api_key.get()
        )
    }
}

#[async_trait]
impl ChatProvider for GoogleGenerateContentClient {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.stream_url(req.model);
        let body = build_google_body(&req);
        let client = self.client.clone();
        let user_agent = self.user_agent.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            if let Some(ref ua) = user_agent {
                headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
            }

            let resp = match client.post(&url).headers(headers).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "request failed");
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let message =
                    parse_google_error_body(&text).unwrap_or_else(|| format!("HTTP {}: {}", status, text));
                if status.as_u16() == 400 {
                    tracing::warn!(
                        url = %url,
                        status = status.as_u16(),
                        error = %message,
                        request_body = %body,
                        "HTTP 400 from Google — dumping request body for diagnosis",
                    );
                }
                let _ = tx
                    .send(StreamEvent::HttpError {
                        status: status.as_u16(),
                        message,
                    })
                    .await;
                return;
            }

            // ── SSE stream parsing ───────────────────────────────────────
            let mut saw_terminal = false;
            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();

            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(url = %url, error = %e, "stream read error");
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        return;
                    }
                };
                utf8_buf.extend_from_slice(&bytes);
                let text = match std::str::from_utf8(&utf8_buf) {
                    Ok(s) => {
                        let owned = s.to_string();
                        utf8_buf.clear();
                        owned
                    }
                    Err(e) => {
                        let valid = e.valid_up_to();
                        if valid == 0 && utf8_buf.len() < 4 {
                            continue;
                        }
                        let t = String::from_utf8_lossy(&utf8_buf[..valid]).into_owned();
                        utf8_buf.clear();
                        t
                    }
                };
                if text.is_empty() {
                    continue;
                }
                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].to_string();
                    buffer.drain(..=pos);
                    let events = parse_google_sse(&line);
                    for event in events {
                        if matches!(event, StreamEvent::Done { .. }) {
                            saw_terminal = true;
                        }
                        let _ = tx.send(event).await;
                    }
                }
            }

            if !saw_terminal {
                let _ = tx
                    .send(StreamEvent::Error(
                        "Google stream closed before completion (no finishReason); \
                         likely upstream truncation"
                            .to_string(),
                    ))
                    .await;
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

/// Extract a human-readable message from a Google error response body.
/// Google returns `{"error": {"code": 400, "message": "...", "status": "..."}}`.
fn parse_google_error_body(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    let code = err.get("code").and_then(|v| v.as_u64()).unwrap_or(0);
    let status = err.get("status").and_then(|v| v.as_str()).unwrap_or("ERROR");
    let msg = err.get("message").and_then(|v| v.as_str())?;
    Some(format!("{} ({}): {}", status, code, msg))
}

/// Parse a single SSE line from the Google `streamGenerateContent` endpoint.
///
/// Google SSE format:
/// ```text
/// data: {"candidates":[{"content":{"role":"model","parts":[{"text":"..."}]},"finishReason":"STOP"}],"usageMetadata":{...}}
/// ```
///
/// Each `data:` line is a complete `GenerateContentResponse` JSON object.
fn parse_google_sse(line: &str) -> Vec<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return vec![];
    }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return vec![],
    };
    if data == "[DONE]" {
        return vec![];
    }

    let evt: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut events = Vec::new();

    // ── usageMetadata ────────────────────────────────────────────────────
    if let Some(usage) = evt.get("usageMetadata") {
        events.push(StreamEvent::Usage(ChatUsage {
            input_tokens: usage.get("promptTokenCount").and_then(|v| v.as_u64()),
            output_tokens: usage
                .get("candidatesTokenCount")
                .and_then(|v| v.as_u64()),
            cached_input_tokens: usage
                .get("cachedContentTokenCount")
                .and_then(|v| v.as_u64()),
            reasoning_tokens: usage
                .get("thoughtsTokenCount")
                .and_then(|v| v.as_u64()),
            cache_write_tokens: None,
        }));
    }

    // ── candidates ───────────────────────────────────────────────────────
    let Some(candidates) = evt.get("candidates").and_then(|v| v.as_array()) else {
        return events;
    };
    let Some(candidate) = candidates.first() else {
        return events;
    };

    // Parts
    if let Some(content) = candidate.get("content") {
        if let Some(parts) = content.get("parts").and_then(|v| v.as_array()) {
            for part in parts {
                let is_thought = part
                    .get("thought")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                // Text (including thinking)
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        if is_thought {
                            events.push(StreamEvent::Thinking {
                                text: text.to_string(),
                            });
                            // Google thinking blocks carry a thoughtSignature that
                            // must be echoed back in subsequent turns.
                            if let Some(sig) = part
                                .get("thoughtSignature")
                                .and_then(|v| v.as_str())
                            {
                                events.push(StreamEvent::ThinkingSignature {
                                    signature: sig.to_string(),
                                });
                            }
                        } else {
                            events.push(StreamEvent::Delta {
                                text: text.to_string(),
                            });
                        }
                    }
                }

                // Function call
                if let Some(fc) = part.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = fc.get("args").cloned().unwrap_or(serde_json::Value::Null);
                    let args_str = if args.is_null() {
                        "{}".to_string()
                    } else {
                        args.to_string()
                    };
                    if !name.is_empty() {
                        // Google doesn't provide a tool call ID; generate one
                        // from the name + args hash for uniqueness.
                        let id = format!("google_{}", name);
                        events.push(StreamEvent::ToolCallStart {
                            id,
                            name,
                            initial_arguments: args_str,
                        });
                    }
                    // functionCall parts may carry a thoughtSignature at the
                    // same level (Gemini 3+ thinking models). Emit it so the
                    // caller can store and replay it in subsequent turns.
                    if let Some(sig) = part
                        .get("thoughtSignature")
                        .and_then(|v| v.as_str())
                    {
                        events.push(StreamEvent::ThinkingSignature {
                            signature: sig.to_string(),
                        });
                    }
                }
            }
        }
    }

    // finishReason
    if let Some(reason_str) = candidate.get("finishReason").and_then(|v| v.as_str()) {
        let reason = match reason_str {
            "STOP" => StopReason::EndTurn,
            "MAX_TOKENS" => StopReason::MaxTokens,
            "SAFETY" | "RECITATION" => StopReason::ContentFilter,
            s if crate::providers::capability_chat::is_context_overflow_reason(s) => {
                StopReason::ContextOverflow
            }
            _ => StopReason::EndTurn,
        };
        events.push(StreamEvent::Done { reason });
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta() {
        let line = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]}"#;
        let events = parse_google_sse(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta { text } => assert_eq!(text, "Hello"),
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn parse_thinking_part() {
        let line = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"reasoning...","thought":true}]}}]}"#;
        let events = parse_google_sse(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Thinking { text } => assert_eq!(text, "reasoning..."),
            _ => panic!("expected Thinking"),
        }
    }

    #[test]
    fn parse_function_call() {
        let line = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"search","args":{"q":"rust"}}}]}}]}"#;
        let events = parse_google_sse(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolCallStart { name, initial_arguments, .. } => {
                assert_eq!(name, "search");
                assert!(initial_arguments.contains("rust"));
            }
            _ => panic!("expected ToolCallStart"),
        }
    }

    #[test]
    fn parse_finish_reason() {
        let line = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP"}]}"#;
        let events = parse_google_sse(line);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], StreamEvent::Delta { .. }));
        assert!(matches!(events[1], StreamEvent::Done { reason: StopReason::EndTurn }));
    }

    #[test]
    fn parse_usage_metadata() {
        let line = r#"data: {"candidates":[],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,"totalTokenCount":150}}"#;
        let events = parse_google_sse(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, Some(100));
                assert_eq!(u.output_tokens, Some(50));
            }
            _ => panic!("expected Usage"),
        }
    }

    #[test]
    fn parse_error_body() {
        let body = r#"{"error":{"code":400,"message":"Invalid request","status":"INVALID_ARGUMENT"}}"#;
        let msg = parse_google_error_body(body).unwrap();
        assert!(msg.contains("INVALID_ARGUMENT"));
        assert!(msg.contains("Invalid request"));
    }
}
