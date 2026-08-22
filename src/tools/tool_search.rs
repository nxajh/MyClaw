//! Tool Search 工具 — 搜索可用工具
//!
//! 灵感来自 Codex 的 tool_search 和 Claude Code 的 ToolSearchTool。
//! 让 Agent 能发现 MCP 工具和 Skills。

use async_trait::async_trait;
use serde_json::{Value, json};
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

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'query'"))?
            .to_lowercase();
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        let all_tools = self.tools.all_tools();

        // OR-across-whitespace-separated-terms, ranked by term hits (issue
        // #109). The old semantics required the entire (multi-word) query
        // to appear verbatim as one substring in a single tool's name or
        // description, so a natural concept query like "image audio video"
        // matched nothing — no one tool's description contains all three
        // words, even though each word individually finds a relevant tool.
        let terms: Vec<&str> = query.split_whitespace().collect();
        let mut scored: Vec<(usize, Value)> = Vec::new();
        for tool in &all_tools {
            let name = tool.name().to_lowercase();
            let desc = tool.description().to_lowercase();

            let score = if terms.is_empty() {
                // Empty/whitespace-only query: match everything, as before.
                1
            } else {
                terms
                    .iter()
                    .filter(|term| name.contains(*term) || desc.contains(*term))
                    .count()
            };

            if score > 0 {
                scored.push((
                    score,
                    json!({
                        "name": tool.name(),
                        "description": tool.description()
                    }),
                ));
            }
        }
        // Most matching terms first; stable sort keeps registry order
        // within a score tier.
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let matches: Vec<Value> = scored.into_iter().take(limit).map(|(_, v)| v).collect();

        let mut output = json!({
            "ok": true,
            "query": args["query"],
            "results": matches,
            "total_available": all_tools.len()
        });
        if output["results"].as_array().is_some_and(|a| a.is_empty()) {
            output["hint"] = json!(
                "No matches for any word in the query. tool_search matches whitespace-\
                 separated words individually (not the full phrase) against tool names \
                 and descriptions — try a single, more specific keyword."
            );
        }

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&output)?,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::ToolRegistry;

    struct FakeTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _session: &crate::agents::session::Session,
        ) -> anyhow::Result<ToolResult> {
            unreachable!("not exercised by tool_search tests")
        }
    }

    fn registry() -> Arc<ToolRegistry> {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(FakeTool {
            name: "view_image",
            description: "View an image file.",
        }));
        reg.register(Arc::new(FakeTool {
            name: "hear_audio",
            description: "Listen to an audio file.",
        }));
        reg.register(Arc::new(FakeTool {
            name: "view_video",
            description: "View a video file.",
        }));
        reg.register(Arc::new(FakeTool {
            name: "media_download",
            description: "Download image, audio, or video media from a URL.",
        }));
        reg.register(Arc::new(FakeTool {
            name: "calculator",
            description: "Evaluate an arithmetic expression.",
        }));
        Arc::new(reg)
    }

    async fn search(query: &str, limit: Option<u64>) -> serde_json::Value {
        let tool = ToolSearchTool::new(registry());
        let mut args = json!({"query": query});
        if let Some(l) = limit {
            args["limit"] = json!(l);
        }
        let result = tool
            .execute(args, &crate::agents::session::Session::new("test".into()))
            .await
            .unwrap();
        assert!(result.success);
        serde_json::from_str(&result.output).unwrap()
    }

    /// issue #109: a natural multi-concept query ("image audio video")
    /// used to require all three words in ONE tool's description, which no
    /// tool satisfies, so it returned zero results despite three directly
    /// relevant tools existing. OR semantics must find them.
    #[tokio::test]
    async fn multi_word_query_matches_any_term() {
        let out = search("image audio video", None).await;
        let names: Vec<&str> = out["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"view_image"), "got: {names:?}");
        assert!(names.contains(&"hear_audio"), "got: {names:?}");
        assert!(names.contains(&"view_video"), "got: {names:?}");
        assert!(!names.contains(&"calculator"), "got: {names:?}");
        assert!(out["hint"].is_null(), "should not hint on non-empty results");
    }

    /// A tool matching more of the query's terms ranks above one matching
    /// fewer (media_download's description hits all three words; the
    /// single-medium tools hit only one each).
    #[tokio::test]
    async fn results_ranked_by_term_hit_count() {
        let out = search("image audio video", None).await;
        let names: Vec<&str> = out["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        let media_download_pos = names.iter().position(|n| *n == "media_download").unwrap();
        let view_image_pos = names.iter().position(|n| *n == "view_image").unwrap();
        assert!(
            media_download_pos < view_image_pos,
            "media_download (3-term hit) should rank above view_image (1-term hit): {names:?}"
        );
    }

    #[tokio::test]
    async fn single_word_query_still_works() {
        let out = search("image", None).await;
        let names: Vec<&str> = out["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"view_image"), "got: {names:?}");
    }

    #[tokio::test]
    async fn empty_results_include_a_hint() {
        let out = search("nonexistent_xyz_keyword", None).await;
        assert!(out["results"].as_array().unwrap().is_empty());
        assert!(out["hint"].as_str().is_some_and(|h| !h.is_empty()));
    }

    #[tokio::test]
    async fn limit_still_caps_results() {
        let out = search("view", Some(1)).await;
        assert_eq!(out["results"].as_array().unwrap().len(), 1);
    }
}
