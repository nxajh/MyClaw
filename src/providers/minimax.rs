//! MiniMax provider — OpenAI-compatible protocol.
//!
//! Handles MiniMax-specific quirks:
//! - SSE streaming via `/text/chatcompletion_v2`
//! - Occasionally, the model emits tool-call JSON as plain text in `content`
//!   instead of using the structured `tool_calls` field.  We detect and
//!   correct this at the provider level so upstream consumers never see it.

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
    user_agent: Option<String>,
}

impl MiniMaxProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, client: Client::new(), user_agent: None }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }
}

#[async_trait]
impl ChatProvider for MiniMaxProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = format!("{}/text/chatcompletion_v2", self.base_url);
        let body = build_minimax_body(&req);
        let auth = crate::providers::shared::build_auth(&crate::providers::shared::AuthStyle::Bearer, &self.api_key);
        let client = self.client.clone();
        let user_agent = self.user_agent.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
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

            let mut saw_tool_call = false;
            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();

            // Accumulator for leaked tool-call detection.
            // MiniMax occasionally sends tool-call JSON as plain `content`
            // text instead of using the `tool_calls` delta field.  We buffer
            // content deltas and inspect them when the stream ends; if they
            // look like a tool call we emit proper tool-call events instead.
            let mut content_buf = String::new();

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
                    if let Some(event) = parse_minimax_sse(&line, &mut saw_tool_call, &mut content_buf) {
                        let _ = tx.send(event).await;
                    }
                }
            }

            // ── Leaked tool-call detection ──────────────────────────────
            // If we accumulated content that looks like a tool-call JSON,
            // emit proper tool-call events so the agent loop handles them.
            if !saw_tool_call && !content_buf.is_empty() {
                if let Some(calls) = extract_leaked_tool_calls(&content_buf) {
                    if !calls.is_empty() {
                        tracing::info!(
                            leaked = calls.len(),
                            "MiniMax leaked tool-call JSON in content, correcting to structured events"
                        );
                        saw_tool_call = true;
                        for (idx, tc) in calls.iter().enumerate() {
                            let id = tc.id.clone();
                            let name = tc.name.clone();
                            let arguments = tc.arguments.clone();

                            // Emit ToolCallStart for the first chunk.
                            let _ = tx.send(StreamEvent::ToolCallStart {
                                id: id.clone(),
                                name,
                                initial_arguments: arguments,
                            }).await;

                            // If there are multiple tool calls, subsequent
                            // ones need their own ToolCallStart (already done
                            // in the loop) but the agent loop's collect_stream
                            // expects ToolCallStart per call.
                            let _ = idx; // suppress unused-var warning
                        }
                        // Clear content_buf so no Delta is emitted.
                        content_buf.clear();
                    }
                }
            }

            // If content was accumulated but NOT a leaked tool call,
            // emit it as a regular Delta now (before Done).
            if !content_buf.is_empty() {
                let _ = tx.send(StreamEvent::Delta { text: content_buf }).await;
            }

            let final_reason = if saw_tool_call { StopReason::ToolUse } else { StopReason::EndTurn };
            let _ = tx.send(StreamEvent::Done { reason: final_reason }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

// ── Body building ────────────────────────────────────────────────────────────

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
            ContentPart::Thinking { .. } => {
                // MiniMax does not support thinking blocks — skip silently.
                serde_json::json!({"type": "text", "text": ""})
            },
        }).collect();

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

        if msg.role == "tool" {
            if let Some(tc_id) = &msg.tool_call_id {
                msg_json["tool_call_id"] = serde_json::json!(tc_id);
            } else if let Some(n) = &msg.name {
                msg_json["tool_call_id"] = serde_json::json!(n);
            }
        } else if msg.role == "assistant" {
            msg_json["content"] = if content.is_string() && content.as_str().unwrap_or("").is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(content)
            };
            if let Some(tcs) = &msg.tool_calls {
                msg_json["tool_calls"] = serde_json::json!(tcs.iter().map(|tc| tc.to_openai()).collect::<Vec<_>>());
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

// ── SSE parsing ──────────────────────────────────────────────────────────────

/// Parse a single SSE line from MiniMax.
///
/// `content_buf` accumulates plain-text content deltas so we can detect
/// leaked tool-call JSON at the end of the stream.  When the stream is
/// still in progress we hold back the content; if a `tool_calls` delta
/// arrives mid-stream we flush the buffer as a normal Delta (it was real text).
fn parse_minimax_sse(
    line: &str,
    saw_tool_call: &mut bool,
    content_buf: &mut String,
) -> Option<StreamEvent> {
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
        // Tool calls take priority.
        if let Some(tcs) = &choice.delta.tool_calls {
            *saw_tool_call = true;

            // If we had buffered content and now see a real tool_call,
            // the buffered content was genuine text — but if tool_calls
            // and content arrive in the same chunk, the content is almost
            // always the leaked text representation.  Discard it.
            content_buf.clear();

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

        // Content: buffer it instead of emitting immediately.
        // This allows us to detect leaked tool-call JSON at stream end.
        if let Some(text) = &choice.delta.content {
            if !text.is_empty() {
                // If we already saw a real tool_call, content is genuine text.
                if *saw_tool_call {
                    return Some(StreamEvent::Delta { text: text.clone() });
                }
                // Otherwise, buffer it for later inspection.
                content_buf.push_str(text);
                // Don't return an event yet — we'll emit it at stream end
                // if it turns out NOT to be a leaked tool call.
                return None;
            }
        }

        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() { return Some(StreamEvent::Thinking { text: reasoning.clone() }); }
        }
        if choice.finish_reason.is_some() {
            let raw = choice.finish_reason.as_ref().unwrap();
            let reason = match raw.as_str() {
                "stop" if *saw_tool_call => StopReason::ToolUse,
                "stop" => StopReason::EndTurn,
                "tool_calls" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                _ => StopReason::EndTurn,
            };
            return Some(StreamEvent::Done { reason });
        }
    }

    None
}

// ── Leaked tool-call detection ───────────────────────────────────────────────

/// A parsed leaked tool call (id + name + arguments).
struct LeakedToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Try to extract one or more tool-call objects from `text`.
///
/// MiniMax occasionally produces responses like:
///
/// ```json
/// {"id":"call_function_xxx","name":"shell","arguments":"{\"command\":\"ls\"}"}
/// ```
///
/// Returns `Some(vec![...])` if at least one valid tool call was found,
/// or `None` if the text doesn't look like a tool call.
fn extract_leaked_tool_calls(text: &str) -> Option<Vec<LeakedToolCall>> {
    let trimmed = text.trim();

    // Fast-path: must start with `{` to be a candidate.
    if !trimmed.starts_with('{') {
        return None;
    }

    // Try parsing as a single tool-call object.
    if let Some(call) = try_parse_tool_call_json(trimmed) {
        return Some(vec![call]);
    }

    // Maybe multiple JSON objects concatenated.  Split by brace depth.
    let mut calls = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, ch) in trimmed.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let fragment = trimmed[start..=i].trim();
                    if let Some(call) = try_parse_tool_call_json(fragment) {
                        calls.push(call);
                    }
                    start = i + 1;
                }
            }
            _ => {}
        }
    }

    if calls.is_empty() { None } else { Some(calls) }
}

/// Try to parse a single JSON string as a tool call.
///
/// Returns `Some(LeakedToolCall)` if the JSON has the shape:
/// `{"id":"call_...", "name":"...", "arguments":"..."}`
fn try_parse_tool_call_json(json_str: &str) -> Option<LeakedToolCall> {
    #[derive(serde::Deserialize)]
    struct RawToolCall {
        id: Option<String>,
        name: Option<String>,
        arguments: Option<serde_json::Value>,
    }

    let raw: RawToolCall = serde_json::from_str(json_str).ok()?;

    let id = raw.id?;
    let name = raw.name?;

    if id.is_empty() || name.is_empty() {
        return None;
    }

    // MiniMax leaked tool calls always have ids starting with "call".
    if !id.starts_with("call") {
        return None;
    }

    let arguments = match raw.arguments {
        Some(serde_json::Value::String(s)) => s,
        Some(v @ serde_json::Value::Object(_)) => v.to_string(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };

    Some(LeakedToolCall { id, name, arguments })
}
