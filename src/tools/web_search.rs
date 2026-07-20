//! Web search tool — searches the web via a search provider.
//!
//! Routes search queries through the Registry's SearchProvider capability.
//! If no SearchProvider is configured, returns a helpful error message.
//! Supports per-provider cooldown: providers that recently failed with
//! retryable errors are skipped until their cooldown expires.

use crate::providers::search::SearchRequest;
use crate::providers::{ClassifiedError, ProviderRegistry, Tool, ToolResult};
use crate::tools::search_cooldown::{
    SearchProviderCooldown, parse_http_error, parse_search_cooldown,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct WebSearchTool {
    registry: Arc<dyn ProviderRegistry>,
    cooldown: Arc<SearchProviderCooldown>,
}

impl WebSearchTool {
    pub fn new(registry: Arc<dyn ProviderRegistry>, cooldown: Arc<SearchProviderCooldown>) -> Self {
        Self { registry, cooldown }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "search_web"
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

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
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
        for entry in &chain {
            // Skip providers that are still in cooldown.
            if self.cooldown.is_cooled_down(&entry.provider_name) {
                tracing::debug!(
                    provider = %entry.provider_name,
                    "search provider in cooldown, skipping"
                );
                skipped += 1;
                continue;
            }

            tracing::debug!(
                query = %query,
                limit = limit,
                provider_model = %entry.model_id,
                provider = %entry.provider_name,
                "executing web search"
            );

            let request = SearchRequest {
                query: query.to_string(),
                limit: Some(limit),
                search_type: Some(entry.model_id.clone()),
            };

            let max_rotations = entry.credential_pool.as_ref().map(|p| p.len()).unwrap_or(1);

            'credential_retry: for _rotation in 0..max_rotations {
                match entry.provider.search(request.clone()) {
                    Ok(results) => {
                        if results.results.is_empty() {
                            return Ok(ToolResult {
                                success: true,
                                output: format!("No results found for \"{}\".", query),
                                error: None,
                            });
                        }

                        // Format results into a readable text response.
                        let mut output = format!(
                            "Search results for \"{}\" ({} found):\n\n",
                            query,
                            results.results.len()
                        );
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

                        // Classify the error to decide rotation vs cooldown.
                        let classified = {
                            let (status, body) = parse_http_error(&error_str);
                            if let Some(code) = status {
                                ClassifiedError::from_http(code, Some(body))
                            } else {
                                ClassifiedError::from_message(&error_str)
                            }
                        };

                        // Try credential rotation first (before recording cooldown).
                        if classified.should_rotate_credential
                            && entry.credential_pool.is_some()
                            && entry.shared_api_key.is_some()
                        {
                            if let (Some(pool), Some(key)) =
                                (&entry.credential_pool, &entry.shared_api_key)
                            {
                                let old_key = key.get();
                                let old_prefix = old_key.chars().take(4).collect::<String>();
                                tracing::warn!(
                                    err = %error_str,
                                    provider = %entry.provider_name,
                                    key_prefix = %old_prefix,
                                    reason = ?classified.reason,
                                    "credential failed, rotating to next key"
                                );
                                pool.mark_exhausted(&old_key, &classified.reason);
                                match pool.next_credential() {
                                    Some(new_key) => {
                                        key.set(&new_key);
                                        tracing::info!(
                                            provider = %entry.provider_name,
                                            key_prefix = %new_key.chars().take(4).collect::<String>(),
                                            "search credential rotated, retrying same provider"
                                        );
                                        continue 'credential_retry;
                                    }
                                    None => {
                                        tracing::warn!(
                                            provider = %entry.provider_name,
                                            "all credentials exhausted, failing over"
                                        );
                                    }
                                }
                            }
                        }

                        // No rotation possible — record cooldown and move on.
                        let reason = self
                            .cooldown
                            .classify_and_record(&entry.provider_name, &error_str);

                        // Additional pass: if classify_and_record didn't find a cooldown
                        // (e.g., non-HTTP errors), try parsing the raw error string directly.
                        if !self.cooldown.is_cooled_down(&entry.provider_name) {
                            if let Some(parsed) = parse_search_cooldown(&error_str) {
                                self.cooldown
                                    .record_failure_with_cooldown(&entry.provider_name, parsed);
                            }
                        }

                        tracing::warn!(
                            err = %e,
                            provider = %entry.provider_name,
                            reason = ?reason,
                            "search provider failed, trying next"
                        );
                        last_error = Some(e);
                        break 'credential_retry;
                    }
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

        let msg = last_error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".into());
        tracing::warn!("all search providers failed: {}", msg);
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("All search providers failed. Last error: {}", msg)),
        })
    }
}
