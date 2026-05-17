//! Standard Anthropic Messages API client.
//!
//! Implements `ChatProvider` for the Anthropic Messages endpoint
//! and Anthropic-compatible providers.

use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::HashMap;

use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, StreamEvent, StopReason,
};
use reqwest::Client;
use std::time::Duration;
use crate::providers::http::build_reqwest_client;
use crate::providers::protocols::anthropic::message_rendering::build_anthropic_body;

/// Anthropic Messages protocol client.
#[derive(Clone)]
pub struct AnthropicMessagesClient {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl AnthropicMessagesClient {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, client: build_reqwest_client(), user_agent: None }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl ChatProvider for AnthropicMessagesClient {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.chat_url();
        let api_key = self.api_key.clone();
        let thinking_enabled = req.thinking.as_ref().is_some_and(|t| t.enabled);
        let body = build_anthropic_body(&req);
        let client = self.client.clone();
        let user_agent = self.user_agent.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("x-api-key", api_key.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
            headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
            if thinking_enabled {
                headers.insert("anthropic-beta", "interleaved-thinking-2025-05-14".parse().unwrap());
            }
            if let Some(ref ua) = user_agent {
                headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
            }

            // Bound the time spent waiting for the initial HTTP response headers.
            // Once headers arrive, per-chunk timeouts in collect_stream_inner take over.
            let resp = match tokio::time::timeout(
                Duration::from_secs(30),
                client.post(&url).headers(headers).json(&body).send()
            ).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!(url = %url, error = %e, "request failed");
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
                Err(_) => {
                    tracing::warn!(url = %url, "timed out waiting for response headers");
                    let _ = tx.send(StreamEvent::Error(
                        "timed out waiting for response headers".to_string()
                    )).await;
                    return;
                }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let message = parse_anthropic_error_body(&text)
                    .unwrap_or_else(|| format!("HTTP {}: {}", status, text));
                let _ = tx.send(StreamEvent::HttpError {
                    status: status.as_u16(),
                    message,
                }).await;
                return;
            }

            // index → (tool_id, tool_name) mapping for Anthropic's block-indexed SSE.
            let mut tool_index_map: HashMap<u64, (String, String)> = HashMap::new();
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
                    Ok(s) => { let owned = s.to_string(); utf8_buf.clear(); owned }
                    Err(e) => {
                        let valid = e.valid_up_to();
                        if valid == 0 && utf8_buf.len() < 4 { continue; }
                        let t = String::from_utf8_lossy(&utf8_buf[..valid]).into_owned();
                        utf8_buf.clear();
                        t
                    }
                };
                if text.is_empty() { continue; }
                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].to_string();
                    buffer.drain(..=pos);
                    let events = parse_anthropic_sse(&line, &mut tool_index_map);
                    for event in events {
                        let _ = tx.send(event).await;
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done { reason: StopReason::EndTurn }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

/// Extract a human-readable message from an Anthropic error response body.
/// Anthropic returns `{"type":"error","error":{"type":"...","message":"..."}}`.
fn parse_anthropic_error_body(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    let kind = err.get("type").and_then(|v| v.as_str()).unwrap_or("error");
    let msg = err.get("message").and_then(|v| v.as_str())?;
    Some(format!("{}: {}", kind, msg))
}

fn parse_anthropic_sse(
    line: &str,
    tool_index_map: &mut HashMap<u64, (String, String)>,
) -> Vec<StreamEvent> {
    use crate::providers::{ChatUsage, StopReason};

    let line = line.trim();
    if line.is_empty() || line.starts_with(':') { return vec![]; }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return vec![],
    };
    if data == "[DONE]" { return vec![]; }

    let evt = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let ty = match evt.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return vec![],
    };

    match ty {
        "content_block_start" => {
            let index = match evt.get("index").and_then(|v| v.as_u64()) {
                Some(i) => i,
                None => return vec![],
            };
            if let Some(cb) = evt.get("content_block") {
                if cb.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if !id.is_empty() && !name.is_empty() {
                        tool_index_map.insert(index, (id.clone(), name.clone()));
                        return vec![StreamEvent::ToolCallStart { id, name, initial_arguments: String::new() }];
                    }
                }
            }
            vec![]
        }
        "content_block_delta" => {
            let delta = match evt.get("delta") {
                Some(d) => d,
                None => return vec![],
            };
            match delta.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text_delta" => {
                    let text = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() { vec![StreamEvent::Delta { text: text.to_string() }] } else { vec![] }
                }
                "thinking_delta" => {
                    let text = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() { vec![StreamEvent::Thinking { text: text.to_string() }] } else { vec![] }
                }
                "signature_delta" => {
                    let sig = delta.get("signature").and_then(|v| v.as_str()).unwrap_or("");
                    if sig.is_empty() { vec![] } else { vec![StreamEvent::ThinkingSignature { signature: sig.to_string() }] }
                }
                "input_json_delta" => {
                    let index = match evt.get("index").and_then(|v| v.as_u64()) {
                        Some(i) => i,
                        None => return vec![],
                    };
                    let (id, _name) = match tool_index_map.get(&index) {
                        Some(entry) => entry.clone(),
                        None => return vec![],
                    };
                    let args = delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or("");
                    vec![StreamEvent::ToolCallDelta { id, delta: args.to_string() }]
                }
                _ => vec![],
            }
        }
        "message_start" | "message_delta" => {
            let mut events = Vec::new();
            let usage = evt
                .get("message").and_then(|m| m.get("usage"))
                .or_else(|| evt.get("usage"));
            if let Some(usage) = usage {
                events.push(StreamEvent::Usage(ChatUsage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    cached_input_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                    reasoning_tokens: None,
                    cache_write_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()),
                }));
            }
            if ty == "message_delta" {
                if let Some(delta) = evt.get("delta") {
                    if let Some(reason_str) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                        let reason = match reason_str {
                            "end_turn" => StopReason::EndTurn,
                            "max_tokens" => StopReason::MaxTokens,
                            "tool_use" => StopReason::ToolUse,
                            "content_filter" => StopReason::ContentFilter,
                            _ => StopReason::EndTurn,
                        };
                        events.push(StreamEvent::Done { reason });
                    }
                }
            }
            events
        }
        "message_stop" => vec![StreamEvent::Done { reason: StopReason::EndTurn }],
        _ => vec![],
    }
}