//! Standard OpenAI Chat Completions client.
//!
//! Implements `ChatProvider` for the OpenAI Chat Completions endpoint
//! and OpenAI-compatible providers.

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, StreamEvent, StopReason,
};
use reqwest::Client;
use std::time::Duration;
use crate::providers::http::build_reqwest_client;
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
        Self { base_url, api_key, client: build_reqwest_client(), user_agent: None }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn chat_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'))
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
            let resp = match tokio::time::timeout(
                Duration::from_secs(30),
                client.post(&url).headers(headers).json(&body).send()
            ).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => { let _ = tx.send(StreamEvent::Error(e.to_string())).await; return; }
                Err(_) => {
                    let _ = tx.send(StreamEvent::Error(
                        "timed out waiting for response headers".to_string()
                    )).await;
                    return;
                }
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
                    let events = parse_openai_sse(&line);
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

fn parse_openai_sse(line: &str) -> Vec<StreamEvent> {
    use crate::providers::{ChatUsage, StopReason};

    let line = line.trim();
    if line.is_empty() || line.starts_with(':') { return vec![]; }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return vec![],
    };
    if data == "[DONE]" { return vec![]; }

    #[derive(serde::Deserialize)]
    struct Chunk {
        #[serde(default)]
        choices: Vec<Choice>,
        usage: Option<ChunkUsage>,
    }
    #[derive(serde::Deserialize)]
    struct Choice { delta: Delta, finish_reason: Option<String> }
    #[derive(serde::Deserialize)]
    struct Delta {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Option<Vec<TcDelta>>,
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(dead_code)]
    struct TcDelta { index: u32, id: Option<String>, function: Option<FuncDelta> }
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(dead_code)]
    struct FuncDelta { name: Option<String>, arguments: Option<String> }
    #[derive(serde::Deserialize)]
    struct ChunkUsage { prompt_tokens: Option<u64>, completion_tokens: Option<u64> }

    let chunk: Chunk = match serde_json::from_str(data) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    if chunk.choices.is_empty() {
        if let Some(u) = chunk.usage {
            return vec![StreamEvent::Usage(ChatUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cached_input_tokens: None,
                reasoning_tokens: None,
                cache_write_tokens: None,
            })];
        }
        return vec![];
    }

    let mut events = Vec::new();

    for choice in &chunk.choices {
        let mut emitted_tool_event = false;
        if let Some(tcs) = &choice.delta.tool_calls {
            tracing::debug!(raw_tool_calls = %serde_json::to_string(tcs).unwrap_or_default(), "SSE tool_calls delta");

            for tc in tcs {
                let id = tc.id.clone().unwrap_or_default();
                let func = tc.function.as_ref();

                // First chunk for this tool call carries id + name → ToolCallStart.
                // GLM sends id + name + arguments all in one chunk.
                if !id.is_empty() && func.is_some_and(|f| f.name.is_some()) {
                    let initial_args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    events.push(StreamEvent::ToolCallStart {
                        id,
                        name: func.and_then(|f| f.name.clone()).unwrap_or_default(),
                        initial_arguments: initial_args,
                    });
                    emitted_tool_event = true;
                } else {
                    let args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    if !args.is_empty() {
                        let delta_id = if id.is_empty() { format!("#{}", tc.index) } else { id };
                        events.push(StreamEvent::ToolCallDelta { id: delta_id, delta: args });
                        emitted_tool_event = true;
                    }
                }
            }
        }

        // Skip content when tool_calls were present (some providers send both).
        if !emitted_tool_event {
            if let Some(text) = &choice.delta.content {
                if !text.is_empty() { events.push(StreamEvent::Delta { text: text.clone() }); }
            }
            if let Some(reasoning) = &choice.delta.reasoning_content {
                if !reasoning.is_empty() { events.push(StreamEvent::Thinking { text: reasoning.clone() }); }
            }
        }

        if let Some(ref r) = choice.finish_reason {
            let reason = match r.as_str() {
                "stop" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                "tool_calls" => StopReason::ToolUse,
                _ => StopReason::EndTurn,
            };
            events.push(StreamEvent::Done { reason });
        }
    }

    events
}