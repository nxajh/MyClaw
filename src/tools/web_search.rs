//! Web search tool — searches the web via a search provider.
//!
//! Routes search queries through the Registry's SearchProvider capability.
//! If no SearchProvider is configured, returns a helpful error message.
//! Supports per-provider cooldown: providers that recently failed with
//! retryable errors are skipped until their cooldown expires.

use async_trait::async_trait;
use std::sync::Arc;
use crate::providers::{ServiceRegistry, Tool, ToolResult};
use crate::providers::search::SearchRequest;
use crate::tools::search_cooldown::{SearchProviderCooldown, parse_search_cooldown};
use serde_json::json;

pub struct WebSearchTool {
    registry: Arc<dyn ServiceRegistry>,
    cooldown: Arc<SearchProviderCooldown>,
}

impl WebSearchTool {
    pub fn new(registry: Arc<dyn ServiceRegistry>, cooldown: Arc<SearchProviderCooldown>) -> Self {
        Self { registry, cooldown }
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
                tracing::debug!(err = %e, "no search provider available");
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
        let mut skipped = 0;
        for (provider, model_id, provider_name) in &chain {
            // Skip providers that are still in cooldown.
            if self.cooldown.is_cooled_down(provider_name) {
                tracing::debug!(
                    provider = %provider_name,
                    "search provider in cooldown, skipping"
                );
                skipped += 1;
                continue;
            }

            tracing::debug!(
                query = %query,
                limit = limit,
                provider_model = %model_id,
                provider = %provider_name,
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
                    let error_str = e.to_string();
                    // classify_and_record internally uses parse_search_cooldown
                    // to extract specific cooldown durations from the response body
                    // (e.g., retry_after JSON field, "try again in X seconds" text).
                    // Falls back to the default cooldown for the classified error type.
                    let reason = self.cooldown.classify_and_record(provider_name, &error_str);

                    // Additional pass: if classify_and_record didn't find a cooldown
                    // (e.g., non-HTTP errors), try parsing the raw error string directly.
                    if !self.cooldown.is_cooled_down(provider_name) {
                        if let Some(parsed) = parse_search_cooldown(&error_str) {
                            self.cooldown.record_failure_with_cooldown(provider_name, parsed);
                        }
                    }

                    tracing::warn!(
                        err = %e,
                        provider = %provider_name,
                        reason = ?reason,
                        "search provider failed, trying next"
                    );
                    last_error = Some(e);
                    // Continue to next provider in chain.
                }
            }
        }

        // All providers failed or were in cooldown.
        if skipped > 0 && skipped == chain.len() {
            let msg = "All search providers are in cooldown. Please try again later.";
            tracing::warn!(providers = skipped, msg);
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(msg.to_string()),
            });
        }

        let msg = last_error.map(|e| e.to_string()).unwrap_or_else(|| "unknown error".into());
        tracing::warn!("all search providers failed: {}", msg);
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("All search providers failed. Last error: {}", msg)),
        })
    }
}
