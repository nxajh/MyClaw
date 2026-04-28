//! Web search tool — searches the web via a search provider.
//!
//! This tool is a placeholder that performs basic web searches.
//! In the future, it will route through the Registry's SearchProvider capability.

use async_trait::async_trait;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

pub struct WebSearchTool;

impl WebSearchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
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

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'query' is required"))?;

        let _limit = args["limit"].as_u64().unwrap_or(5) as usize;

        // TODO: Route through Registry's SearchProvider capability.
        // For now, return a helpful message indicating the search query was received
        // but no search backend is configured yet.
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "web_search for '{}' not yet connected to a search provider. Configure a SearchProvider in routing to enable web search.",
                query
            )),
        })
    }
}
