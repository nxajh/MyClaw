//! Anthropic Messages API message rendering.
//!
//! Converts internal `ChatMessage` / `ChatRequest` into the JSON body expected
//! by the Anthropic Messages endpoint (and Anthropic-compatible providers).

use serde_json::json;
use crate::providers::ChatRequest;

/// Infer image MIME type from the leading bytes of a base64-encoded image.
fn detect_image_media_type(b64: &str) -> &'static str {
    if b64.starts_with("/9j/")  { "image/jpeg" }
    else if b64.starts_with("iVBOR") { "image/png"  }
    else if b64.starts_with("R0lG")  { "image/gif"  }
    else if b64.starts_with("UklG")  { "image/webp" }
    else                              { "image/jpeg" }
}

/// Rendered Anthropic messages: top-level system prompt + conversation messages.
pub struct RenderedAnthropicMessages {
    pub system_prompt: Option<String>,
    pub messages: Vec<serde_json::Value>,
}

/// Build the request body for the Anthropic Messages API.
///
/// Handles:
/// - Extracting system messages to the top-level `system` field.
/// - Converting tool results to `tool_result` content blocks.
/// - Converting assistant tool_calls to `tool_use` content blocks.
/// - Merging consecutive same-role messages.
/// - Filtering empty text blocks.
pub fn render_anthropic_messages<'a>(req: &ChatRequest<'a>) -> RenderedAnthropicMessages {
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
                    crate::providers::ContentPart::ImageB64 { b64_json, media_type, .. } => {
                        let mime = media_type.as_deref()
                            .unwrap_or_else(|| detect_image_media_type(b64_json));
                        serde_json::json!({"type": "image", "source": {
                            "type": "base64", "media_type": mime, "data": b64_json,
                        }})
                    }
                    crate::providers::ContentPart::Thinking { thinking, signature } => {
                        let mut block = serde_json::json!({"type": "thinking", "thinking": thinking});
                        if let Some(sig) = signature {
                            block["signature"] = serde_json::json!(sig);
                        }
                        block
                    }
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
        "render_anthropic_messages: message merge complete"
    );

    // Final pass: filter out empty text blocks from content arrays.
    // Anthropic returns 400 "text content blocks must be non-empty".
    for msg in &mut messages {
        if let Some(serde_json::Value::Array(arr)) = msg.get_mut("content") {
            arr.retain(|p| {
                let is_text = p.get("type").and_then(|v| v.as_str()) == Some("text");
                if is_text {
                    let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if text.trim().is_empty() {
                        tracing::warn!("Dropping empty text block from message");
                        return false;
                    }
                }
                true
            });
        }
    }

    // After filtering empty blocks, some messages might have empty content arrays.
    // Assistant messages must NOT have empty content in Anthropic API.
    messages.retain(|msg| {
        let content = msg.get("content");
        let has_content = match content {
            Some(serde_json::Value::Array(arr)) => !arr.is_empty(),
            Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
            Some(serde_json::Value::Null) => false,
            _ => true,
        };
        if !has_content {
            tracing::warn!(role = ?msg.get("role"), "Dropping message with empty content");
        }
        has_content
    });

    RenderedAnthropicMessages {
        system_prompt: if system.is_empty() { None } else { Some(system) },
        messages,
    }
}

/// Build the full JSON request body from rendered messages.
pub fn build_anthropic_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    let rendered = render_anthropic_messages(req);

    let mut body = json!({
        "model": req.model,
        "messages": rendered.messages,
        "stream": true,
    });
    if let Some(system) = rendered.system_prompt {
        body["system"] = serde_json::json!(system);
    }
    if let Some(temp) = req.temperature { body["temperature"] = serde_json::json!(temp); }
    // max_tokens is required by the Anthropic API; default to 8192 when not set.
    body["max_tokens"] = serde_json::json!(req.max_tokens.unwrap_or(8192));
    if let Some(ref thinking) = req.thinking {
        if thinking.enabled {
            let budget_tokens: u32 = match thinking.effort.as_deref() {
                Some("high") => 10_000,
                Some("low")  =>  1_000,
                _            =>  5_000,
            };
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget_tokens});
        }
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