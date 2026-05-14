//! Anthropic provider — implements ChatProvider only.

use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::HashMap;

use crate::providers::Client;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, ChatUsage, StreamEvent, StopReason,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self { base_url: DEFAULT_BASE_URL.to_string(), api_key, client: Client::new(), user_agent: None }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, client: Client::new(), user_agent: None }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    fn chat_url(&self) -> String { format!("{}/v1/messages", self.base_url) }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.chat_url();
        let auth = format!("Bearer {}", self.api_key);
        let body = build_anthropic_body(&req);
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
                Err(e) => { let _ = tx.send(StreamEvent::Error(e.to_string())).await; return; }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let _ = tx.send(StreamEvent::HttpError {
                    status: status.as_u16(),
                    message: format!("HTTP {}: {}", status, text),
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
                    Err(e) => { let _ = tx.send(StreamEvent::Error(e.to_string())).await; return; }
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

fn build_anthropic_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    let system: String = req.messages.iter()
        .filter(|m| m.role == "system")
        .filter_map(|m| {
            let text: String = m.parts.iter().filter_map(|p| match p {
                crate::providers::ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            }).collect();
            if text.is_empty() { None } else { Some(text) }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let raw_messages: Vec<serde_json::Value> = req.messages
        .iter()
        .filter(|m| m.role != "system")
        .filter_map(|msg| {
            let role = if msg.role == "assistant" { "assistant" } else { "user" };

            let has_tool_result = msg.tool_call_id.is_some();
            let mut parts: Vec<serde_json::Value> = if has_tool_result {
                let content = msg.parts.iter().map(|p| match p {
                    crate::providers::ContentPart::Text { text } => text.clone(),
                    _ => String::new(),
                }).collect::<String>();
                vec![serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.as_ref().unwrap(),
                    "content": content,
                    "is_error": msg.is_error.unwrap_or(false),
                })]
            } else {
                let mut p: Vec<serde_json::Value> = msg.parts.iter().map(|part| match part {
                    crate::providers::ContentPart::Text { text } =>
                        serde_json::json!({"type": "text", "text": text}),
                    crate::providers::ContentPart::ImageUrl { url, .. } =>
                        serde_json::json!({"type": "image", "source": {"type": "url", "url": url}}),
                    crate::providers::ContentPart::ImageB64 { b64_json, .. } =>
                        serde_json::json!({"type": "image", "source": {
                            "type": "base64", "media_type": "image/jpeg", "data": b64_json,
                        }}),
                    crate::providers::ContentPart::Thinking { thinking } =>
                        serde_json::json!({"type": "thinking", "thinking": thinking}),
                }).collect();

                if msg.role == "assistant" {
                    if let Some(ref tcs) = msg.tool_calls {
                        for tc in tcs {
                            let input = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                            p.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": input,
                            }));
                        }
                    }
                }
                p
            };

            let non_empty: Vec<serde_json::Value> = parts.drain(..).filter(|p| {
                match p.get("type").and_then(|v| v.as_str()) {
                    Some("text") => !p.get("text").and_then(|v| v.as_str()).unwrap_or("").is_empty(),
                    Some("tool_result") => true,
                    Some("tool_use") => !p.get("id").and_then(|v| v.as_str()).unwrap_or("").is_empty(),
                    _ => true,
                }
            }).collect();

            if role == "assistant" && non_empty.is_empty() {
                return None;
            }

            let content = if non_empty.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(non_empty)
            };

            Some(serde_json::json!({"role": role, "content": content}))
        })
        .collect();

    let raw_count = raw_messages.len();
    // Merge consecutive same-role messages.  Some providers (notably MiniMax's
    // Anthropic-compatible endpoint) internally convert to OpenAI format and
    // break when messages don't strictly alternate between user and assistant.
    // The Anthropic API itself merges consecutive same-role messages silently,
    // so this is safe for all consumers.
    let mut messages: Vec<serde_json::Value> = Vec::with_capacity(raw_count);
    for msg in raw_messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = msg.get("content");

        if let Some(last) = messages.last_mut() {
            if last.get("role").and_then(|v| v.as_str()) == Some(role) {
                tracing::debug!(role, "merging consecutive same-role messages");
                // Merge content arrays.
                let last_content = last.get_mut("content").unwrap();
                let new_items = match content {
                    Some(serde_json::Value::Array(arr)) => arr.clone(),
                    Some(other) => vec![other.clone()],
                    None => vec![],
                };
                match last_content {
                    serde_json::Value::Array(arr) => arr.extend(new_items),
                    serde_json::Value::Null => *last_content = serde_json::Value::Array(new_items),
                    other => {
                        let prev = other.clone();
                        *other = serde_json::Value::Array(vec![prev]);
                        if let serde_json::Value::Array(arr) = other {
                            arr.extend(new_items);
                        }
                    }
                }
                continue;
            }
        }
        messages.push(msg);
    }

    tracing::debug!(
        raw_count,
        merged_count = messages.len(),
        "build_anthropic_body: message merge complete"
    );

    // Final pass: filter out empty text blocks from content arrays.
    // Anthropic returns 400 "text content blocks must be non-empty".
    for msg in &mut messages {
        if let Some(serde_json::Value::Array(arr)) = msg.get_mut("content") {
            arr.retain(|p| {
                if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                    !p.get("text").and_then(|v| v.as_str()).unwrap_or("").is_empty()
                } else {
                    true
                }
            });
        }
    }

    // After filtering empty blocks, some messages might have empty content arrays.
    // Assistant messages must NOT have empty content in Anthropic API.
    messages.retain(|msg| {
        let content = msg.get("content");
        match content {
            Some(serde_json::Value::Array(arr)) => !arr.is_empty(),
            Some(serde_json::Value::String(s)) => !s.is_empty(),
            Some(serde_json::Value::Null) => false,
            _ => true,
        }
    });

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() { body["system"] = serde_json::json!(system); }
    if let Some(temp) = req.temperature { body["temperature"] = serde_json::json!(temp); }
    if let Some(max) = req.max_tokens { body["max_tokens"] = serde_json::json!(max); }
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

fn parse_anthropic_sse(
    line: &str,
    tool_index_map: &mut HashMap<u64, (String, String)>,
) -> Vec<StreamEvent> {
    use crate::providers::StreamEvent as SE;

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
                        return vec![SE::ToolCallStart { id, name, initial_arguments: String::new() }];
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
                    if !text.is_empty() { vec![SE::Delta { text: text.to_string() }] } else { vec![] }
                }
                "thinking_delta" => {
                    let text = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() { vec![SE::Thinking { text: text.to_string() }] } else { vec![] }
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
                    vec![SE::ToolCallDelta { id, delta: args.to_string() }]
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
                events.push(SE::Usage(ChatUsage {
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
                        events.push(SE::Done { reason });
                    }
                }
            }
            events
        }
        "message_stop" => vec![SE::Done { reason: StopReason::EndTurn }],
        _ => vec![],
    }
}