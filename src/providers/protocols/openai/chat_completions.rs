//! Standard OpenAI Chat Completions client.
//!
//! Implements `ChatProvider` for the OpenAI Chat Completions endpoint
//! and OpenAI-compatible providers.

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::Client;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, StreamEvent, StopReason,
};
use crate::providers::protocols::openai::chat_message_rendering::render_openai_chat_body;

/// OpenAI Chat Completions protocol client.
#[derive(Clone)]
pub struct OpenAiChatCompletionsClient {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl OpenAiChatCompletionsClient {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, client: Client::new(), user_agent: None }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.contains("/v4") {
            // GLM uses /v4/chat/completions — don't inject /v1.
            format!("{}/chat/completions", base)
        } else {
            format!("{}/v1/chat/completions", base)
        }
    }

    fn common_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, self.auth().parse().unwrap());
        headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
        if let Some(ref ua) = self.user_agent {
            headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
        }
        headers
    }
}

#[async_trait]
impl ChatProvider for OpenAiChatCompletionsClient {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.chat_url();
        let body = render_openai_chat_body(&req);
        let client = self.client.clone();
        let headers = self.common_headers();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
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

            let mut saw_tool_call = false;
            let mut sse_stop_reason: Option<StopReason> = None;
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
                    let events = crate::providers::shared::parse_openai_sse(&line);
                    for ev in events {
                        match ev {
                            StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallDelta { .. } => {
                                saw_tool_call = true;
                                let _ = tx.send(ev).await;
                            }
                            // Consume SSE-reported Done; emit a single authoritative Done at the end.
                            StreamEvent::Done { reason } => {
                                sse_stop_reason = Some(reason);
                            }
                            _ => { let _ = tx.send(ev).await; }
                        }
                    }
                }
            }

            // Determine final stop reason: prefer the SSE-reported reason (which carries
            // MaxTokens / ContentFilter), but override with ToolUse when tool calls were
            // made and the provider reported "stop" instead of "tool_calls".
            let final_reason = match sse_stop_reason {
                Some(StopReason::ToolUse) => StopReason::ToolUse,
                Some(r) if saw_tool_call => {
                    tracing::debug!(?r, "overriding SSE stop reason with ToolUse (saw tool call events)");
                    StopReason::ToolUse
                }
                Some(r) => r,
                None => if saw_tool_call { StopReason::ToolUse } else { StopReason::EndTurn },
            };
            let _ = tx.send(StreamEvent::Done { reason: final_reason }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}