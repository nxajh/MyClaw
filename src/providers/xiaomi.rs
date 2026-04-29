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
        let body_str = serde_json::to_string_pretty(&body).unwrap_or_default();
        tracing::debug!(url, body = %body_str, "xiaomi: sending chat request");
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
                let body_text = resp.text().await.unwrap_or_default();
                let _ = tx
                    .send(StreamEvent::HttpError {
                        status: status.as_u16(),
                        message: format!("HTTP {}: {}", status, body_text),
                    })
                    .await;
                return;
            }

            // SSE parsing state: index → (tool_id, tool_name) mapping.
            // Anthropic uses index (block number) in content_block_delta, not id.
            let mut tool_index_map: std::collections::HashMap<u64, (String, String)> =
                std::collections::HashMap::new();
            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();
            tracing::debug!(url, "xiaomi: starting SSE stream");

            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(url, error = %e, "xiaomi: bytes stream error");
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
                    let event = parse_xiaomi_sse(&line, &mut tool_index_map);
                    tracing::debug!(line = %line, event = ?event, "xiaomi: SSE line parsed");
                    if let Some(ev) = event {
                        let _ = tx.send(ev).await;
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
            let role = if msg.role == "assistant" { "assistant" } else { "user" };

            // Build content array from parts (text, image, thinking).
            // For tool-result messages: use {"type":"tool_result","tool_use_id":"...","content":"..."}
            // For assistant messages with tool_calls: append tool_use blocks to content array.
            let has_tool_result = msg.tool_call_id.is_some();
            let parts_json: Vec<serde_json::Value> = if has_tool_result {
                    // Tool-result: single tool_result block.
                    let content = msg.parts.iter().map(|p| match p {
                        crate::providers::ContentPart::Text { text } => text.clone(),
                        _ => String::new(),
                    }).collect::<String>();
                    vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.as_ref().unwrap(),
                        "content": content,
                        "is_error": false,
                    })]
                } else {
                    // Regular parts.
                    let mut parts: Vec<serde_json::Value> = msg.parts.iter().map(|part| match part {
                        crate::providers::ContentPart::Text { text } => {
                            serde_json::json!({"type": "text", "text": text})
                        }
                        crate::providers::ContentPart::ImageUrl { url, detail: _ } => {
                            serde_json::json!({"type": "image", "source": {"type": "url", "url": url}})
                        }
                        crate::providers::ContentPart::ImageB64 { b64_json, detail: _ } => {
                            serde_json::json!({"type": "image", "source": {
                                "type": "base64", "media_type": "image/jpeg", "data": b64_json,
                            }})
                        }
                        crate::providers::ContentPart::Thinking { thinking } => {
                            serde_json::json!({"type": "thinking", "thinking": thinking})
                        }
                    }).collect();

                    // For assistant messages with tool_calls: append tool_use blocks to content
                    // alongside text/thinking parts.
                    if msg.role == "assistant" && msg.tool_calls.is_some() {
                        let blocks: Vec<serde_json::Value> = msg.tool_calls.as_ref().unwrap().iter().map(|tc| {
                            let input = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                .unwrap_or(serde_json::Value::String(tc.arguments.clone()));
                            serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": input,
                            })
                        }).collect();
                        parts.extend(blocks);
                    }
                    parts
                };

            // input is empty if null, empty string, or empty JSON object "{}"
            fn input_is_empty(v: &serde_json::Value) -> bool {
                match v {
                    serde_json::Value::Null => true,
                    serde_json::Value::String(s) => s.trim().is_empty(),
                    serde_json::Value::Object(m) => m.is_empty(),
                    _ => false,
                }
            }

            fn is_non_empty_block(p: &serde_json::Value) -> bool {
                match p.get("type").and_then(|v| v.as_str()) {
                    Some("tool_use") => {
                        // tool_use must have a non-empty id and non-empty input object
                        let id_empty = p.get("id").and_then(|v| v.as_str()).is_none_or(|s| s.is_empty());
                        let input_empty = p.get("input").map(input_is_empty).unwrap_or(true);
                        !(id_empty || input_empty)
                    }
                    Some("tool_result") => {
                        !p.get("content").and_then(|v| v.as_str()).is_none_or(|s| s.is_empty())
                    }
                    Some("text") => !p.get("text").and_then(|v| v.as_str()).is_none_or(|t| t.is_empty()),
                    Some("thinking") => true, // always keep — empty thinking blocks are
                    // semantically meaningful; actual thinking streams via deltas.
                    Some("image") => true,
                    _ => true,
                }
            }
            let non_empty_parts: Vec<serde_json::Value> = parts_json.into_iter().filter(is_non_empty_block).collect();
            let has_non_empty_part = !non_empty_parts.is_empty();

            let final_content = if !has_non_empty_part {
                serde_json::Value::Null
            } else {
                serde_json::json!(non_empty_parts)
            };

            // Build the message object. No top-level tool_calls — Xiaomi puts
            // tool_call info only in content blocks.
            let mut msg_json = serde_json::Map::new();
            msg_json.insert("role".to_string(), serde_json::json!(role));
            msg_json.insert("content".to_string(), final_content);
            serde_json::json!(msg_json)
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
    if let Some(ref thinking) = req.thinking {
        // Build thinking config per Anthropic/Xiaomi protocol.
        // Xiaomi expects: {"type": "enabled", "budget_tokens": N}
        // effort field maps to budget_tokens level when present.
        let mut t = serde_json::Map::new();
        t.insert("type".to_string(), serde_json::json!("enabled"));
        if let Some(budget) = thinking.budget_tokens {
            t.insert("budget_tokens".to_string(), serde_json::json!(budget));
        }
        body["thinking"] = serde_json::json!(t);
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
fn parse_xiaomi_sse(
    line: &str,
    tool_index_map: &mut std::collections::HashMap<u64, (String, String)>,
) -> Option<StreamEvent> {
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
            let index = evt.get("index").and_then(|v| v.as_u64())?;
            if let Some(cb) = evt.get("content_block") {
                if cb.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if !id.is_empty() && !name.is_empty() {
                        // Store index → (id, name) so subsequent deltas can look it up.
                        tool_index_map.insert(index, (id.clone(), name.clone()));
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
                    // Delta carries index, not id. Look up (id, name) from the map.
                    let index = evt.get("index").and_then(|v| v.as_u64())?;
                    let (id, _name) = tool_index_map.get(&index)?.clone();
                    let args = delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or("");
                    return Some(SE::ToolCallDelta {
                        id,
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
