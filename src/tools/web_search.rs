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

        // Obtain the fallback chain from the registry.
        let chain = match self.registry.get_search_fallback_chain() {
            Ok(chain) => chain,
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

        // Try each provider in the fallback chain.
        let mut last_error = None;
        for (provider, model_id) in &chain {
            tracing::debug!(
                query = %query,
                limit = limit,
                provider_model = %model_id,
                "executing web search"
            );

            let request = SearchRequest {
                query: query.to_string(),
                limit: Some(limit),
                search_type: Some(model_id.clone()),
            };

            match provider.search(request) {
                Ok(results) => {
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

                    return Ok(ToolResult {
                        success: true,
                        output,
                        error: None,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        provider = %model_id,
                        "search provider failed, trying next"
                    );
                    last_error = Some(e);
                    // Continue to next provider in chain.
                }
            }
        }

        // All providers failed.
        let msg = last_error.map(|e| e.to_string()).unwrap_or_else(|| "unknown error".into());
        tracing::warn!("all search providers failed: {}", msg);
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("All search providers failed. Last error: {}", msg)),
        })
    }
}
