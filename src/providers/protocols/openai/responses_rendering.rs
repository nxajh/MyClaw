//! Body rendering for the OpenAI Responses API.
//!
//! Converts `ChatRequest` (messages, tools, params) into the JSON body
//! expected by `POST /v1/responses`.  Key differences from Chat Completions:
//!
//! - `messages` → `input` (array of items, not messages)
//! - `max_tokens` → `max_output_tokens`
//! - Function tools are flat (`{type:"function", name, ...}`) not nested
//! - Tool results use `{type:"function_call_output", call_id, output}`
//! - `store: false` (MyClaw manages conversation history itself)

use base64::Engine;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, ContentPart};
use crate::providers::media;

pub fn render_responses_body(req: &ChatRequest<'_>) -> serde_json::Value {
    use serde_json::json;

    // ── Render messages → input items ──────────────────────────────────────
    let mut input_items: Vec<serde_json::Value> = Vec::new();

    for msg in req.messages.iter() {
        match msg.role.as_str() {
            "system" => {
                // System messages: pass as role=system in input array.
                let text = msg.text_content();
                input_items.push(json!({"role": "system", "content": text}));
            }
            "user" => {
                let item = render_user_message(msg);
                input_items.push(item);
            }
            "assistant" => {
                // Assistant message may contain text, tool_calls, or both.
                // For Responses API, text and function_calls are separate items.
                let text = msg.text_content();
                if !text.is_empty() {
                    let content_parts = render_content_parts(msg, "output_text");
                    let content = if content_parts.len() == 1 {
                        // Simplified: if single text part, use string content.
                        if let Some(serde_json::Value::Object(map)) = content_parts.first() {
                            if map.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                                json!(map.get("text").cloned().unwrap_or(json!("")))
                            } else {
                                json!(content_parts)
                            }
                        } else {
                            json!(content_parts)
                        }
                    } else if content_parts.is_empty() {
                        json!("")
                    } else {
                        json!(content_parts)
                    };
                    input_items.push(json!({"role": "assistant", "content": content}));
                }
                // Emit function_call items for each tool call.
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        input_items.push(json!({
                            "type": "function_call",
                            "name": tc.name,
                            "call_id": tc.id,
                            "arguments": if tc.arguments.trim().is_empty() {
                                "{}"
                            } else {
                                tc.arguments.as_str()
                            }
                        }));
                    }
                }
            }
            "tool" => {
                // Tool result → function_call_output item.
                let text = msg.text_content();
                let call_id = msg
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| msg.name.clone().unwrap_or_default());
                input_items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": text
                }));
            }
            other => {
                // Unknown role — treat as user.
                let text = msg.text_content();
                input_items.push(json!({"role": other, "content": text}));
            }
        }
    }

    // ── Build body ─────────────────────────────────────────────────────────
    let mut body = json!({
        "model": req.model,
        "input": input_items,
        "stream": true,
        "store": false,
        "truncation": "disabled",
    });

    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }

    if let Some(max) = req.max_tokens {
        body["max_output_tokens"] = json!(max);
    }

    if let Some(stop) = &req.stop {
        // Responses API has no stop sequences field; silently skip.
        let _ = stop;
    }

    // ── Tools ──────────────────────────────────────────────────────────────
    if let Some(tools) = req.tools {
        if !tools.is_empty() {
            let tools_arr: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema
                    })
                })
                .collect();
            body["tools"] = json!(tools_arr);
            body["parallel_tool_calls"] = json!(true);
        }
    }

    // ── Reasoning / thinking ───────────────────────────────────────────────
    if let Some(thinking) = &req.thinking {
        if thinking.enabled {
            if let Some(ref effort) = thinking.effort {
                body["reasoning"] = json!({"effort": effort});
            }
        }
    }

    body
}

/// Render content parts for a message.
/// `text_type` is "input_text" for user messages, "output_text" for assistant.
fn render_content_parts(msg: &ChatMessage, text_type: &str) -> Vec<serde_json::Value> {
    use serde_json::json;

    msg.parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(json!({"type": text_type, "text": text})),
            ContentPart::Thinking { .. } => None,
            ContentPart::File {
                path,
                mime_type,
                ..
            } => {
                let modality = media::modality_from_mime(mime_type.as_deref(), path);
                let abs = media::resolve_path(path);
                let bytes = match std::fs::read(&abs) {
                    Ok(b) => b,
                    Err(e) => {
                        return Some(json!({
                            "type": text_type,
                            "text": format!(
                                "{}（读取失败: {e}）",
                                media::marker_for_file(path, mime_type.as_deref())
                            )
                        }));
                    }
                };
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                match modality {
                    media::FileModality::Image => {
                        let mime = mime_type
                            .as_deref()
                            .or_else(|| media::infer_image_mime(path))
                            .unwrap_or("image/jpeg");
                        Some(json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", mime, b64)
                        }))
                    }
                    media::FileModality::Audio => {
                        let format = mime_type.as_deref().unwrap_or("audio/mp3");
                        Some(json!({
                            "type": "input_audio",
                            "input_audio": {
                                "data": b64,
                                "format": format.rsplit('/').next().unwrap_or("mp3"),
                            }
                        }))
                    }
                    media::FileModality::Video => {
                        let mime = mime_type
                            .as_deref()
                            .or_else(|| media::infer_video_mime(path))
                            .unwrap_or("video/mp4");
                        Some(json!({
                            "type": "input_video",
                            "video_url": format!("data:{};base64,{}", mime, b64)
                        }))
                    }
                    media::FileModality::Other => Some(json!({
                        "type": text_type,
                        "text": media::marker_for_file(path, mime_type.as_deref())
                    })),
                }
            }
        })
        .collect()
}

/// Render a user message into an input item.
fn render_user_message(msg: &ChatMessage) -> serde_json::Value {
    use serde_json::json;

    let parts = render_content_parts(msg, "input_text");
    if parts.is_empty() {
        return json!({"role": "user", "content": ""});
    }
    // Simplified format: single text-only part → string content.
    if parts.len() == 1 {
        if let Some(serde_json::Value::Object(map)) = parts.first() {
            if map.get("type").and_then(|t| t.as_str()) == Some("input_text") {
                if let Some(text) = map.get("text") {
                    return json!({"role": "user", "content": text});
                }
            }
        }
    }
    json!({"role": "user", "content": parts})
}
