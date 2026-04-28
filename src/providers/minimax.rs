//! MiniMax provider — OpenAI-compatible protocol.

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::Client;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, ContentPart, StreamEvent, StopReason};

const DEFAULT_BASE_URL: &str = "https://api.minimaxi.chat/v1";

#[derive(Clone)]
pub struct MiniMaxProvider {
    base_url: String,
    api_key: String,
    client: Client,
}

impl MiniMaxProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, client: Client::new() }
    }
}

#[async_trait]
impl ChatProvider for MiniMaxProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = format!("{}/text/chatcompletion_v2", self.base_url);
        let body = build_minimax_body(&req);
        let auth = crate::providers::shared::build_auth(&crate::providers::shared::AuthStyle::Bearer, &self.api_key);
        let client = self.client.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

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

            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();

            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => { let _ = tx.send(StreamEvent::Error(e.to_string())).await; return; }
                };
                utf8_buf.extend_from_slice(&bytes);
                let try_decode = std::str::from_utf8(&utf8_buf);
                let text = match try_decode {
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
                    if let Some(event) = parse_minimax_sse(&line) {
                        let _ = tx.send(event).await;
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done { reason: StopReason::EndTurn }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

fn build_minimax_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    let mut messages: Vec<serde_json::Value> = Vec::new();

    for msg in req.messages {
        let content_vec: Vec<serde_json::Value> = msg.parts.iter().map(|part| match part {
            ContentPart::Text { text } => serde_json::json!({"type": "text", "text": text}),
            ContentPart::ImageUrl { url, detail } => serde_json::json!({
                "type": "image_url",
                "image_url": { "url": url, "detail": format!("{:?}", detail).to_lowercase() }
            }),
            ContentPart::ImageB64 { b64_json, detail } => serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:image;base64,{}", b64_json), "detail": format!("{:?}", detail).to_lowercase() }
            }),
        }).collect();

        // For a single text part, emit plain string for maximum compatibility
        // (matches build_openai_chat_body behavior).
        let content = if content_vec.len() == 1 {
            if let Some(text) = msg.parts.iter().find_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            }) {
                serde_json::json!(text)
            } else {
                content_vec.into_iter().next().unwrap()
            }
        } else {
            serde_json::json!(content_vec)
        };

        let mut msg_json = json!({ "role": msg.role, "content": content });

        // Handle "tool" role: must include tool_call_id.
        if msg.role == "tool" {
            if let Some(tc_id) = &msg.tool_call_id {
                msg_json["tool_call_id"] = serde_json::json!(tc_id);
            } else if let Some(n) = &msg.name {
                msg_json["tool_call_id"] = serde_json::json!(n);
            }
        } else if msg.role == "assistant" {
            // Assistant message may carry tool_calls from a previous turn.
            msg_json["content"] = if content.is_string() && content.as_str().unwrap_or("").is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(content)
            };
            if let Some(tcs) = &msg.tool_calls {
                msg_json["tool_calls"] = serde_json::json!(tcs);
            }
        }

        messages.push(msg_json);
    }

    let mut body = json!({ "model": req.model, "messages": messages, "stream": true });
    if let Some(temp) = req.temperature { body["temperature"] = serde_json::json!(temp); }
    if let Some(max) = req.max_tokens { body["max_tokens"] = serde_json::json!(max); }
    if let Some(tools) = req.tools {
        body["tools"] = serde_json::json!(tools.iter().map(|t| {
            serde_json::json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.input_schema }
            })
        }).collect::<Vec<_>>());
    }

    body
}

fn parse_minimax_sse(line: &str) -> Option<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') { return None; }
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" { return None; }

    #[derive(serde::Deserialize)]
    struct Chunk { choices: Vec<Choice> }
    #[derive(serde::Deserialize)]
    struct Choice { delta: Delta, finish_reason: Option<String> }
    #[derive(serde::Deserialize)]
    struct Delta {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Option<Vec<TcDelta>>,
    }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct TcDelta { index: u32, id: Option<String>, function: Option<FuncDelta> }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FuncDelta { name: Option<String>, arguments: Option<String> }

    let chunk: Chunk = serde_json::from_str(data).ok()?;

    for choice in &chunk.choices {
        // Tool calls take priority (same as parse_openai_sse).
        if let Some(tcs) = &choice.delta.tool_calls {
            if let Some(tc) = tcs.first() {
                let id = tc.id.clone().unwrap_or_default();
                let func = tc.function.as_ref();

                if !id.is_empty() && func.is_some_and(|f| f.name.is_some()) {
                    let initial_args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    return Some(StreamEvent::ToolCallStart {
                        id: id.clone(),
                        name: func.and_then(|f| f.name.clone()).unwrap_or_default(),
                        initial_arguments: initial_args,
                    });
                }

                let args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                if !args.is_empty() {
                    return Some(StreamEvent::ToolCallDelta { id, delta: args });
                }
            }
        }

        if let Some(text) = &choice.delta.content {
            if !text.is_empty() { return Some(StreamEvent::Delta { text: text.clone() }); }
        }
        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() { return Some(StreamEvent::Thinking { text: reasoning.clone() }); }
        }
        if choice.finish_reason.is_some() {
            let reason = choice.finish_reason.as_ref().and_then(|r| match r.as_str() {
                "stop" => Some(StopReason::EndTurn),
                "tool_calls" => Some(StopReason::ToolUse),
                "length" => Some(StopReason::MaxTokens),
                "content_filter" => Some(StopReason::ContentFilter),
                _ => None,
            }).unwrap_or(StopReason::EndTurn);
            return Some(StreamEvent::Done { reason });
        }
    }

    None
}