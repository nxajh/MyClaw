//! Tool Search 工具 — 搜索可用工具
//!
//! 灵感来自 Codex 的 tool_search 和 Claude Code 的 ToolSearchTool。
//! 让 Agent 能发现 MCP 工具和 Skills。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::agents::ToolRegistry;
use crate::providers::{Tool, ToolResult};

pub struct ToolSearchTool {
    tools: Arc<ToolRegistry>,
}

impl ToolSearchTool {
    pub fn new(tools: Arc<ToolRegistry>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Search available tools by keyword. Returns matching tool names and descriptions. \
         Use this to discover MCP tools or skills that are available but not in the default tool set."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search keyword to match against tool names and descriptions"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default 10)"
                }
            },
            "required": ["query"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        3_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'query'"))?
            .to_lowercase();
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        let all_tools = self.tools.all_tools();

        let mut matches: Vec<Value> = Vec::new();
        for tool in &all_tools {
            let name = tool.name().to_lowercase();
            let desc = tool.description().to_lowercase();

            if name.contains(&query) || desc.contains(&query) {
                matches.push(json!({
                    "name": tool.name(),
                    "description": tool.description()
                }));
                if matches.len() >= limit {
                    break;
                }
            }
        }

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({
                "ok": true,
                "query": args["query"],
                "results": matches,
                "total_available": all_tools.len()
            }))?,
            error: None,
        })
    }
}
