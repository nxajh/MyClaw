//! OpenAI Chat Completions message rendering.
//!
//! Converts internal `ChatMessage` / `ChatRequest` into the JSON body expected
//! by the OpenAI Chat Completions endpoint (and OpenAI-compatible providers).

use serde_json::json;
use crate::providers::{ChatRequest, ContentPart};

fn detect_image_media_type(b64: &str) -> &'static str {
    if b64.starts_with("/9j/")   { "image/jpeg" }
    else if b64.starts_with("iVBOR") { "image/png"  }
    else if b64.starts_with("R0lG")  { "image/gif"  }
    else if b64.starts_with("UklG")  { "image/webp" }
    else                              { "image/jpeg" }
}

/// Map an audio MIME type to the OpenAI `input_audio.format` token. OpenAI
/// accepts a short codec name ("wav", "mp3", ...) rather than a MIME type.
/// Falls back to "wav" when the type is absent or unrecognized; an unsupported
/// codec simply makes the transcription call fail, which the modality adapter
/// already degrades gracefully.
fn audio_format_hint(media_type: Option<&str>) -> &'static str {
    match media_type.map(|m| m.trim().to_ascii_lowercase()) {
        Some(m) if m.contains("mpeg") || m.contains("mp3") => "mp3",
        Some(m) if m.contains("ogg") || m.contains("opus") || m.contains("oga") => "ogg",
        Some(m) if m.contains("wav") || m.contains("x-wav") || m.contains("wave") => "wav",
        Some(m) if m.contains("webm") => "webm",
        Some(m) if m.contains("m4a") || m.contains("mp4") || m.contains("aac") => "m4a",
        Some(m) if m.contains("flac") => "flac",
        _ => "wav",
    }
}

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
            // Thinking blocks are not supported by OpenAI — skip them entirely.
            let content_vec: Vec<serde_json::Value> = msg.parts.iter().filter_map(|part| match part {
                ContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
                ContentPart::ImageUrl { url, detail } => Some(json!({
                    "type": "image_url",
                    "image_url": { "url": url, "detail": format!("{:?}", detail).to_lowercase() }
                })),
                ContentPart::ImageB64 { b64_json, detail, media_type } => {
                    let mime = media_type.as_deref().unwrap_or_else(|| detect_image_media_type(b64_json));
                    Some(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", mime, b64_json),
                            "detail": format!("{:?}", detail).to_lowercase()
                        }
                    }))
                }
                ContentPart::ImageRef { .. } => {
                    unreachable!("ImageRef is disk-only; hydrate before render")
                }
                // `input_audio` content block (gpt-4o-audio family / OpenAI-
                // compatible STT models). Reached only when this model is the
                // auxiliary transcription model — the primary model never sees
                // audio because the modality adapter transcribes it to text first.
                ContentPart::AudioB64 { b64_json, media_type } => Some(json!({
                    "type": "input_audio",
                    "input_audio": {
                        "data": b64_json,
                        "format": audio_format_hint(media_type.as_deref()),
                    }
                })),
                ContentPart::AudioRef { .. } => {
                    unreachable!("AudioRef is disk-only; hydrate before render")
                }
                ContentPart::Thinking { .. } => None,
            }).collect();

            let content = if content_vec.is_empty() {
                json!("")
            } else if content_vec.len() == 1 {
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

    // max_completion_tokens is the current parameter; include max_tokens for
    // providers that haven't updated yet.
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::capability_chat::ChatMessage;

    #[test]
    fn audio_format_hint_maps_common_mimes() {
        assert_eq!(audio_format_hint(Some("audio/ogg")), "ogg");
        assert_eq!(audio_format_hint(Some("audio/mpeg")), "mp3");
        assert_eq!(audio_format_hint(Some("audio/wav")), "wav");
        assert_eq!(audio_format_hint(Some("audio/webm")), "webm");
        assert_eq!(audio_format_hint(None), "wav");
        assert_eq!(audio_format_hint(Some("application/octet-stream")), "wav");
    }

    #[test]
    fn renders_audio_as_input_audio_block() {
        let messages = [ChatMessage {
            role: "user".into(),
            parts: vec![ContentPart::AudioB64 {
                b64_json: "QUJD".into(),
                media_type: Some("audio/ogg".into()),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }];
        let req = ChatRequest {
            model: "m",
            messages: &messages,
            temperature: None,
            max_tokens: None,
            thinking: None,
            stop: None,
            seed: None,
            tools: None,
            stream: true,
        };
        let body = render_openai_chat_body(&req);
        // A lone non-text part is rendered as the content object directly.
        let block = &body["messages"][0]["content"];
        assert_eq!(block["type"], "input_audio");
        assert_eq!(block["input_audio"]["data"], "QUJD");
        assert_eq!(block["input_audio"]["format"], "ogg");
    }
}
