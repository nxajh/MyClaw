//! Web search tool — searches the web via a search provider.
//!
//! Routes search queries through the Registry's SearchProvider capability.
//! If no SearchProvider is configured, returns a helpful error message.

use async_trait::async_trait;
use std::sync::Arc;
use crate::providers::{ServiceRegistry, Tool, ToolResult};
use crate::providers::search::SearchRequest;
use serde_json::json;

pub struct WebSearchTool {
    registry: Arc<dyn ServiceRegistry>,
}

impl WebSearchTool {
    pub fn new(registry: Arc<dyn ServiceRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns search results with titles, URLs, and snippets."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default 5)."
                }
            },
            "required": ["query"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'query' is required"))?;

        let limit = args["limit"].as_u64().unwrap_or(5) as usize;

        // Obtain a search provider from the registry.
        let (provider, model_id) = match self.registry.get_search_provider() {
            Ok(tuple) => tuple,
            Err(e) => {
                tracing::debug!(error = %e, "no search provider available");
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "No search provider is configured. To enable web search, \
                         add a search provider (e.g., GLM) to your routing config. \
                         (details: {})",
                        e
                    )),
                });
            }
        };

        tracing::debug!(
            query = %query,
            limit = limit,
            provider_model = %model_id,
            "executing web search"
        );

        let request = SearchRequest {
            query: query.to_string(),
            limit: Some(limit),
            search_type: None,
        };

        let results = match provider.search(request) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "search provider failed");
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Search failed: {}", e)),
                });
            }
        };

        if results.results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No results found for \"{}\".", query),
                error: None,
            });
        }

        // Format results into a readable text response.
        let mut output = format!("Search results for \"{}\" ({} found):\n\n", query, results.results.len());
        for (i, result) in results.results.iter().enumerate() {
            output.push_str(&format!("{}. {}\n", i + 1, result.title));
            output.push_str(&format!("   URL: {}\n", result.url));
            if !result.snippet.is_empty() {
                output.push_str(&format!("   {}\n", result.snippet));
            }
            if let Some(ref published) = result.published_at {
                output.push_str(&format!("   Published: {}\n", published));
            }
            output.push('\n');
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}
