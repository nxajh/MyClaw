//! MiniMax provider — OpenAI-compatible protocol.
//!
//! MiniMax requires system messages to be merged into the first user message
//! (rejects `role: system`).

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::Client;
use capability::chat::{BoxStream, ChatProvider, ChatRequest, ContentPart, StreamEvent, StopReason};

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
        let auth = crate::shared::build_auth(&crate::shared::AuthStyle::Bearer, &self.api_key);
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
                let _ = tx.send(StreamEvent::Error(format!("HTTP {}: {}", status, text))).await;
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

    let mut system_parts: Vec<ContentPart> = Vec::new();
    let mut messages: Vec<serde_json::Value> = Vec::new();

    for msg in req.messages {
        if msg.role == "system" {
            system_parts.extend(msg.parts.clone());
        } else {
            let content: Vec<serde_json::Value> = msg.parts.iter().map(|part| match part {
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

            let content = if content.len() == 1 {
                content.into_iter().next().unwrap()
            } else {
                serde_json::json!(content)
            };

            messages.push(json!({ "role": msg.role, "content": content }));
        }
    }

    // Merge system into first user message
    if !system_parts.is_empty() && !messages.is_empty() {
        let system_text: String = system_parts.iter().filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>().join("\n");

        if let Some(obj) = messages[0].as_object_mut() {
            if let Some(content) = obj.get("content").and_then(|c| c.as_str()) {
                obj["content"] = serde_json::json!(format!("[System instructions]\n{}\n\n{}", system_text, content));
            }
        }
    }

    let mut body = json!({ "model": req.model, "messages": messages, "stream": true });
    if let Some(temp) = req.temperature { body["temperature"] = serde_json::json!(temp); }
    if let Some(max) = req.max_tokens { body["max_tokens"] = serde_json::json!(max); }
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
    struct Delta { content: Option<String>, reasoning_content: Option<String> }

    let chunk: Chunk = serde_json::from_str(data).ok()?;

    for choice in &chunk.choices {
        if let Some(text) = &choice.delta.content {
            if !text.is_empty() { return Some(StreamEvent::Delta { text: text.clone() }); }
        }
        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() { return Some(StreamEvent::Thinking { text: reasoning.clone() }); }
        }
        if choice.finish_reason.is_some() {
            return Some(StreamEvent::Done { reason: StopReason::EndTurn });
        }
    }

    None
}