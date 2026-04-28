//! GLM (Zhipu/GLM-4) provider — Chat + Embedding + Search.
//!
//! All endpoints are relative to base_url (which already contains /v4 or similar version prefix).
//! Chat:      {base_url}/chat/completions
//! Embedding: {base_url}/embeddings
//! Search:    {base_url}/web_search

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::Client;
use crate::shared::{parse_openai_sse, build_openai_chat_body};
use myclaw_capability::chat::{BoxStream, ChatProvider, ChatRequest, StreamEvent, StopReason};
use myclaw_capability::embedding::{EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider};
use myclaw_capability::search::{SearchProvider, SearchRequest, SearchResult, SearchResults};

const DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";

#[derive(Clone)]
pub struct GlmProvider {
    base_url: String,
    api_key: String,
    client: Client,
}

impl GlmProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, client: Client::new() }
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
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
        let body = build_openai_chat_body(&req);
        let auth = self.auth();
        let client = self.client.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

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
                    if let Some(event) = parse_openai_sse(&line) {
                        let _ = tx.send(event).await;
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done { reason: StopReason::EndTurn }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

// ── EmbeddingProvider ─────────────────────────────────────────────────────────
//
// GLM Embedding API (OpenAI-compatible):
//   POST /v4/embeddings
//   { "model": "embedding-3", "input": "text" | ["t1", "t2"], "dimensions": 2048 }
//
// Response:
//   { "data": [{ "embedding": [0.1, ...], "index": 0, "object": "embedding" }],
//     "usage": { "prompt_tokens": N, "total_tokens": N },
//     "model": "embedding-3" }

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

            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let resp = resp.error_for_status()?;
            resp.text().await
        })?;

        #[derive(serde::Deserialize)]
        struct Er {
            data: Vec<Ed>,
            usage: Option<Eu>,
            model: String,
        }
        #[derive(serde::Deserialize)]
        struct Ed { embedding: Vec<f32> }
        #[derive(serde::Deserialize)]
        struct Eu { prompt_tokens: u64 }

        let resp: Er = serde_json::from_str(&text)?;
        let usage = resp.usage.map(|u| myclaw_capability::embedding::EmbeddingUsage {
            prompt_tokens: u.prompt_tokens,
        });

        // Flatten all embedding vectors. For single input, there's one vector.
        // For batch input, concatenate (matching existing capability API).
        let embeddings: Vec<f32> = resp.data.into_iter().flat_map(|d| d.embedding).collect();

        Ok(EmbedResponse {
            embeddings,
            usage,
            model: resp.model,
        })
    }
}

// ── SearchProvider ────────────────────────────────────────────────────────────
//
// GLM Web Search API:
//   POST /v4/web_search
//   { "search_query": "query", "search_engine": "search_std", "count": 10 }
//
// Response:
//   { "search_result": [{ "title": "", "content": "", "link": "", "media": "",
//                        "icon": "", "refer": "", "publish_date": "" }],
//     "search_intent": [...], "id": "", "created": N, "request_id": "" }

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

            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("GLM web_search HTTP {}: {}", status, body);
            }
            resp.text().await.map_err(|e| anyhow::anyhow!(e.to_string()))
        })?;

        #[derive(serde::Deserialize)]
        struct SearchResp {
            #[serde(default)]
            search_result: Vec<Sr>,
        }
        #[derive(serde::Deserialize)]
        struct Sr {
            title: String,
            content: String,
            link: String,
            #[allow(dead_code)]
            media: String,
            #[serde(default)]
            publish_date: Option<String>,
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
