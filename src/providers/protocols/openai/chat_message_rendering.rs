//! OpenAI Chat Completions message rendering.
//!
//! Converts internal `ChatMessage` / `ChatRequest` into the JSON body expected
//! by the OpenAI Chat Completions endpoint (and OpenAI-compatible providers).

use serde_json::json;
use crate::providers::{ChatRequest, ContentPart};

/// Build the request body for the OpenAI Chat Completions API.
///
/// Per the latest OpenAI documentation:
/// - `max_completion_tokens` is preferred over the deprecated `max_tokens`.
/// - `stream_options: { include_usage: true }` requests a final usage chunk.
/// - `parallel_tool_calls: true` when tools are present.
pub fn render_openai_chat_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = req.messages
        .iter()
        .map(|msg| {
            let content_vec: Vec<serde_json::Value> = msg.parts.iter().map(|part| match part {
                ContentPart::Text { text } => json!({"type": "text", "text": text}),
                ContentPart::ImageUrl { url, detail } => json!({
                    "type": "image_url",
                    "image_url": { "url": url, "detail": format!("{:?}", detail).to_lowercase() }
                }),
                ContentPart::ImageB64 { b64_json, detail, .. } => json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:image;base64,{}", b64_json), "detail": format!("{:?}", detail).to_lowercase() }
                }),
                ContentPart::Thinking { .. } => {
                    // OpenAI does not support thinking blocks — skip silently.
                    json!({"type": "text", "text": ""})
                }
            }).collect();

            let content = if content_vec.len() == 1 {
                if let Some(text) = msg.parts.iter().find_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                }) {
                    json!(text)
                } else {
                    content_vec.into_iter().next().unwrap()
                }
            } else {
                json!(content_vec)
            };

            let mut msg_json = json!({ "role": msg.role });

            if msg.role == "tool" {
                if let Some(tc_id) = &msg.tool_call_id {
                    msg_json["tool_call_id"] = json!(tc_id);
                } else if let Some(n) = &msg.name {
                    msg_json["tool_call_id"] = json!(n);
                }
                msg_json["content"] = json!(content);
            } else if msg.role == "assistant" {
                msg_json["content"] = if content.is_string() && content.as_str().unwrap_or("").is_empty() {
                    serde_json::Value::Null
                } else {
                    json!(content)
                };
                if let Some(tcs) = &msg.tool_calls {
                    msg_json["tool_calls"] = serde_json::json!(tcs.iter().map(|tc| tc.to_openai()).collect::<Vec<_>>());
                }
            } else {
                msg_json["content"] = json!(content);
            }

            msg_json
        })
        .collect();

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if let Some(temp) = req.temperature { body["temperature"] = json!(temp); }

    // max_completion_tokens is the current parameter; fall back to max_tokens
    // for providers that haven't updated yet.
    if let Some(max) = req.max_tokens {
        body["max_completion_tokens"] = json!(max);
        body["max_tokens"] = json!(max);
    }
    if let Some(stop) = &req.stop { body["stop"] = json!(stop); }
    if let Some(seed) = req.seed { body["seed"] = json!(seed); }
    if let Some(tools) = req.tools {
        body["tools"] = json!(tools.iter().map(|t| {
            json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.input_schema }
            })
        }).collect::<Vec<_>>());
        body["parallel_tool_calls"] = json!(true);
    }

    body
}