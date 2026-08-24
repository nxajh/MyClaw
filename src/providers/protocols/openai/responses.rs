//! OpenAI Responses API client.
//!
//! Implements `ChatProvider` for the `POST /v1/responses` endpoint.
//! Used by grok-4.5 via CPA or api.x.ai to get working server-side
//! web_search and clean function calling.

use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::HashMap;

use crate::providers::http::build_reqwest_client;
use crate::providers::protocols::openai::responses_rendering::render_responses_body;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, SharedApiKey, StopReason, StreamEvent};
use reqwest::Client;

/// Post-process the rendered body with provider-specific fields.
pub type BodyOverrideFn = fn(serde_json::Value, &ChatRequest<'_>) -> serde_json::Value;

/// OpenAI Responses protocol client.
#[derive(Clone)]
pub struct OpenAiResponsesClient {
    base_url: String,
    api_key: SharedApiKey,
    client: Client,
    user_agent: Option<String>,
    body_override: Option<BodyOverrideFn>,
    hosted_tools: Vec<String>,
}

impl OpenAiResponsesClient {
    pub fn new(api_key: impl Into<SharedApiKey>, base_url: String) -> Self {
        Self {
            base_url,
            api_key: api_key.into(),
            client: build_reqwest_client(),
            user_agent: None,
            body_override: None,
            hosted_tools: Vec::new(),
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    pub fn with_body_override(mut self, f: BodyOverrideFn) -> Self {
        self.body_override = Some(f);
        self
    }

    pub fn with_hosted_tools(mut self, tools: Vec<String>) -> Self {
        self.hosted_tools = tools;
        self
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key.get())
    }

    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    fn common_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            self.auth().parse().unwrap(),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if let Some(ref ua) = self.user_agent {
            headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
        }
        headers
    }
}

#[async_trait]
impl ChatProvider for OpenAiResponsesClient {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.responses_url();
        let body = render_responses_body(&req);
        let mut body = match self.body_override {
            Some(f) => f(body, &req),
            None => body,
        };

        // Apply hosted tools: filter local function tools, add hosted built-ins.
        if !self.hosted_tools.is_empty() {
            apply_hosted_tools(&mut body, &self.hosted_tools);
        }
        let client = self.client.clone();
        let headers = self.common_headers();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let resp = match tokio::time::timeout(
                crate::providers::shared::REQUEST_SEND_TIMEOUT,
                client.post(&url).headers(headers).json(&body).send(),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!(url = %url, error = %e, "responses request failed");
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
                Err(_) => {
                    tracing::warn!(
                        url = %url,
                        timeout_secs = crate::providers::shared::REQUEST_SEND_TIMEOUT.as_secs(),
                        "responses request send timed out"
                    );
                    let _ = tx
                        .send(StreamEvent::Error(format!(
                            "request timed out after {}s",
                            crate::providers::shared::REQUEST_SEND_TIMEOUT.as_secs()
                        )))
                        .await;
                    return;
                }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = match tokio::time::timeout(
                    crate::providers::shared::ERROR_BODY_TIMEOUT,
                    resp.text(),
                )
                .await
                {
                    Ok(t) => t.unwrap_or_default(),
                    Err(_) => String::new(),
                };
                let _ = tx
                    .send(StreamEvent::HttpError {
                        status: status.as_u16(),
                        message: format!("HTTP {}: {}", status, text),
                    })
                    .await;
                return;
            }

            // Track state across the SSE stream.
            let mut saw_tool_call = false;
            let mut stream_completed = false;
            // Maps output_index → call_id for function calls (to correlate
            // argument deltas with their call_id).
            let mut index_to_call_id: HashMap<u64, String> = HashMap::new();
            // Maps output_index → sequential tool_calls index (0, 1, 2, ...).
            // Needed because output_index counts ALL output items (reasoning,
            // message, function_call), not just function calls.
            let mut index_to_tool_idx: HashMap<u64, u32> = HashMap::new();
            let mut buffer = String::new();
            let mut utf8_decoder = crate::providers::shared::Utf8StreamDecoder::new();
            let mut current_event_type: Option<String> = None;
            let mut stream = resp.bytes_stream();

            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(url = %url, error = %e, "responses stream read error");
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        return;
                    }
                };
                let (text, invalid) = utf8_decoder.push(&bytes);
                for diag in invalid {
                    tracing::warn!(
                        url = %url,
                        valid_up_to = diag.valid_up_to,
                        bad_len = diag.bad_len,
                        "invalid UTF-8 byte sequence in responses SSE stream, skipping"
                    );
                }
                if text.is_empty() {
                    continue;
                }
                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].to_string();
                    buffer.drain(..=pos);

                    let trimmed = line.trim();

                    // Track event type from "event:" lines.
                    if let Some(ev) = trimmed.strip_prefix("event:") {
                        current_event_type = Some(ev.trim().to_string());
                        continue;
                    }

                    // Empty line resets event type (SSE event boundary).
                    if trimmed.is_empty() {
                        current_event_type = None;
                        continue;
                    }

                    // Parse "data:" lines.
                    let data = match trimmed.strip_prefix("data:") {
                        Some(d) => d.trim(),
                        None => continue, // skip comments (: prefix) and unknown lines
                    };

                    if data.is_empty() {
                        continue;
                    }

                    let event_type = current_event_type.as_deref().unwrap_or("");
                    let events = parse_responses_sse(
                        event_type,
                        data,
                        &mut index_to_call_id,
                        &mut index_to_tool_idx,
                    );

                    for ev in events {
                        match &ev {
                            StreamEvent::ToolCallStart { .. }
                            | StreamEvent::ToolCallDelta { .. }
                            | StreamEvent::ToolCallEnd { .. } => {
                                saw_tool_call = true;
                                let _ = tx.send(ev).await;
                            }
                            StreamEvent::Done { reason } => {
                                stream_completed = true;
                                let _ = tx
                                    .send(StreamEvent::Done {
                                        reason: *reason,
                                    })
                                    .await;
                            }
                            _ => {
                                let _ = tx.send(ev).await;
                            }
                        }
                    }
                }
            }

            // If the stream ended without a terminal event, emit an error.
            if !stream_completed {
                if saw_tool_call {
                    let _ = tx
                        .send(StreamEvent::Done {
                            reason: StopReason::ToolUse,
                        })
                        .await;
                } else {
                    let _ = tx
                        .send(StreamEvent::Error(
                            "responses stream closed before completion (no response.completed)"
                                .to_string(),
                        ))
                        .await;
                }
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

/// Replace local function tools with hosted built-in tools.
///
/// For each hosted tool name (e.g. "web_search"):
/// - Remove the matching function tool from `tools` array
/// - Append `{"type": "<name>"}` as a built-in tool
fn apply_hosted_tools(body: &mut serde_json::Value, hosted_tools: &[String]) {
    if body.get("tools").is_none() {
        body["tools"] = serde_json::json!([]);
    }
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        tools.retain(|t| {
            if t.get("type").and_then(|s| s.as_str()) == Some("function") {
                if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                    return !hosted_tools.iter().any(|ht| ht == name);
                }
            }
            true
        });
        for ht in hosted_tools {
            tools.push(serde_json::json!({"type": ht}));
        }
    }
    body["parallel_tool_calls"] = serde_json::json!(true);
}

/// Parse a single Responses API SSE data line into StreamEvents.
///
/// `event_type` is the value from the preceding `event:` line
/// (e.g. "response.output_text.delta").
/// `data` is the raw JSON string from the `data:` line.
/// `index_to_call_id` maps output_index → call_id for function call correlation.
/// `index_to_tool_idx` maps output_index → sequential tool_calls index.
fn parse_responses_sse(
    event_type: &str,
    data: &str,
    index_to_call_id: &mut HashMap<u64, String>,
    index_to_tool_idx: &mut HashMap<u64, u32>,
) -> Vec<StreamEvent> {
    use crate::providers::{ChatUsage, StopReason};

    let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            // Was silent before #91: a malformed chunk here can drop part of
            // a function_call_arguments delta the same way a Chat Completions
            // chunk parse failure can drop a whole tool call. `response.completed`
            // still reports StopReason::ToolUse correctly (it's a full snapshot,
            // not reconstructed from these deltas), but the tool call's
            // *arguments* reassembled from deltas may be truncated/corrupted —
            // this WARN at least makes that visible instead of silently eating
            // the chunk.
            tracing::warn!(
                event_type = %event_type,
                error = %e,
                data_len = data.len(),
                "responses SSE chunk JSON parse failed (event dropped)"
            );
            return vec![];
        }
    };

    match event_type {
        // ── Text output deltas ──────────────────────────────────────────────
        "response.output_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(|d| d.as_str()) {
                if !delta.is_empty() {
                    return vec![StreamEvent::Delta {
                        text: delta.to_string(),
                    }];
                }
            }
        }

        // ── Reasoning / thinking deltas ─────────────────────────────────────
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(|d| d.as_str()) {
                if !delta.is_empty() {
                    return vec![StreamEvent::Thinking {
                        text: delta.to_string(),
                    }];
                }
            }
        }

        // ── Function call lifecycle ─────────────────────────────────────────
        "response.output_item.added" => {
            if let Some(item) = value.get("item") {
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let output_index = value
                        .get("output_index")
                        .and_then(|i| i.as_u64())
                        .unwrap_or(0);
                    index_to_call_id.insert(output_index, call_id.clone());
                    let tool_idx = index_to_tool_idx.len() as u32;
                    index_to_tool_idx.insert(output_index, tool_idx);

                    return vec![StreamEvent::ToolCallStart {
                        id: call_id,
                        name,
                        initial_arguments: String::new(),
                    }];
                }
            }
        }

        "response.function_call_arguments.delta" => {
            let delta = value
                .get("delta")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            if !delta.is_empty() {
                let output_index = value
                    .get("output_index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0);
                let call_id = index_to_call_id
                    .get(&output_index)
                    .cloned()
                    .unwrap_or_default();
                let tool_idx = index_to_tool_idx
                    .get(&output_index)
                    .copied()
                    .unwrap_or(0);
                return vec![StreamEvent::ToolCallDelta {
                    index: tool_idx,
                    id: call_id,
                    name: String::new(),
                    delta: delta.to_string(),
                }];
            }
        }

        "response.output_item.done" => {
            if let Some(item) = value.get("item") {
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = item
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}")
                        .to_string();

                    return vec![StreamEvent::ToolCallEnd {
                        id: call_id,
                        name,
                        arguments,
                    }];
                }
            }
        }

        // ── Terminal events ─────────────────────────────────────────────────
        "response.completed" => {
            let mut events = Vec::new();

            // Extract usage from response.usage
            if let Some(usage) = value
                .pointer("/response/usage")
                .or(value.get("response").and_then(|r| r.get("usage")))
            {
                let input_tokens = usage
                    .get("input_tokens")
                    .and_then(|t| t.as_u64())
                    .or_else(|| {
                        usage
                            .get("prompt_tokens")
                            .and_then(|t| t.as_u64())
                    });
                let output_tokens = usage
                    .get("output_tokens")
                    .and_then(|t| t.as_u64())
                    .or_else(|| {
                        usage
                            .get("completion_tokens")
                            .and_then(|t| t.as_u64())
                    });
                let cached_input_tokens = usage
                    .pointer("/input_tokens_details/cached_tokens")
                    .or_else(|| {
                        usage
                            .pointer("/prompt_tokens_details/cached_tokens")
                    })
                    .and_then(|t| t.as_u64());
                let reasoning_tokens = usage
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .or_else(|| {
                        usage
                            .pointer("/completion_tokens_details/reasoning_tokens")
                    })
                    .and_then(|t| t.as_u64());

                events.push(StreamEvent::Usage(ChatUsage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    reasoning_tokens,
                    cache_write_tokens: None,
                }));
            }

            // Determine stop reason from output items
            let has_function_call = value
                .pointer("/response/output")
                .and_then(|o| o.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("function_call"))
                })
                .unwrap_or(false);

            let reason = if has_function_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };

            events.push(StreamEvent::Done { reason });
            return events;
        }

        "response.incomplete" => {
            let reason = value
                .pointer("/response/incomplete_details/reason")
                .and_then(|r| r.as_str());
            let stop = match reason {
                Some("max_output_tokens") | Some("max_tokens") => StopReason::MaxTokens,
                Some("content_filter") => StopReason::ContentFilter,
                _ => StopReason::MaxTokens,
            };
            return vec![StreamEvent::Done { reason: stop }];
        }

        "response.failed" | "response.error" => {
            let msg = value
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("responses API returned an error");
            return vec![StreamEvent::Error(msg.to_string())];
        }

        _ => {
            // Unhandled event type — ignore silently.
            // (web_search_call events, reasoning_summary_part, content_part,
            //  refusal, etc. don't need special handling.)
        }
    }

    vec![]
}
