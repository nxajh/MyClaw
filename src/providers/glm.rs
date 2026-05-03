//! GLM (Zhipu) provider — Chat + Embedding + Search.
//!
//! Reference: https://docs.bigmodel.cn/api-reference/模型-api/对话补全.md
//!
//! Endpoints (relative to base_url):
//!   Chat:      {base_url}/chat/completions
//!   Embedding: {base_url}/embeddings
//!   Search:    {base_url}/web_search
//!
//! GLM-specific behaviours handled here:
//! - `do_sample` is required for non-greedy decoding
//! - `tool_stream: true` enables streaming tool-call deltas (otherwise the
//!   entire tool call is returned in a single chunk with finish_reason="stop")
//! - finish_reason can be "sensitive" (content filtered by GLM's safety)
//! - finish_reason may be "stop" even when tool_calls were emitted; we track
//!   `saw_tool_call` and override to ToolUse in that case

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::Client;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, ContentPart, StreamEvent, StopReason};
use crate::providers::{EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider};
use crate::providers::{SearchProvider, SearchRequest, SearchResult, SearchResults};

const DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";

#[derive(Clone)]
pub struct GlmProvider {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl GlmProvider {
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
        // If api_key contains '.', treat it as id.secret format and generate JWT.
        if self.api_key.contains('.') {
            generate_zhipu_jwt(&self.api_key)
                .unwrap_or_else(|_| format!("Bearer {}", self.api_key))
        } else {
            format!("Bearer {}", self.api_key)
        }
    }

    fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    fn web_search_url(&self) -> String {
        format!("{}/web_search", self.base_url.trim_end_matches('/'))
    }
}

// ── ChatProvider ───────────────────────────────────────────────────────────────

#[async_trait]
impl ChatProvider for GlmProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_glm_body(&req);
        let body_str = serde_json::to_string_pretty(&body).unwrap_or_default();
        crate::providers::append_to_debug_log(&format!(
            "=== REQUEST ===\nURL: {}\nBody:\n{}\n",
            url, body_str
        ));
        let auth = self.auth();
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
                crate::providers::append_to_debug_log(&format!(
                    "=== HTTP ERROR ===\nURL: {}\nStatus: {}\nBody: {}\n",
                    url, status, text
                ));
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
            crate::providers::append_to_debug_log(&format!("=== SSE STREAM START ===\nURL: {}\n", url));

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
                    let event = parse_glm_sse(&line, &mut saw_tool_call);
                    crate::providers::append_to_debug_log(&format!(
                        "SSE LINE: {}\nEVENT: {:?}\n",
                        line, event
                    ));
                    if let Some(event) = event {
                        let _ = tx.send(event).await;
                    }
                }
            }
            crate::providers::append_to_debug_log(&format!("=== SSE STREAM END ===\nURL: {}\n\n", url));
            // GLM may report finish_reason="stop" even when tool calls were present.
            let final_reason = if saw_tool_call { StopReason::ToolUse } else { StopReason::EndTurn };
            let _ = tx.send(StreamEvent::Done { reason: final_reason }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

// ── Body building ─────────────────────────────────────────────────────────────

/// Build a request body tailored to GLM's API.
///
/// Differences from the generic OpenAI body:
/// - `do_sample: true` is added when `temperature > 0` so GLM actually
///   samples (without it, temperature is silently ignored).
/// - `tool_stream: true` when tools are present so that tool calls arrive
///   as incremental SSE deltas instead of a single blob at stream end.
fn build_glm_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value {
    use serde_json::json;

    let messages: Vec<serde_json::Value> = req.messages
        .iter()
        .map(|msg| {
            let content_vec: Vec<serde_json::Value> = msg.parts.iter().filter_map(|part| match part {
                ContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
                ContentPart::ImageUrl { url, detail } => Some(json!({
                    "type": "image_url",
                    "image_url": { "url": url, "detail": format!("{:?}", detail).to_lowercase() }
                })),
                ContentPart::ImageB64 { b64_json, detail } => Some(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:image;base64,{}", b64_json), "detail": format!("{:?}", detail).to_lowercase() }
                })),
                ContentPart::Thinking { .. } => {
                    // GLM does not support thinking blocks — skip entirely.
                    None
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
            } else if content_vec.is_empty() {
                json!("")
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
    });

    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
        // GLM requires do_sample=true for non-greedy decoding.
        body["do_sample"] = json!(true);
    }
    if let Some(max) = req.max_tokens {
        body["max_tokens"] = json!(max);
    }
    if let Some(stop) = &req.stop {
        body["stop"] = json!(stop);
    }
    if let Some(tools) = req.tools {
        body["tools"] = json!(tools.iter().map(|t| {
            json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.input_schema }
            })
        }).collect::<Vec<_>>());
        // Request incremental tool-call deltas so we can stream them
        // instead of receiving the entire call in one chunk.
        body["tool_stream"] = json!(true);
    }

    body
}

// ── SSE parsing ───────────────────────────────────────────────────────────────

/// Parse a single SSE line from GLM.
///
/// GLM-specific handling:
/// - `finish_reason` can be `"sensitive"` (content filtered by GLM safety)
/// - `finish_reason` may be `"stop"` even when `tool_calls` were emitted
///   in previous chunks; `saw_tool_call` tracks this and overrides the reason
fn parse_glm_sse(line: &str, saw_tool_call: &mut bool) -> Option<StreamEvent> {
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
        // Tool calls take priority — GLM occasionally sends both content
        // and tool_calls in the same chunk.  When that happens the content
        // is usually a text representation of the tool call and must be
        // ignored in favour of the structured tool_calls field.
        if let Some(tcs) = &choice.delta.tool_calls {
            *saw_tool_call = true;
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

        if let Some(text) = &choice.delta.content {
            if !text.is_empty() { return Some(StreamEvent::Delta { text: text.clone() }); }
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
                "content_filter" | "sensitive" => StopReason::ContentFilter,
                _ => StopReason::EndTurn,
            };
            return Some(StreamEvent::Done { reason });
        }
    }

    None
}

// ── EmbeddingProvider ─────────────────────────────────────────────────────────

impl EmbeddingProvider for GlmProvider {
    fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse> {
        let url = self.embeddings_url();
        let auth = self.auth();

        let input = match &req.input {
            EmbedInput::Text(t) => serde_json::json!(vec![t.clone()]),
            EmbedInput::Texts(ts) => serde_json::json!(ts.clone()),
        };

        let mut body = serde_json::json!({ "model": req.model, "input": input });
        if let Some(dim) = req.dimensions {
            body["dimensions"] = serde_json::json!(dim);
        }

        let text = futures::executor::block_on(async {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
            if let Some(ref ua) = self.user_agent {
                headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
            }

            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let resp = resp.error_for_status()?;
            resp.text().await
        })?;

        #[derive(serde::Deserialize)]
        struct Er { data: Vec<Ed>, usage: Option<Eu>, model: String }
        #[derive(serde::Deserialize)]
        struct Ed { embedding: Vec<f32> }
        #[derive(serde::Deserialize)]
        struct Eu { prompt_tokens: u64 }

        let resp: Er = serde_json::from_str(&text)?;
        let usage = resp.usage.map(|u| crate::providers::EmbeddingUsage {
            prompt_tokens: u.prompt_tokens,
        });

        let embeddings: Vec<f32> = resp.data.into_iter().flat_map(|d| d.embedding).collect();

        Ok(EmbedResponse {
            embeddings,
            usage,
            model: resp.model,
        })
    }
}

// ── SearchProvider ────────────────────────────────────────────────────────────

impl SearchProvider for GlmProvider {
    fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults> {
        let url = self.web_search_url();
        let auth = self.auth();

        let limit = req.limit.unwrap_or(10).min(50);

        let body = serde_json::json!({
            "search_query": req.query,
            "search_engine": "search_std",
            "search_intent": false,
            "count": limit,
        });

        let text = futures::executor::block_on(async {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
            if let Some(ref ua) = self.user_agent {
                headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
            }

            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("GLM web_search HTTP {}: {}", status, body);
            }
            resp.text().await.map_err(|e| anyhow::anyhow!(e.to_string()))
        })?;

        #[derive(serde::Deserialize)]
        struct SearchResp { #[serde(default)] search_result: Vec<Sr> }
        #[derive(serde::Deserialize)]
        struct Sr {
            title: String,
            content: String,
            link: String,
            #[allow(dead_code)] media: String,
            #[serde(default)] publish_date: Option<String>,
        }

        let resp: SearchResp = serde_json::from_str(&text)?;

        let results: Vec<SearchResult> = resp
            .search_result
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.link,
                snippet: r.content,
                published_at: r.publish_date,
            })
            .collect();

        let total = Some(results.len() as u64);

        Ok(SearchResults {
            results,
            total,
            query: req.query,
        })
    }
}

// ── JWT generation (Zhipu / GLM) ─────────────────────────────────────────────

/// Generate a JWT for Zhipu/GLM authentication from an `id.secret` API key.
fn generate_zhipu_jwt(credential: &str) -> Result<String, String> {
    use base64::Engine;
    use sha2::Digest;
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
