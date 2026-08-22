//! OpenAI Chat Completions message rendering.
//!
//! Converts internal `ChatMessage` / `ChatRequest` into the JSON body expected
//! by the OpenAI Chat Completions endpoint (and OpenAI-compatible providers).

use crate::providers::{ChatRequest, ContentPart};
use base64::Engine as _;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Tool-call ids already WARNed about for empty `arguments` (issue #113).
/// History is re-sent on every request, so a single stale historical
/// tool_call with empty arguments re-triggered this WARN on every
/// subsequent turn (observed: 21x/2h for one static defect) — pure log
/// noise, not a new event each time. Dedup by id (stable across turns,
/// unlike message index which shifts as history grows/truncates) so it
/// fires once per distinct defect, then drops to DEBUG.
fn warned_empty_args_ids() -> &'static Mutex<HashSet<String>> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
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
    // Debug: log message count and check for empty tool_call names
    {
        let mut empty_name_count = 0;
        for (i, msg) in req.messages.iter().enumerate() {
            if let Some(tcs) = &msg.tool_calls {
                for tc in tcs {
                    if tc.name.is_empty() {
                        tracing::error!(
                            idx = i,
                            id = %tc.id,
                            "render_openai_chat_body: EMPTY tool_call name in message"
                        );
                        empty_name_count += 1;
                    }
                    if tc.arguments.trim().is_empty() {
                        let first_time = warned_empty_args_ids()
                            .lock()
                            .unwrap()
                            .insert(tc.id.clone());
                        if first_time {
                            tracing::warn!(
                                idx = i,
                                id = %tc.id,
                                name = %tc.name,
                                "render_openai_chat_body: EMPTY tool_call arguments (will be sanitized to \"{{}}\"); \
                                 further occurrences of this same tool_call id are logged at DEBUG, not re-warned"
                            );
                        } else {
                            tracing::debug!(
                                idx = i,
                                id = %tc.id,
                                name = %tc.name,
                                "render_openai_chat_body: EMPTY tool_call arguments (already warned for this id)"
                            );
                        }
                    }
                }
            }
        }
        tracing::info!(
            msg_count = req.messages.len(),
            model = req.model,
            empty_name_count,
            tool_count = req.tools.map(|t| t.len()).unwrap_or(0),
            "render_openai_chat_body: rendering request"
        );
    }
    let messages: Vec<serde_json::Value> = req.messages
        .iter()
        .map(|msg| {
            // Thinking blocks are not supported by OpenAI — skip them entirely.
            let content_vec: Vec<serde_json::Value> = msg.parts.iter().filter_map(|part| match part {
                ContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
                ContentPart::File { path, mime_type, .. } => {
                    let modality = crate::providers::media::modality_from_mime(mime_type.as_deref(), path);
                    let abs = crate::providers::media::resolve_path(path);
                    let bytes = match std::fs::read(&abs) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            tracing::warn!(
                                path = %path,
                                resolved = %abs.display(),
                                err = %e,
                                "openai chat rendering: failed to read media file, falling back to marker text"
                            );
                            return Some(json!({"type": "text", "text": format!("{}（读取失败: {e}）", crate::providers::media::marker_for_file(path, mime_type.as_deref()))}));
                        }
                    };
                    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                    match modality {
                        crate::providers::media::FileModality::Image => {
                            let mime = mime_type.as_deref()
                                .or_else(|| crate::providers::media::infer_image_mime(path))
                                .unwrap_or("image/jpeg");
                            Some(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", mime, b64),
                                    "detail": "auto"
                                }
                            }))
                        }
                        crate::providers::media::FileModality::Audio => Some(json!({
                            "type": "input_audio",
                            "input_audio": {
                                "data": b64,
                                "format": audio_format_hint(mime_type.as_deref()),
                            }
                        })),
                        crate::providers::media::FileModality::Video => {
                            let mime = mime_type.as_deref()
                                .or_else(|| crate::providers::media::infer_video_mime(path))
                                .unwrap_or("video/mp4");
                            Some(json!({
                                "type": "video_url",
                                "video_url": { "url": format!("data:{};base64,{}", mime, b64) }
                            }))
                        }
                        crate::providers::media::FileModality::Other => Some(json!({"type": "text", "text": crate::providers::media::marker_for_file(path, mime_type.as_deref())})),
                    }
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

    // Log a content-type summary for debugging media handling.
    {
        let mut summaries = Vec::new();
        for msg in req.messages {
            let mut kinds = Vec::new();
            for part in &msg.parts {
                match part {
                    ContentPart::Text { text } => {
                        let preview: String = text.chars().take(40).collect();
                        kinds.push(format!("text({})", preview));
                    }
                    ContentPart::File {
                        mime_type,
                        size_bytes,
                        path,
                        ..
                    } => {
                        let modality =
                            crate::providers::media::modality_from_mime(mime_type.as_deref(), path);
                        let label = match modality {
                            crate::providers::media::FileModality::Image => "image",
                            crate::providers::media::FileModality::Audio => "audio",
                            crate::providers::media::FileModality::Video => "video",
                            crate::providers::media::FileModality::Other => "file",
                        };
                        let mb = size_bytes
                            .map(|b| format!("{:.1}MB", b as f64 / 1048576.0))
                            .unwrap_or_default();
                        kinds.push(format!("{}({})", label, mb));
                    }
                    ContentPart::Thinking { .. } => kinds.push("thinking".to_string()),
                }
            }
            if !kinds.is_empty() {
                summaries.push(format!("{}:[{}]", msg.role, kinds.join(",")));
            }
        }
        tracing::info!(
            model = req.model,
            msg_count = req.messages.len(),
            content = %summaries.join(" | "),
            "render_openai_chat_body: content summary"
        );
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }

    // max_completion_tokens is the current parameter; include max_tokens for
    // providers that haven't updated yet.
    if let Some(max) = req.max_tokens {
        body["max_completion_tokens"] = json!(max);
        body["max_tokens"] = json!(max);
    }
    if let Some(stop) = &req.stop {
        body["stop"] = json!(stop);
    }
    if let Some(seed) = req.seed {
        body["seed"] = json!(seed);
    }
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

    /// issue #113: history is re-sent on every request, so a stale
    /// historical tool_call with empty arguments must only be WARNed about
    /// once per id — not every time render_openai_chat_body replays it.
    #[test]
    fn empty_tool_call_args_warned_only_once_per_id() {
        let id = format!("dedup-test-{}", std::process::id());
        let messages = [ChatMessage {
            role: "assistant".into(),
            parts: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![crate::providers::capability_chat::ToolCall {
                id: id.clone(),
                name: "some_tool".into(),
                arguments: "   ".into(),
            }]),
            is_error: None,
            model: None,
            usage: None,
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

        assert!(!warned_empty_args_ids().lock().unwrap().contains(&id));
        render_openai_chat_body(&req);
        assert!(
            warned_empty_args_ids().lock().unwrap().contains(&id),
            "first occurrence must be recorded"
        );
        // Second (and any further) render of the same historical message
        // must not re-insert or otherwise misbehave — this is the "history
        // replay" scenario the issue describes.
        render_openai_chat_body(&req);
        assert!(warned_empty_args_ids().lock().unwrap().contains(&id));
    }

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
        let path =
            std::env::temp_dir().join(format!("myclaw-audio-test-{}.ogg", std::process::id()));
        std::fs::write(&path, b"ABC").unwrap();
        let messages = [ChatMessage {
            role: "user".into(),
            parts: vec![ContentPart::File {
                path: path.to_string_lossy().to_string(),
                mime_type: Some("audio/ogg".into()),
                name: None,
                size_bytes: Some(3),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
            model: None,
            usage: None,
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
