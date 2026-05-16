//! Shared utilities for providers: auth, SSE parsing, body building, factory.

use crate::providers::{ChatRequest, ChatUsage, ContentPart, StreamEvent, StopReason};

// ── Auth ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AuthStyle {
    Bearer,
    XApiKey,
}

pub fn build_auth(auth: &AuthStyle, credential: &str) -> String {
    match auth {
        AuthStyle::Bearer => format!("Bearer {}", credential),
        AuthStyle::XApiKey => credential.to_string(),
    }
}

// ── SSE parsing ──────────────────────────────────────────────────────────────

pub fn parse_openai_sse(line: &str) -> Vec<StreamEvent> {
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

    // Usage-only chunk: choices is empty but usage is present (stream_options.include_usage).
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
        // Check tool_calls FIRST — some providers (GLM) occasionally send both
        // content and tool_calls in the same chunk.  When that happens the
        // content is usually a text representation of the tool call and must
        // be ignored in favour of the structured tool_calls field.
        let mut emitted_tool_event = false;
        if let Some(tcs) = &choice.delta.tool_calls {
            tracing::debug!(raw_tool_calls = %serde_json::to_string(tcs).unwrap_or_default(), "SSE tool_calls delta");

            for tc in tcs {
                let id = tc.id.clone().unwrap_or_default();
                let func = tc.function.as_ref();

                // If this delta carries an id AND a name, it's the first chunk
                // for this tool call → emit ToolCallStart.
                // GLM sends id + name + arguments ALL in one chunk, so carry
                // initial_arguments along.
                if !id.is_empty() && func.is_some_and(|f| f.name.is_some()) {
                    let initial_args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    events.push(StreamEvent::ToolCallStart {
                        id,
                        name: func.and_then(|f| f.name.clone()).unwrap_or_default(),
                        initial_arguments: initial_args,
                    });
                    emitted_tool_event = true;
                } else {
                    // Subsequent deltas carry argument fragments.
                    // Use index as fallback id for parallel tool calls where id is absent.
                    let args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    if !args.is_empty() {
                        let delta_id = if id.is_empty() { format!("#{}", tc.index) } else { id };
                        events.push(StreamEvent::ToolCallDelta { id: delta_id, delta: args });
                        emitted_tool_event = true;
                    }
                }
            }
        }

        // Skip content when tool_calls were present in this choice (avoids emitting
        // the text shadow some providers include alongside structured tool_calls).
        if !emitted_tool_event {
            if let Some(text) = &choice.delta.content {
                if !text.is_empty() { events.push(StreamEvent::Delta { text: text.clone() }); }
            }
            if let Some(reasoning) = &choice.delta.reasoning_content {
                if !reasoning.is_empty() { events.push(StreamEvent::Thinking { text: reasoning.clone() }); }
            }
        }

        // finish_reason can appear in the same chunk as content or tool_calls.
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

// ── Body building ─────────────────────────────────────────────────────────────

fn detect_image_media_type_openai(b64: &str) -> &'static str {
    if b64.starts_with("/9j/")       { "image/jpeg" }
    else if b64.starts_with("iVBOR") { "image/png"  }
    else if b64.starts_with("R0lG")  { "image/gif"  }
    else if b64.starts_with("UklG")  { "image/webp" }
    else                              { "image/jpeg" }
}

pub fn build_openai_chat_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    let messages: Vec<serde_json::Value> = req.messages
        .iter()
        .map(|msg| {
            // Thinking blocks are not supported by OpenAI-compatible APIs — skip entirely.
            let content_vec: Vec<serde_json::Value> = msg.parts.iter().filter_map(|part| match part {
                ContentPart::Text { text } => Some(serde_json::json!({"type": "text", "text": text})),
                ContentPart::ImageUrl { url, detail } => Some(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": url, "detail": format!("{:?}", detail).to_lowercase() }
                })),
                ContentPart::ImageB64 { b64_json, detail, media_type } => {
                    let mime = media_type.as_deref()
                        .unwrap_or_else(|| detect_image_media_type_openai(b64_json));
                    Some(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", mime, b64_json),
                            "detail": format!("{:?}", detail).to_lowercase()
                        }
                    }))
                }
                ContentPart::Thinking { .. } => None,
            }).collect();

            let content = if content_vec.is_empty() {
                // All parts were filtered (e.g. only Thinking parts) — empty content.
                serde_json::json!("")
            } else if content_vec.len() == 1 {
                // For a single text part, emit plain string content for maximum compatibility.
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

            let mut msg_json = serde_json::json!({ "role": msg.role });

            // Handle "tool" role: must include tool_call_id.
            if msg.role == "tool" {
                if let Some(tc_id) = &msg.tool_call_id {
                    msg_json["tool_call_id"] = serde_json::json!(tc_id);
                } else if let Some(n) = &msg.name {
                    msg_json["tool_call_id"] = serde_json::json!(n);
                }
                msg_json["content"] = serde_json::json!(content);
            } else if msg.role == "assistant" {
                let is_empty = match &content {
                    serde_json::Value::String(s) => s.is_empty(),
                    serde_json::Value::Array(arr) => arr.is_empty(),
                    _ => false,
                };
                msg_json["content"] = if is_empty {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(content)
                };
                if let Some(tcs) = &msg.tool_calls {
                    msg_json["tool_calls"] = serde_json::json!(tcs.iter().map(|tc| tc.to_openai()).collect::<Vec<_>>());
                }
            } else {
                msg_json["content"] = serde_json::json!(content);
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

    if let Some(temp) = req.temperature { body["temperature"] = serde_json::json!(temp); }
    // max_completion_tokens is preferred; include max_tokens for older providers.
    if let Some(max) = req.max_tokens {
        body["max_completion_tokens"] = serde_json::json!(max);
        body["max_tokens"] = serde_json::json!(max);
    }
    if let Some(stop) = &req.stop { body["stop"] = serde_json::json!(stop); }
    if let Some(seed) = req.seed { body["seed"] = serde_json::json!(seed); }
    if let Some(tools) = req.tools {
        body["tools"] = serde_json::json!(tools.iter().map(|t| {
            serde_json::json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.input_schema }
            })
        }).collect::<Vec<_>>());
        body["parallel_tool_calls"] = serde_json::json!(true);
    }

    body
}

// ── Factory ───────────────────────────────────────────────────────────────────

pub fn create_provider(name: &str, api_key: String) -> Option<Box<dyn ProviderInstance>> {
    match name {
        "openai" => Some(Box::new(crate::providers::openai::OpenAiProvider::new(api_key)) as _),
        "anthropic" => Some(Box::new(crate::providers::anthropic::AnthropicProvider::new(api_key)) as _),
        "glm" => Some(Box::new(crate::providers::glm::GlmProvider::new(api_key)) as _),
        "kimi" => Some(Box::new(crate::providers::kimi::KimiProvider::new(api_key)) as _),
        "minimax" => Some(Box::new(crate::providers::minimax::MiniMaxProvider::new(api_key)) as _),
        "xiaomi" | "mimo" => Some(Box::new(crate::providers::xiaomi::XiaomiProvider::new(api_key)) as _),
        _ => None,
    }
}

pub trait ProviderInstance: Send + Sync {}

impl ProviderInstance for crate::providers::openai::OpenAiProvider {}
impl ProviderInstance for crate::providers::anthropic::AnthropicProvider {}
impl ProviderInstance for crate::providers::glm::GlmProvider {}
impl ProviderInstance for crate::providers::kimi::KimiProvider {}
impl ProviderInstance for crate::providers::minimax::MiniMaxProvider {}
impl ProviderInstance for crate::providers::xiaomi::XiaomiProvider {}

/// Create a provider by inspecting the base_url hostname.
/// Falls back to OpenAI-compatible if no specific match is found.
pub fn create_provider_by_url(
    api_key: String,
    base_url: &str,
) -> Option<Box<dyn crate::providers::ChatProvider>> {
    create_provider_by_url_with_user_agent(api_key, base_url, None)
}

/// Create a provider with optional user_agent by inspecting the base_url hostname.
pub fn create_provider_by_url_with_user_agent(
    api_key: String,
    base_url: &str,
    user_agent: Option<&str>,
) -> Option<Box<dyn crate::providers::ChatProvider>> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/').next()
        .unwrap_or("");

    tracing::info!(base_url, host, "auto-detecting provider type from base_url");

    if host.contains("bigmodel.cn") || host.contains("zhipuai") {
        let mut p = crate::providers::glm::GlmProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("xiaomimimo") {
        let mut p = crate::providers::xiaomi::XiaomiProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("anthropic.com") || host.contains("claude.ai") {
        let mut p = crate::providers::anthropic::AnthropicProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("minimax") {
        let mut p = crate::providers::minimax::MiniMaxProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("moonshot") || host.contains("kimi") {
        let mut p = crate::providers::kimi::KimiProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else {
        // Default: OpenAI-compatible (covers api.openai.com, api.deepseek.com, etc.)
        tracing::info!(host, "no specific match, using OpenAI-compatible provider");
        let mut p = crate::providers::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    }
}

/// Create a full OpenAI provider (Chat + Embedding + Image + TTS) by URL.
/// Only succeeds for providers that implement all capabilities via the OpenAI protocol.
/// Returns `None` for non-OpenAI providers (Anthropic, etc.).
pub fn create_full_openai_provider(
    api_key: String,
    base_url: &str,
) -> Option<crate::providers::openai::OpenAiProvider> {
    create_full_openai_provider_with_user_agent(api_key, base_url, None)
}

/// Create a full OpenAI provider with optional user_agent.
pub fn create_full_openai_provider_with_user_agent(
    api_key: String,
    base_url: &str,
    user_agent: Option<&str>,
) -> Option<crate::providers::openai::OpenAiProvider> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/').next()
        .unwrap_or("");

    // Only create full provider for OpenAI-compatible endpoints
    // Non-OpenAI providers (Anthropic, Xiaomi, etc.) don't implement Embedding/Image/TTS
    if host.contains("anthropic.com") || host.contains("claude.ai")
        || host.contains("bigmodel.cn") || host.contains("zhipuai")
        || host.contains("minimax") || host.contains("moonshot") || host.contains("kimi")
        || host.contains("xiaomimimo")
    {
        return None;
    }

    let mut p = crate::providers::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string());
    if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
    Some(p)
}

/// Capability-aware provider creation result.
/// Holds the concrete provider type and lets the caller extract trait objects.
pub enum ProviderHandle {
    OpenAi(crate::providers::openai::OpenAiProvider),
    Glm(crate::providers::glm::GlmProvider),
    Google(crate::providers::google::GoogleProvider),
    Kimi(crate::providers::kimi::KimiProvider),
    MiniMax(crate::providers::minimax::MiniMaxProvider),
    Anthropic(crate::providers::anthropic::AnthropicProvider),
    Xiaomi(crate::providers::xiaomi::XiaomiProvider),
}

impl ProviderHandle {
    /// Create a ProviderHandle by inspecting the base_url hostname.
    pub fn from_url(api_key: String, base_url: &str) -> Option<Self> {
        Self::from_url_with_user_agent(api_key, base_url, None)
    }

    /// Create a ProviderHandle with an optional user_agent.
    pub fn from_url_with_user_agent(api_key: String, base_url: &str, user_agent: Option<&str>) -> Option<Self> {
        let host = base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/').next()
            .unwrap_or("");

        if host.contains("bigmodel.cn") || host.contains("zhipuai") {
            let mut p = crate::providers::glm::GlmProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Glm(p))
        } else if host.contains("googleapis.com") || host.contains("google.com") {
            let p = crate::providers::google::GoogleProvider::with_base_url(api_key, base_url.to_string());
            Some(ProviderHandle::Google(p))
        } else if host.contains("xiaomimimo") {
            let mut p = crate::providers::xiaomi::XiaomiProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Xiaomi(p))
        } else if host.contains("anthropic.com") || host.contains("claude.ai") {
            let mut p = crate::providers::anthropic::AnthropicProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Anthropic(p))
        } else if host.contains("minimax") {
            let mut p = crate::providers::minimax::MiniMaxProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::MiniMax(p))
        } else if host.contains("moonshot") || host.contains("kimi") {
            let mut p = crate::providers::kimi::KimiProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Kimi(p))
        } else {
            let mut p = crate::providers::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::OpenAi(p))
        }
    }

    /// Return a boxed ChatProvider.
    pub fn into_chat_provider(self) -> Box<dyn crate::providers::ChatProvider> {
        match self {
            ProviderHandle::OpenAi(p) => Box::new(p),
            ProviderHandle::Glm(p) => Box::new(p),
            ProviderHandle::Google(_) => panic!("Google provider does not implement ChatProvider"),
            ProviderHandle::Kimi(p) => Box::new(p),
            ProviderHandle::MiniMax(p) => Box::new(p),
            ProviderHandle::Anthropic(p) => Box::new(p),
            ProviderHandle::Xiaomi(p) => Box::new(p),
        }
    }

    /// Return a boxed EmbeddingProvider, if this provider supports it.
    pub fn into_embedding_provider(self) -> Option<Box<dyn crate::providers::EmbeddingProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            ProviderHandle::Glm(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed ImageGenerationProvider, if this provider supports it.
    pub fn into_image_provider(self) -> Option<Box<dyn crate::providers::ImageGenerationProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed TtsProvider, if this provider supports it.
    pub fn into_tts_provider(self) -> Option<Box<dyn crate::providers::TtsProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed VideoGenerationProvider, if this provider supports it.
    pub fn into_video_provider(self) -> Option<Box<dyn crate::providers::VideoGenerationProvider>> {
        // No provider implements VideoGeneration yet
        None
    }

    /// Return a boxed SearchProvider, if this provider supports it.
    pub fn into_search_provider(self) -> Option<Box<dyn crate::providers::SearchProvider>> {
        match self {
            ProviderHandle::Glm(p) => Some(Box::new(p)),
            ProviderHandle::Google(p) => Some(Box::new(p)),
            ProviderHandle::MiniMax(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed SttProvider, if this provider supports it.
    pub fn into_stt_provider(self) -> Option<Box<dyn crate::providers::SttProvider>> {
        // No provider implements STT yet
        None
    }
}