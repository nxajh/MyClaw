//! MiniMax provider — delegates to Anthropic-compatible API for chat,
//! and implements SearchProvider via MiniMax's coding plan search API.
//!
//! Chat: uses the Anthropic-compatible endpoint (`https://api.minimaxi.com/anthropic`).
//! Search: uses the coding plan search API (`/v1/coding_plan/search`).

use async_trait::async_trait;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::search::{SearchProvider, SearchRequest, SearchResult, SearchResults};
use crate::providers::{BoxStream, ChatProvider, ChatRequest, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/anthropic";
const SEARCH_BASE_URL: &str = "https://api.minimaxi.com";

#[derive(Clone)]
pub struct MiniMaxProvider {
    inner: AnthropicProvider,
    api_key: String,
    base_url: String,
}

impl MiniMaxProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            inner: AnthropicProvider::with_base_url(api_key.clone(), base_url.clone()),
            api_key,
            base_url,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.inner = self.inner.with_user_agent(user_agent);
        self
    }
}

#[async_trait]
impl ChatProvider for MiniMaxProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        self.inner.chat(req)
    }
}

// ── SearchProvider ────────────────────────────────────────────────────────────

/// MiniMax web search via the coding plan search API.
///
/// Endpoint: `POST {search_base}/v1/coding_plan/search`
/// Auth: Bearer token (same API key as chat)
/// Request: `{"q": "search query"}`
/// Response: `{organic: [{title, link, snippet, date}], related_searches: [{query}]}`
impl SearchProvider for MiniMaxProvider {
    fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults> {
        // Use the configured base_url but switch to the API root for search.
        // If base_url is the anthropic endpoint, use the default search base.
        let search_base = if self.base_url.contains("/anthropic") {
            SEARCH_BASE_URL.to_string()
        } else {
            self.base_url.clone()
        };

        let url = format!(
            "{}/v1/coding_plan/search",
            search_base.trim_end_matches('/')
        );

        let body = serde_json::json!({ "q": req.query });

        let text = futures::executor::block_on(async {
            let resp = reqwest::Client::new()
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("MiniMax search HTTP {}: {}", status, body);
            }
            resp.text().await.map_err(|e| anyhow::anyhow!(e.to_string()))
        })?;

        #[derive(serde::Deserialize)]
        struct SearchResp {
            #[serde(default)]
            organic: Vec<OrganicResult>,
            #[serde(default)]
            base_resp: Option<BaseResp>,
        }
        #[derive(serde::Deserialize)]
        struct OrganicResult {
            title: String,
            link: String,
            #[serde(default)]
            snippet: String,
            #[serde(default)]
            date: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct BaseResp {
            #[serde(default)]
            status_code: i32,
            #[serde(default)]
            status_msg: String,
        }

        let resp: SearchResp = serde_json::from_str(&text)?;

        // Check API-level error.
        if let Some(br) = &resp.base_resp {
            if br.status_code != 0 {
                anyhow::bail!(
                    "MiniMax search API error {}: {}",
                    br.status_code,
                    br.status_msg
                );
            }
        }

        let results: Vec<SearchResult> = resp
            .organic
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.link,
                snippet: r.snippet,
                published_at: r.date,
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
