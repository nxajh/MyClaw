//! Xiaomi MiMo provider — implements ChatProvider via Anthropic-compatible API.
//!
//! Xiaomi MiMo uses the Anthropic Messages API protocol.
//! Endpoint: https://api.xiaomimimo.com/anthropic/v1/messages
//! Auth: Bearer token or api-key header.
//!
//! Key differences from Anthropic:
//! - Different base URL
//! - Usage includes `cache_read_input_tokens`
//! - Extra stop reason: `repetition_truncation`

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::Client;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, ChatUsage, StreamEvent, StopReason,
};

const DEFAULT_BASE_URL: &str = "https://api.xiaomimimo.com/anthropic";

#[derive(Clone)]
pub struct XiaomiProvider {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl XiaomiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            client: Client::new(),
            user_agent: None,
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            base_url,
            api_key,
            client: Client::new(),
            user_agent: None,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }
}

#[async_trait]
impl ChatProvider for XiaomiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.chat_url();
        let auth = format!("Bearer {}", self.api_key);
        let body = build_xiaomi_body(&req);
        let client = self.client.clone();
        let user_agent = self.user_agent.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
            headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
            if let Some(ref ua) = user_agent {
                headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
            }

            let resp = match client.post(&url).headers(headers).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let _ = tx
                    .send(StreamEvent::HttpError {
                        status: status.as_u16(),
                        message: format!("HTTP {}: {}", status, text),
                    })
                    .await;
                return;
            }

            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();

            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
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
                    if let Some(event) = parse_xiaomi_sse(&line) {
                        let _ = tx.send(event).await;
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done { reason: StopReason::EndTurn }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

/// Build the request body — same structure as Anthropic Messages API.
fn build_xiaomi_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    // Extract system messages.
    let system: String = req
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .filter_map(|m| {
            let text: String = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    crate::providers::ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Build non-system messages.
    let messages: Vec<serde_json::Value> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|msg| {
            let content: Vec<serde_json::Value> = msg
                .parts
                .iter()
                .map(|part| match part {
                    crate::providers::ContentPart::Text { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    crate::providers::ContentPart::ImageUrl { url, detail: _ } => {
                        serde_json::json!({"type": "image", "source": {
                            "type": "url", "url": url,
                        }})
                    }
                    crate::providers::ContentPart::ImageB64 { b64_json, detail: _ } => {
                        serde_json::json!({"type": "image", "source": {
                            "type": "base64", "media_type": "image/jpeg",
                            "data": b64_json,
                        }})
                    }
                })
                .collect();

            let content = if content.len() == 1 {
                content.into_iter().next().unwrap()
            } else {
                serde_json::json!(content)
            };

            serde_json::json!({
                "role": if msg.role == "assistant" { "assistant" } else { "user" },
                "content": content,
            })
        })
        .collect();

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });

    if !system.is_empty() {
        body["system"] = serde_json::json!(system);
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(max) = req.max_tokens {
        body["max_tokens"] = serde_json::json!(max);
    }
    if let Some(tools) = req.tools {
        body["tools"] = serde_json::json!(tools.iter().map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        }).collect::<Vec<_>>());
    }

    body
}

/// Parse Anthropic-style SSE events from Xiaomi MiMo.
/// Handles: message_start, content_block_start, content_block_delta,
/// content_block_stop, message_delta, message_stop.
fn parse_xiaomi_sse(line: &str) -> Option<StreamEvent> {
    use crate::providers::StreamEvent as SE;

    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return None;
    }

    let evt = serde_json::from_str::<serde_json::Value>(data).ok()?;
    let ty = evt.get("type")?.as_str()?;

    match ty {
        "content_block_start" => {
            // Handle tool_use content block start — emit ToolCallStart.
            if let Some(cb) = evt.get("content_block") {
                if cb.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if !id.is_empty() && !name.is_empty() {
                        return Some(SE::ToolCallStart {
                            id,
                            name,
                            initial_arguments: String::new(),
                        });
                    }
                }
            }
            None
        }
        "content_block_delta" => {
            let delta = evt.get("delta")?;
            match delta.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text_delta" => {
                    let text = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        return Some(SE::Delta {
                            text: text.to_string(),
                        });
                    }
                }
                "thinking_delta" => {
                    let text = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        return Some(SE::Thinking {
                            text: text.to_string(),
                        });
                    }
                }
                "input_json_delta" => {
                    let idx = evt.get("index").and_then(|v| v.as_u64()).unwrap_or(0).to_string();
                    let args = delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or("");
                    return Some(SE::ToolCallDelta {
                        id: idx,
                        delta: args.to_string(),
                    });
                }
                _ => {}
            }
            None
        }
        "message_start" | "message_delta" => {
            // Usage may appear in message_start (via message.usage) or message_delta (via usage).
            let usage = evt
                .get("message")
                .and_then(|m| m.get("usage"))
                .or_else(|| evt.get("usage"));

            if let Some(usage) = usage {
                let cu = ChatUsage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    cached_input_tokens: usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64()),
                    reasoning_tokens: None,
                    cache_write_tokens: None,
                };
                return Some(SE::Usage(cu));
            }

            // message_delta may carry stop_reason.
            if ty == "message_delta" {
                if let Some(delta) = evt.get("delta") {
                    if let Some(reason_str) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                        let reason = match reason_str {
                            "end_turn" => StopReason::EndTurn,
                            "max_tokens" => StopReason::MaxTokens,
                            "tool_use" => StopReason::ToolUse,
                            "content_filter" => StopReason::ContentFilter,
                            _ => StopReason::EndTurn, // covers repetition_truncation
                        };
                        return Some(SE::Done { reason });
                    }
                }
            }

            None
        }
        "message_stop" => Some(SE::Done {
            reason: StopReason::EndTurn,
        }),
        _ => None,
    }
}
