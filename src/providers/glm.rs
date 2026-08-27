//! GLM (Zhipu) provider — Embedding + Search.
//!
//! Chat is handled by the generic `OpenAiChatCompletionsClient` via
//! `ProviderFactory`.  `glm_body_override` adds GLM-specific thinking
//! parameters and reasoning_content echo to the rendered body.
//!
//! Reference: https://docs.bigmodel.cn/api-reference/模型-api/对话补全.md
//!
//! Endpoints (relative to base_url):
//!   Embedding: {base_url}/v4/embeddings
//!   Search:    {base_url}/v4/web_search

use crate::providers::{
    ContentPart, EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider, SearchProvider,
    SearchRequest, SearchResult, SearchResults, SharedApiKey,
};
use reqwest::Client;

const DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";

// ── Body override (used by ProviderFactory for GLM chat) ──────────────────────

/// GLM-specific body override.
///
/// 1. Echoes `reasoning_content` for **all** assistant messages (Preserved Thinking).
/// 2. Adds `thinking: {"type":"enabled","clear_thinking":false}` when reasoning is on.
pub fn glm_body_override(
    mut body: serde_json::Value,
    req: &crate::providers::ChatRequest<'_>,
) -> serde_json::Value {
    use serde_json::json;

    // Inject reasoning_content into assistant messages from Thinking parts.
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for (i, msg) in messages.iter_mut().enumerate() {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            let orig = &req.messages[i];
            let reasoning: Vec<&str> = orig
                .parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            if !reasoning.is_empty() {
                msg["reasoning_content"] = json!(reasoning.join(""));
            }
        }
    }

    // Enable thinking with Preserved Thinking when configured.
    if let Some(ref tc) = req.thinking {
        if tc.enabled {
            body["thinking"] = json!({"type": "enabled", "clear_thinking": false});
        } else {
            body["thinking"] = json!({"type": "disabled"});
        }
    }

    body
}

// ── GlmProvider (Embedding + Search) ──────────────────────────────────────────

#[derive(Clone)]
pub struct GlmProvider {
    base_url: String,
    api_key: SharedApiKey,
    client: Client,
    user_agent: Option<String>,
}

impl GlmProvider {
    pub fn new(api_key: impl Into<SharedApiKey>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: impl Into<SharedApiKey>, base_url: String) -> Self {
        Self {
            base_url,
            api_key: api_key.into(),
            client: crate::providers::infra::build_reqwest_client(),
            user_agent: None,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    fn auth(&self) -> String {
        crate::providers::infra::build_auth(
            &crate::providers::infra::AuthStyle::Bearer,
            &self.api_key.get(),
        )
    }

    fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    fn web_search_url(&self) -> String {
        format!("{}/web_search", self.base_url.trim_end_matches('/'))
    }
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

        let text = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
                headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                );
                if let Some(ref ua) = self.user_agent {
                    headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
                }

                let resp = self
                    .client
                    .post(&url)
                    .headers(headers)
                    .json(&body)
                    .send()
                    .await?;
                let resp = resp.error_for_status()?;
                resp.text().await
            })
        })?;

        #[derive(serde::Deserialize)]
        struct Er {
            data: Vec<Ed>,
            usage: Option<Eu>,
            model: String,
        }
        #[derive(serde::Deserialize)]
        struct Ed {
            embedding: Vec<f32>,
        }
        #[derive(serde::Deserialize)]
        struct Eu {
            prompt_tokens: u64,
        }

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

        let text = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
                headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                );
                if let Some(ref ua) = self.user_agent {
                    headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
                }

                let resp = self
                    .client
                    .post(&url)
                    .headers(headers)
                    .json(&body)
                    .send()
                    .await?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    anyhow::bail!("GLM web_search HTTP {}: {}", status, body);
                }
                resp.text()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
            })
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
