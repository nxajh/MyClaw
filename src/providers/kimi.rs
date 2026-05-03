//! Kimi (Moonshot) provider — Kimi-specific protocol.
//!
//! Key differences from generic OpenAI:
//! - `reasoning_content` is a top-level message field (not a content part)
//! - K2.5/K2.6 support `thinking` parameter for enabling/disabling reasoning
//! - `content` must not be empty; assistant messages with only tool_calls use null

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::Client;
use crate::providers::shared::parse_openai_sse;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, ContentPart, StreamEvent, StopReason};

const DEFAULT_BASE_URL: &str = "https://api.moonshot.cn/v1";

#[derive(Clone)]
pub struct KimiProvider {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl KimiProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
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
}

fn build_kimi_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    let messages: Vec<serde_json::Value> = req
        .messages
        .iter()
        .map(|msg| {
            // Extract reasoning_content from Thinking parts.
            let reasoning: Option<String> = {
                let parts: Vec<&str> = msg
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Thinking { thinking } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join(""))
                }
            };

            // Build content parts, skipping Thinking.
            let content_vec: Vec<serde_json::Value> = msg
                .parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
                    ContentPart::ImageUrl { url, detail } => Some(json!({
                        "type": "image_url",
                        "image_url": { "url": url, "detail": format!("{:?}", detail).to_lowercase() }
                    })),
                    ContentPart::ImageB64 { b64_json, detail } => Some(json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:image;base64,{}", b64_json), "detail": format!("{:?}", detail).to_lowercase() }
                    })),
                    ContentPart::Thinking { .. } => None,
                })
                .collect();

            // Determine content value.
            let content = if content_vec.len() == 1 {
                // Single text part → plain string for compatibility.
                if let Some(text) = msg.parts.iter().find_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                }) {
                    json!(text)
                } else {
                    content_vec.into_iter().next().unwrap()
                }
            } else if content_vec.is_empty() {
                // No non-thinking parts — empty placeholder.
                json!("")
            } else {
                json!(content_vec)
            };

            let mut msg_json = json!({ "role": msg.role });

            if msg.role == "assistant" {
                // Kimi requires non-empty content or null.
                let is_empty = match &content {
                    serde_json::Value::String(s) => s.is_empty(),
                    serde_json::Value::Array(arr) => arr.is_empty(),
                    _ => false,
                };
                msg_json["content"] = if is_empty {
                    serde_json::Value::Null
                } else {
                    json!(content)
                };
                // Attach reasoning_content as top-level field.
                if let Some(ref rc) = reasoning {
                    msg_json["reasoning_content"] = json!(rc);
                }
                // Attach tool_calls.
                if let Some(tcs) = &msg.tool_calls {
                    msg_json["tool_calls"] =
                        json!(tcs.iter().map(|tc| tc.to_openai()).collect::<Vec<_>>());
                }
            } else {
                msg_json["content"] = json!(content);
            }

            // tool role: attach tool_call_id.
            if msg.role == "tool" {
                if let Some(tc_id) = &msg.tool_call_id {
                    msg_json["tool_call_id"] = json!(tc_id);
                } else if let Some(n) = &msg.name {
                    msg_json["tool_call_id"] = json!(n);
                }
            }

            msg_json
        })
        .collect();

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });

    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = req.max_tokens {
        // Kimi docs recommend max_completion_tokens over deprecated max_tokens.
        body["max_completion_tokens"] = json!(max);
    }
    if let Some(stop) = &req.stop {
        body["stop"] = json!(stop);
    }
    if let Some(tools) = req.tools {
        body["tools"] = json!(tools.iter().map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema
                }
            })
        }).collect::<Vec<_>>());
    }

    // Add thinking parameter when configured.
    if let Some(ref tc) = req.thinking {
        let mut thinking_val = json!({"type": tc.type_});
        if tc.type_ == "enabled" {
            // Preserved Thinking: keep all historical reasoning_content.
            thinking_val["keep"] = json!("all");
        }
        body["thinking"] = thinking_val;
    }

    body
}

#[async_trait]
impl ChatProvider for KimiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_kimi_body(&req);
        let body_str = serde_json::to_string_pretty(&body).unwrap_or_default();
        crate::providers::append_to_debug_log(&format!(
            "=== REQUEST ===\nURL: {}\nBody:\n{}\n",
            url, body_str
        ));
        let auth =
            crate::providers::shared::build_auth(
                &crate::providers::shared::AuthStyle::Bearer,
                &self.api_key,
            );
        let client = self.client.clone();
        let user_agent = self.user_agent.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                auth.parse().unwrap(),
            );
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            if let Some(ref ua) = user_agent {
                headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
            }

            let resp = match client
                .post(&url)
                .headers(headers)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                crate::providers::append_to_debug_log(&format!(
                    "=== HTTP ERROR ===\nURL: {}\nStatus: {}\nBody: {}\n",
                    url, status, text
                ));
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
            let byte_stream = resp.bytes_stream();
            crate::providers::append_to_debug_log(&format!(
                "=== SSE STREAM START ===\nURL: {}\n",
                url
            ));

            let mut stream = std::pin::pin!(byte_stream);
            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        return;
                    }
                };
                utf8_buf.extend_from_slice(&bytes);
                let try_decode = std::str::from_utf8(&utf8_buf);
                let text = match try_decode {
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
                    let event = parse_openai_sse(&line);
                    crate::providers::append_to_debug_log(&format!(
                        "SSE LINE: {}\nEVENT: {:?}\n",
                        line, event
                    ));
                    if let Some(event) = event {
                        let _ = tx.send(event).await;
                    }
                }
            }
            crate::providers::append_to_debug_log(&format!(
                "=== SSE STREAM END ===\nURL: {}\n\n",
                url
            ));
            let _ = tx
                .send(StreamEvent::Done {
                    reason: StopReason::EndTurn,
                })
                .await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}
