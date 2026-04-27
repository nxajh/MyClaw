//! Shared utilities for providers: auth, SSE parsing, body building, factory.

use base64::Engine;
use sha2::Digest;
use capability::chat::{ChatRequest, ContentPart, StreamEvent, StopReason};

// ── Auth ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AuthStyle {
    Bearer,
    XApiKey,
    ZhipuJwt,
}

pub fn build_auth(auth: &AuthStyle, credential: &str) -> String {
    match auth {
        AuthStyle::Bearer => format!("Bearer {}", credential),
        AuthStyle::XApiKey => credential.to_string(),
        AuthStyle::ZhipuJwt => generate_zhipu_jwt(credential)
            .unwrap_or_else(|_| format!("Bearer {}", credential)),
    }
}

fn generate_zhipu_jwt(credential: &str) -> Result<String, String> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    let (id, secret) = credential
        .split_once('.')
        .ok_or_else(|| "GLM API key must be in 'id.secret' format".to_string())?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;
    let exp_ms = now_ms + 210_000; // 3.5 minutes

    #[derive(serde::Serialize)]
    struct JwtHeader { alg: String, typ: String }
    #[derive(serde::Serialize)]
    struct JwtPayload { api_key: String, exp: u64, timestamp: u64 }

    let header = JwtHeader { alg: "HS256".to_string(), typ: "JWT".to_string() };
    let payload = JwtPayload {
        api_key: id.to_string(),
        exp: exp_ms / 1000,
        timestamp: now_ms / 1000,
    };

    let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        &serde_json::to_vec(&header).map_err(|e| e.to_string())?
    );
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        &serde_json::to_vec(&payload).map_err(|e| e.to_string())?
    );
    let signing_input = format!("{}.{}", header_b64, payload_b64);

    // HMAC-SHA256: H((K ⊕ 0x5c..) || H((K ⊕ 0x36..) || data))
    let ipad: [u8; 64] = [0x36u8; 64];
    let opad: [u8; 64] = [0x5cu8; 64];
    let mut key = secret.as_bytes().to_vec();
    key.resize(64, 0);
    let mut inner_key = [0u8; 64];
    let mut outer_key = [0u8; 64];
    for i in 0..64 { inner_key[i] = key[i] ^ ipad[i]; outer_key[i] = key[i] ^ opad[i]; }

    let inner = {
        let mut h = sha2::Sha256::default();
        h.write_all(&inner_key).unwrap();
        h.write_all(signing_input.as_bytes()).unwrap();
        h.finalize()
    };
    let mut h = sha2::Sha256::default();
    h.write_all(&outer_key).unwrap();
    h.write_all(&inner).unwrap();
    let sig = h.finalize();
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);

    Ok(format!("{}.{}.{}", header_b64, payload_b64, sig_b64))
}

// ── SSE parsing ──────────────────────────────────────────────────────────────

pub fn parse_openai_sse(line: &str) -> Option<StreamEvent> {
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
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(dead_code)]
    struct TcDelta { index: u32, id: Option<String>, function: Option<FuncDelta> }
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(dead_code)]
    struct FuncDelta { name: Option<String>, arguments: Option<String> }

    let chunk: Chunk = serde_json::from_str(data).ok()?;

    for choice in &chunk.choices {
        if let Some(tcs) = &choice.delta.tool_calls {
            // Log raw tool_calls delta for debugging.
            tracing::debug!(raw_tool_calls = %serde_json::to_string(tcs).unwrap_or_default(), "SSE tool_calls delta");
        }
        if let Some(text) = &choice.delta.content {
            if !text.is_empty() { return Some(StreamEvent::Delta { text: text.clone() }); }
        }
        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() { return Some(StreamEvent::Thinking { text: reasoning.clone() }); }
        }
        if let Some(tcs) = &choice.delta.tool_calls {
            if let Some(tc) = tcs.first() {
                let id = tc.id.clone().unwrap_or_default();
                let func = tc.function.as_ref();

                // If this delta carries an id AND a name, it's the first chunk
                // for this tool call → emit ToolCallStart.
                // GLM sends id + name + arguments ALL in one chunk, so carry
                // initial_arguments along.
                if !id.is_empty() && func.is_some_and(|f| f.name.is_some()) {
                    let initial_args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    return Some(StreamEvent::ToolCallStart {
                        id: id.clone(),
                        name: func.and_then(|f| f.name.clone()).unwrap_or_default(),
                        initial_arguments: initial_args,
                    });
                }

                // Subsequent deltas carry argument fragments.
                let args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                if !args.is_empty() {
                    return Some(StreamEvent::ToolCallDelta { id, delta: args });
                }
            }
        }
        if choice.finish_reason.is_some() {
            let reason = choice.finish_reason.as_ref().and_then(|r| match r.as_str() {
                "stop" => Some(StopReason::EndTurn),
                "length" => Some(StopReason::MaxTokens),
                "content_filter" => Some(StopReason::ContentFilter),
                "tool_calls" => Some(StopReason::ToolUse),
                _ => None,
            }).unwrap_or(StopReason::EndTurn);
            return Some(StreamEvent::Done { reason });
        }
    }

    None
}

// ── Body building ─────────────────────────────────────────────────────────────

pub fn build_openai_chat_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    let messages: Vec<serde_json::Value> = req.messages
        .iter()
        .map(|msg| {
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
            }).collect();

            let content = if content_vec.len() == 1 {
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
                    // Backward compat: name field used as tool_call_id.
                    msg_json["tool_call_id"] = serde_json::json!(n);
                }
                msg_json["content"] = serde_json::json!(content);
            } else if msg.role == "assistant" {
                // Assistant message may carry tool_calls from a previous turn.
                msg_json["content"] = if content.is_string() && content.as_str().unwrap_or("").is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(content)
                };
                if let Some(tcs) = &msg.tool_calls {
                    msg_json["tool_calls"] = serde_json::json!(tcs);
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
    });

    if let Some(temp) = req.temperature { body["temperature"] = serde_json::json!(temp); }
    if let Some(max) = req.max_tokens { body["max_tokens"] = serde_json::json!(max); }
    if let Some(stop) = &req.stop { body["stop"] = serde_json::json!(stop); }
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

// ── Factory ───────────────────────────────────────────────────────────────────

pub fn create_provider(name: &str, api_key: String) -> Option<Box<dyn ProviderInstance>> {
    match name {
        "openai" => Some(Box::new(crate::openai::OpenAiProvider::new(api_key)) as _),
        "anthropic" => Some(Box::new(crate::anthropic::AnthropicProvider::new(api_key)) as _),
        "glm" => Some(Box::new(crate::glm::GlmProvider::new(api_key)) as _),
        "kimi" => Some(Box::new(crate::kimi::KimiProvider::new(api_key)) as _),
        "minimax" => Some(Box::new(crate::minimax::MiniMaxProvider::new(api_key)) as _),
        _ => None,
    }
}

pub trait ProviderInstance: Send + Sync {}

impl ProviderInstance for crate::openai::OpenAiProvider {}
impl ProviderInstance for crate::anthropic::AnthropicProvider {}
impl ProviderInstance for crate::glm::GlmProvider {}
impl ProviderInstance for crate::kimi::KimiProvider {}
impl ProviderInstance for crate::minimax::MiniMaxProvider {}

/// Create a provider by inspecting the base_url hostname.
/// Falls back to OpenAI-compatible if no specific match is found.
pub fn create_provider_by_url(
    api_key: String,
    base_url: &str,
) -> Option<Box<dyn capability::chat::ChatProvider>> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/').next()
        .unwrap_or("");

    tracing::info!(base_url, host, "auto-detecting provider type from base_url");

    if host.contains("bigmodel.cn") || host.contains("zhipuai") {
        Some(Box::new(crate::glm::GlmProvider::with_base_url(api_key, base_url.to_string())))
    } else if host.contains("anthropic.com") || host.contains("claude.ai") {
        Some(Box::new(crate::anthropic::AnthropicProvider::with_base_url(api_key, base_url.to_string())))
    } else if host.contains("minimax") {
        Some(Box::new(crate::minimax::MiniMaxProvider::with_base_url(api_key, base_url.to_string())))
    } else if host.contains("moonshot") || host.contains("kimi") {
        Some(Box::new(crate::kimi::KimiProvider::with_base_url(api_key, base_url.to_string())))
    } else {
        // Default: OpenAI-compatible (covers api.openai.com, api.deepseek.com, etc.)
        tracing::info!(host, "no specific match, using OpenAI-compatible provider");
        Some(Box::new(crate::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string())))
    }
}

/// Create a full OpenAI provider (Chat + Embedding + Image + TTS) by URL.
/// Only succeeds for providers that implement all capabilities via the OpenAI protocol.
/// Returns `None` for non-OpenAI providers (Anthropic, etc.).
pub fn create_full_openai_provider(
    api_key: String,
    base_url: &str,
) -> Option<crate::openai::OpenAiProvider> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/').next()
        .unwrap_or("");

    // Only create full provider for OpenAI-compatible endpoints
    // Non-OpenAI providers (Anthropic) don't implement Embedding/Image/TTS
    if host.contains("anthropic.com") || host.contains("claude.ai")
        || host.contains("bigmodel.cn") || host.contains("zhipuai")
        || host.contains("minimax") || host.contains("moonshot") || host.contains("kimi")
    {
        return None;
    }

    Some(crate::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string()))
}

/// Capability-aware provider creation result.
/// Holds the concrete provider type and lets the caller extract trait objects.
pub enum ProviderHandle {
    OpenAi(crate::openai::OpenAiProvider),
    Glm(crate::glm::GlmProvider),
    Kimi(crate::kimi::KimiProvider),
    MiniMax(crate::minimax::MiniMaxProvider),
    Anthropic(crate::anthropic::AnthropicProvider),
}

impl ProviderHandle {
    /// Create a ProviderHandle by inspecting the base_url hostname.
    pub fn from_url(api_key: String, base_url: &str) -> Option<Self> {
        let host = base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/').next()
            .unwrap_or("");

        if host.contains("bigmodel.cn") || host.contains("zhipuai") {
            Some(ProviderHandle::Glm(
                crate::glm::GlmProvider::with_base_url(api_key, base_url.to_string())
            ))
        } else if host.contains("anthropic.com") || host.contains("claude.ai") {
            Some(ProviderHandle::Anthropic(
                crate::anthropic::AnthropicProvider::with_base_url(api_key, base_url.to_string())
            ))
        } else if host.contains("minimax") {
            Some(ProviderHandle::MiniMax(
                crate::minimax::MiniMaxProvider::with_base_url(api_key, base_url.to_string())
            ))
        } else if host.contains("moonshot") || host.contains("kimi") {
            Some(ProviderHandle::Kimi(
                crate::kimi::KimiProvider::with_base_url(api_key, base_url.to_string())
            ))
        } else {
            Some(ProviderHandle::OpenAi(
                crate::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string())
            ))
        }
    }

    /// Return a boxed ChatProvider.
    pub fn into_chat_provider(self) -> Box<dyn capability::chat::ChatProvider> {
        match self {
            ProviderHandle::OpenAi(p) => Box::new(p),
            ProviderHandle::Glm(p) => Box::new(p),
            ProviderHandle::Kimi(p) => Box::new(p),
            ProviderHandle::MiniMax(p) => Box::new(p),
            ProviderHandle::Anthropic(p) => Box::new(p),
        }
    }

    /// Return a boxed EmbeddingProvider, if this provider supports it.
    pub fn into_embedding_provider(self) -> Option<Box<dyn capability::embedding::EmbeddingProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            ProviderHandle::Glm(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed ImageGenerationProvider, if this provider supports it.
    pub fn into_image_provider(self) -> Option<Box<dyn capability::image::ImageGenerationProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed TtsProvider, if this provider supports it.
    pub fn into_tts_provider(self) -> Option<Box<dyn capability::tts::TtsProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed VideoGenerationProvider, if this provider supports it.
    pub fn into_video_provider(self) -> Option<Box<dyn capability::video::VideoGenerationProvider>> {
        // No provider implements VideoGeneration yet
        None
    }

    /// Return a boxed SearchProvider, if this provider supports it.
    pub fn into_search_provider(self) -> Option<Box<dyn capability::search::SearchProvider>> {
        match self {
            ProviderHandle::Glm(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    /// Return a boxed SttProvider, if this provider supports it.
    pub fn into_stt_provider(self) -> Option<Box<dyn capability::stt::SttProvider>> {
        // No provider implements STT yet
        None
    }
}