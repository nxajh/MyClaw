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
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct TcDelta { index: u32, id: Option<String>, function: Option<FuncDelta> }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FuncDelta { name: Option<String>, arguments: Option<String> }

    let chunk: Chunk = serde_json::from_str(data).ok()?;

    for choice in &chunk.choices {
        if let Some(text) = &choice.delta.content {
            if !text.is_empty() { return Some(StreamEvent::Delta { text: text.clone() }); }
        }
        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() { return Some(StreamEvent::Thinking { text: reasoning.clone() }); }
        }
        if let Some(tcs) = &choice.delta.tool_calls {
            if let Some(tc) = tcs.first() {
                let id = tc.id.clone().unwrap_or_default();
                let args = tc.function.as_ref().and_then(|f| f.arguments.clone()).unwrap_or_default();
                return Some(StreamEvent::ToolCallDelta { id, delta: args });
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
                content_vec.into_iter().next().unwrap()
            } else {
                serde_json::json!(content_vec)
            };

            serde_json::json!({ "role": msg.role, "content": content })
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