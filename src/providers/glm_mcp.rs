//! GLM MCP Search Provider — wraps GLM Coding Plan's MCP search endpoint.
//!
//! GLM Coding Plan includes web search via an MCP (Streamable HTTP) server,
//! which uses the Coding Plan quota rather than a separate paid search
//! resource pack. This provider wraps that MCP endpoint as a regular
//! `SearchProvider` so it integrates transparently with MyClaw's existing
//! search routing, credential pool, and cooldown system.
//!
//! Endpoint: https://open.bigmodel.cn/api/mcp/web_search_prime/mcp
//! Tool:    web_search_prime

use crate::providers::{
    SearchProvider, SearchRequest, SearchResult, SearchResults, SharedApiKey,
};
use reqwest::Client;

pub struct GlmMcpSearchProvider {
    client: Client,
    api_key: SharedApiKey,
    url: String,
    user_agent: Option<String>,
}

impl GlmMcpSearchProvider {
    pub fn new(api_key: impl Into<SharedApiKey>, url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            url: url.into(),
            user_agent: None,
        }
    }

    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key.get())
    }
}

/// Extract the JSON payload from the last `data:` line in an SSE response.
fn parse_sse_data(text: &str) -> Option<&str> {
    let mut last_data = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            last_data = Some(rest.trim());
        }
    }
    last_data
}

impl SearchProvider for GlmMcpSearchProvider {
    fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults> {
        let limit = req.limit.unwrap_or(10).min(50);
        let auth = self.auth();
        let url = self.url.clone();
        let ua = self.user_agent.clone();
        let query = req.query.clone();

        let text = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // ── 1. Initialize ───────────────────────────────────────────
                let mut init_headers = reqwest::header::HeaderMap::new();
                init_headers.insert(
                    reqwest::header::AUTHORIZATION,
                    auth.parse().unwrap(),
                );
                init_headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                );
                init_headers.insert(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream".parse().unwrap(),
                );
                if let Some(ref ua) = ua {
                    init_headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
                }

                let init_body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "myclaw",
                            "version": "0.1.0",
                        }
                    }
                });

                let init_resp = self.client.post(&url).headers(init_headers).json(&init_body).send().await?;
                let status = init_resp.status();
                if !status.is_success() {
                    let body = init_resp.text().await.unwrap_or_default();
                    anyhow::bail!("GLM MCP search HTTP {}: {}", status, body);
                }

                // Extract mcp-session-id from headers.
                let session_id = init_resp
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let init_text = init_resp.text().await?;

                // Verify init succeeded.
                if let Some(data) = parse_sse_data(&init_text) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        if json.get("error").is_some() {
                            anyhow::bail!(
                                "GLM MCP initialize error: {}",
                                serde_json::to_string(&json["error"]).unwrap_or_default()
                            );
                        }
                    }
                }

                // ── 2. Send initialized notification ────────────────────────
                if let Some(ref sid) = session_id {
                    let mut notif_headers = reqwest::header::HeaderMap::new();
                    notif_headers.insert(
                        reqwest::header::AUTHORIZATION,
                        auth.parse().unwrap(),
                    );
                    notif_headers.insert(
                        reqwest::header::CONTENT_TYPE,
                        "application/json".parse().unwrap(),
                    );
                    notif_headers.insert(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream".parse().unwrap(),
                    );
                    notif_headers.insert(
                        "mcp-session-id",
                        sid.parse().unwrap(),
                    );
                    if let Some(ref ua) = ua {
                        notif_headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
                    }

                    let notif_body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    });

                    let _ = self
                        .client
                        .post(&url)
                        .headers(notif_headers)
                        .json(&notif_body)
                        .send()
                        .await;
                }

                // ── 3. Call web_search_prime ───────────────────────────────
                let mut call_headers = reqwest::header::HeaderMap::new();
                call_headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
                call_headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                );
                call_headers.insert(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream".parse().unwrap(),
                );
                if let Some(ref sid) = session_id {
                    call_headers.insert("mcp-session-id", sid.parse().unwrap());
                }
                if let Some(ref ua) = ua {
                    call_headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
                }

                let call_body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "web_search_prime",
                        "arguments": {
                            "search_query": query,
                            "content_size": "medium",
                            "location": "cn"
                        }
                    }
                });

                let call_resp = self.client.post(&url).headers(call_headers).json(&call_body).send().await?;
                let status = call_resp.status();
                if !status.is_success() {
                    let body = call_resp.text().await.unwrap_or_default();
                    anyhow::bail!("GLM MCP search HTTP {}: {}", status, body);
                }
                call_resp.text().await.map_err(|e| anyhow::anyhow!(e.to_string()))
            })
        })?;

        // ── 4. Parse SSE → JSON-RPC → results ────────────────────────────
        let data = parse_sse_data(&text)
            .ok_or_else(|| anyhow::anyhow!("GLM MCP search: no data in SSE response"))?;

        let rpc: serde_json::Value = serde_json::from_str(data)
            .map_err(|e| anyhow::anyhow!("GLM MCP search: failed to parse JSON-RPC response: {}", e))?;

        if let Some(err) = rpc.get("error") {
            anyhow::bail!(
                "GLM MCP search error: {}",
                serde_json::to_string(err).unwrap_or_default()
            );
        }

        // result.content[0].text is a JSON-encoded array of search results.
        let inner_text = rpc["result"]["content"][0]["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("GLM MCP search: unexpected response structure"))?;

        #[derive(serde::Deserialize)]
        struct McpSearchItem {
            title: String,
            link: String,
            #[serde(default)]
            content: String,
        }

        let items: Vec<McpSearchItem> = serde_json::from_str(inner_text)
            .map_err(|e| anyhow::anyhow!("GLM MCP search: failed to parse results array: {}", e))?;

        let results: Vec<SearchResult> = items
            .into_iter()
            .take(limit)
            .map(|item| SearchResult {
                title: item.title,
                url: item.link,
                snippet: item.content,
                published_at: None,
            })
            .collect();

        let total = results.len() as u64;
        Ok(SearchResults {
            results,
            total: Some(total),
            query: req.query,
        })
    }
}
