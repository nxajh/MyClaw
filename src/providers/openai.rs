//! OpenAI provider — Chat + Image + TTS + Embedding.
//!
//! Reference: https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
//!
//! Handles OpenAI-specific streaming behaviours:
//! - `finish_reason` can be `"stop"` even when tool_calls were emitted in prior
//!   chunks; we track `saw_tool_call` and override to ToolUse.
//! - `finish_reason` can be `"function_call"` (deprecated but still possible).
//! - `reasoning_content` in delta carries o-series chain-of-thought.
//! - `stream_options.include_usage` requests a final usage chunk.
//!
//! Body differences from the generic shared builder:
//! - `max_completion_tokens` is preferred over the deprecated `max_tokens`.
//! - `stream_options: { include_usage: true }` is added for token tracking.
//! - `parallel_tool_calls: true` when tools are present (OpenAI default, but
//!   explicit for safety).

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::Client;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, ContentPart, StreamEvent, StopReason,
};
use crate::providers::{
    EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider,
};
use crate::providers::{
    ImageGenerationProvider, ImageRequest, ImageResponse, ImageFormat, ImageOutput,
};
use crate::providers::{TtsProvider, TtsRequest, TtsFormat, TtsVoice};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

#[derive(Clone)]
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl OpenAiProvider {
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

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn chat_url(&self) -> String {
        if self.base_url.contains("/v1") || self.base_url.contains("/v4") {
            format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
        } else {
            format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'))
        }
    }
    fn images_url(&self) -> String { format!("{}/v1/images/generations", self.base_url.trim_end_matches('/')) }
    fn embeddings_url(&self) -> String { format!("{}/v1/embeddings", self.base_url.trim_end_matches('/')) }
    fn tts_url(&self) -> String { format!("{}/v1/audio/speech", self.base_url.trim_end_matches('/')) }

    /// Build a HeaderMap with common headers (auth, content-type, optional user-agent).
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

// ── ChatProvider ───────────────────────────────────────────────────────────────

#[async_trait]
impl ChatProvider for OpenAiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.chat_url();
        let body = build_openai_body(&req);
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
                    let parsed = parse_openai_sse(&line, &mut saw_tool_call);
                    if let Some(events) = parsed {
                        for ev in events {
                            let _ = tx.send(ev).await;
                        }
                    }
                }
            }
            // OpenAI may report finish_reason="stop" even when tool calls were present.
            let final_reason = if saw_tool_call { StopReason::ToolUse } else { StopReason::EndTurn };
            let _ = tx.send(StreamEvent::Done { reason: final_reason }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

// ── Body building ─────────────────────────────────────────────────────────────

/// Build the request body for the OpenAI Chat Completions API.
///
/// Per the latest OpenAI documentation:
/// - `max_completion_tokens` is preferred over the deprecated `max_tokens`.
/// - `stream_options: { include_usage: true }` requests a final usage chunk.
/// - `parallel_tool_calls: true` when tools are present.
fn build_openai_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    let messages: Vec<serde_json::Value> = req.messages
        .iter()
        .map(|msg| {
            let content_vec: Vec<serde_json::Value> = msg.parts.iter().map(|part| match part {
                ContentPart::Text { text } => json!({"type": "text", "text": text}),
                ContentPart::ImageUrl { url, detail } => json!({
                    "type": "image_url",
                    "image_url": { "url": url, "detail": format!("{:?}", detail).to_lowercase() }
                }),
                ContentPart::ImageB64 { b64_json, detail } => json!({
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

// ── SSE parsing ───────────────────────────────────────────────────────────────

/// Parse a single SSE line from the OpenAI Chat Completions streaming API.
///
/// OpenAI-specific handling:
/// - `finish_reason` can be `"stop"` after tool calls were emitted in earlier
///   chunks; `saw_tool_call` tracks this and overrides the reason.
/// - `finish_reason` can be `"function_call"` (deprecated).
/// - `delta.tool_calls` takes priority over `delta.content` when both are present.
/// - `delta.reasoning_content` carries o-series chain-of-thought.
fn parse_openai_sse(line: &str, saw_tool_call: &mut bool) -> Option<Vec<StreamEvent>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') { return None; }
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" { return None; }

    #[derive(serde::Deserialize)]
    struct Chunk { choices: Vec<Choice>, #[serde(default)] usage: Option<Usage> }
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
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct Usage {
        #[serde(default)] prompt_tokens: Option<u64>,
        #[serde(default)] completion_tokens: Option<u64>,
        #[serde(default)] total_tokens: Option<u64>,
        #[serde(default)] prompt_tokens_details: Option<PromptDetails>,
        #[serde(default)] completion_tokens_details: Option<CompletionDetails>,
    }
    #[derive(serde::Deserialize)]
    struct PromptDetails { #[serde(default)] cached_tokens: Option<u64> }
    #[derive(serde::Deserialize)]
    struct CompletionDetails { #[serde(default)] reasoning_tokens: Option<u64> }

    let chunk: Chunk = serde_json::from_str(data).ok()?;

    // Extract usage whenever present — some providers (e.g., GLM/Zhipu) send
    // usage in the final chunk alongside choices (with finish_reason), unlike
    // standard OpenAI which sends it in a separate choices=[] chunk.
    let mut events: Vec<StreamEvent> = Vec::new();
    if let Some(usage) = chunk.usage {
        // OpenAI's prompt_tokens = total input (cached + non-cached).
        // Normalize to non-cached only so token_tracker semantics match Anthropic:
        //   input_tokens = non-cached,  cached_input_tokens = cached portion.
        let cached = usage.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens).unwrap_or(0);
        let non_cached = usage.prompt_tokens.map(|t| t.saturating_sub(cached));
        let reasoning = usage.completion_tokens_details.and_then(|d| d.reasoning_tokens);
        events.push(StreamEvent::Usage(crate::providers::ChatUsage {
            input_tokens: non_cached,
            output_tokens: usage.completion_tokens,
            cached_input_tokens: if cached > 0 { Some(cached) } else { None },
            reasoning_tokens: reasoning,
            ..Default::default()
        }));
    }

    if chunk.choices.is_empty() {
        return if events.is_empty() { None } else { Some(events) };
    }

    for choice in &chunk.choices {
        // Tool calls take priority — when both content and tool_calls are
        // present in the same chunk, the content is usually a text
        // representation of the tool call and must be ignored.
        if let Some(tcs) = &choice.delta.tool_calls {
            *saw_tool_call = true;
            if let Some(tc) = tcs.first() {
                let id = tc.id.clone().unwrap_or_default();
                let func = tc.function.as_ref();

                if !id.is_empty() && func.is_some_and(|f| f.name.is_some()) {
                    let initial_args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    events.push(StreamEvent::ToolCallStart {
                        id: id.clone(),
                        name: func.and_then(|f| f.name.clone()).unwrap_or_default(),
                        initial_arguments: initial_args,
                    });
                    return Some(events);
                }

                let args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                if !args.is_empty() {
                    events.push(StreamEvent::ToolCallDelta { id, delta: args });
                    return Some(events);
                }
            }
        }

        if let Some(text) = &choice.delta.content {
            if !text.is_empty() {
                events.push(StreamEvent::Delta { text: text.clone() });
                return Some(events);
            }
        }
        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() {
                events.push(StreamEvent::Thinking { text: reasoning.clone() });
                return Some(events);
            }
        }
        if choice.finish_reason.is_some() {
            let raw = choice.finish_reason.as_ref().unwrap();
            let reason = match raw.as_str() {
                "stop" if *saw_tool_call => StopReason::ToolUse,
                "stop" => StopReason::EndTurn,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                _ => StopReason::EndTurn,
            };
            events.push(StreamEvent::Done { reason });
        }
    }

    if events.is_empty() { None } else { Some(events) }
}

// ── ImageGenerationProvider ────────────────────────────────────────────────────

#[async_trait]
impl ImageGenerationProvider for OpenAiProvider {
    fn generate_image(&self, req: ImageRequest) -> anyhow::Result<ImageResponse> {
        let url = self.images_url();
        let headers = self.common_headers();

        let body = serde_json::json!({
            "model": req.model,
            "prompt": req.prompt,
            "n": req.n.unwrap_or(1),
            "size": match req.size {
                Some(crate::providers::image::ImageSize::Square1024) => "1024x1024",
                Some(crate::providers::image::ImageSize::Landscape1792) => "1792x1024",
                Some(crate::providers::image::ImageSize::Portrait1024) => "1024x1792",
                None => "1024x1024",
            },
            "quality": match req.quality {
                Some(crate::providers::image::ImageQuality::HD) => "hd",
                Some(crate::providers::image::ImageQuality::Standard) | None => "standard",
            },
            "response_format": match req.response_format {
                Some(ImageFormat::Url) | None => "url",
                Some(ImageFormat::B64Json) => "b64_json",
            },
        });

        let text = futures::executor::block_on(async move {
            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let resp = resp.error_for_status()?;
            resp.text().await
        })?;

        #[derive(serde::Deserialize)]
        struct ImgResp { data: Vec<ImgData> }
        #[derive(serde::Deserialize)]
        struct ImgData { url: Option<String>, b64_json: Option<String>, revised_prompt: Option<String> }

        let resp: ImgResp = serde_json::from_str(&text)?;
        let images = resp.data.into_iter().map(|d| ImageOutput {
            url: d.url,
            b64_json: d.b64_json,
            revised_prompt: d.revised_prompt,
        }).collect();

        Ok(ImageResponse { images, usage: None })
    }
}

// ── TtsProvider ──────────────────────────────────────────────────────────────

#[async_trait]
impl TtsProvider for OpenAiProvider {
    fn synthesize(&self, req: TtsRequest) -> anyhow::Result<crate::providers::tts::AudioResponse> {
        let url = self.tts_url();
        let auth = self.auth();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
        headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
            "voice": match &req.voice {
                TtsVoice::Id(id) => serde_json::json!({"type": "tts-1", "voice": id})
            },
            "response_format": match req.response_format {
                Some(TtsFormat::Mp3) | None => "mp3",
                Some(TtsFormat::Opus) => "opus",
                Some(TtsFormat::Flac) => "flac",
                Some(TtsFormat::Wav) => "wav",
            },
            "speed": req.speed.unwrap_or(1.0),
        });

        let bytes = futures::executor::block_on(async move {
            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            resp.bytes().await
        })?;

        Ok(crate::providers::tts::AudioResponse {
            audio: crate::providers::tts::AudioData {
                bytes: bytes.to_vec(),
                mime_type: "audio/mp3".to_string(),
            },
            usage: None,
        })
    }
}

// ── EmbeddingProvider ─────────────────────────────────────────────────────────

impl EmbeddingProvider for OpenAiProvider {
    fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse> {
        let url = self.embeddings_url();
        let headers = self.common_headers();

        let input = match &req.input {
            EmbedInput::Text(t) => serde_json::json!(vec![t.clone()]),
            EmbedInput::Texts(ts) => serde_json::json!(ts.clone()),
        };

        let mut body = serde_json::json!({ "model": req.model, "input": input });
        if let Some(dim) = req.dimensions {
            body["dimensions"] = serde_json::json!(dim);
        }

        let text = futures::executor::block_on(async move {
            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let resp = resp.error_for_status()?;
            resp.text().await
        })?;

        #[derive(serde::Deserialize)]
        struct Er { data: Vec<Ed>, usage: Option<Eu> }
        #[derive(serde::Deserialize)]
        struct Ed { embedding: Vec<f32> }
        #[derive(serde::Deserialize)]
        struct Eu { prompt_tokens: u64 }

        let resp: Er = serde_json::from_str(&text)?;
        let usage = resp.usage.map(|u| crate::providers::EmbeddingUsage { prompt_tokens: u.prompt_tokens });
        let embeddings = resp.data.into_iter().flat_map(|d| d.embedding).collect();

        Ok(EmbedResponse { embeddings, usage, model: req.model })
    }
}
