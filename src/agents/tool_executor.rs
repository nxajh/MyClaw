use std::sync::Arc;
use std::time::Duration;

use crate::config::agent::PermissionMode;
use crate::providers::ToolCall;
use crate::providers::capability_tool::ToolResult;
use super::session::Session;
use super::tokens::is_write_tool;
use super::tool_registry::ToolRegistry;

/// Executes tool calls on behalf of `Agent::run`.
///
/// Per the RFC v2 target shape, the executor is a global singleton —
/// it carries only the timeout policy and dispatches against a
/// `&[Arc<dyn Tool>]` slice the caller (Agent::run) has already
/// filtered through the agent's `tools` / `skills` / `mcp` filters.
/// No `ToolRegistry` field; the executor is stateless w.r.t. which
/// tools an agent may call.
pub struct ToolExecutor {
    pub timeout_secs: u64,
}

impl ToolExecutor {
    pub fn new(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }

    /// Execute a single tool call against the pre-filtered `allowed`
    /// slice.
    ///
    /// `autonomy` controls write-tool blocking. Special tools
    /// (`ask_user`, `agent_delegate`) are real `Tool` impls in
    /// `allowed`, dispatched through the same path as any other tool.
    pub(crate) async fn execute(
        &self,
        call: &ToolCall,
        session: &mut Session,
        autonomy: Option<&PermissionMode>,
        allowed: &[Arc<dyn crate::providers::Tool>],
    ) -> anyhow::Result<ToolResult> {
        // Autonomy enforcement: block write tools in ReadOnly mode.
        if let Some(autonomy) = autonomy {
            if matches!(autonomy, PermissionMode::ReadOnly) && is_write_tool(&call.name) {
                tracing::info!(tool = %call.name, "tool blocked by ReadOnly autonomy policy");
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Tool '{}' is not allowed in read-only mode (autonomy: ReadOnly).",
                        call.name
                    ),
                    error: Some("autonomy_policy: ReadOnly".to_string()),
                });
            }
        }

        // Generic tool dispatch against the filtered slice. `ask_user` /
        // `agent_delegate` are real Tool impls in `allowed` (no
        // special-casing).
        let tool = allowed
            .iter()
            .find(|t| t.spec().name == call.name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: '{}'", call.name))?;
        let args = parse_tool_args(&call.arguments);
        self.run_tool(tool.as_ref(), &call.name, args, session).await
    }

    /// Execute a tool with timeout and framework-level output truncation.
    async fn run_tool(
        &self,
        tool: &dyn crate::providers::Tool,
        name: &str,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let raw = if self.timeout_secs > 0 {
            let timeout = Duration::from_secs(self.timeout_secs);
            tokio::time::timeout(timeout, tool.execute(args, session))
                .await
                .unwrap_or_else(|_| Ok(ToolResult {
                    success: false,
                    output: format!("Tool '{}' timed out after {}s", name, self.timeout_secs),
                    error: Some("timeout".to_string()),
                }))?
        } else {
            tool.execute(args, session).await?
        };

        let max_tokens = tool.max_output_tokens();
        let output = crate::tools::truncation::truncate_tool_result(&raw.output, max_tokens);
        if output.len() != raw.output.len() {
            tracing::debug!(
                tool = %name,
                original_len = raw.output.len(),
                truncated_len = output.len(),
                max_tokens,
                "tool output truncated by framework"
            );
        }
        Ok(ToolResult { output, ..raw })
    }
}


pub(crate) fn parse_tool_args(arguments: &str) -> serde_json::Value {
    if arguments.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments).unwrap_or_else(|_| {
            serde_json::json!({ "raw": arguments })
        })
    }
}

/// Restricted tool executor for the compaction summarizer.
///
/// Only allows file read/write/edit and shell — prevents the summarizer from
/// touching session state, triggering ask_user, or spawning sub-agents.
pub(crate) struct MemoryToolExecutor {
    tools: Arc<ToolRegistry>,
}

impl MemoryToolExecutor {
    /// Tools the summarizer is permitted to call during compaction. Memory is
    /// file-based (notes under the knowledge dir), so these four cover it. The
    /// summarizer request still advertises the *full* tool list (for prefix-cache
    /// matching with the main request); this allow-list is the actual gate —
    /// any other tool call is blocked with an error result.
    const ALLOWED: &'static [&'static str] = &["file_read", "file_write", "file_edit", "shell"];

    pub(crate) fn new(tools: Arc<ToolRegistry>) -> Self {
        Self { tools }
    }

    pub(crate) async fn execute(&self, call: &ToolCall, session: &Session) -> anyhow::Result<ToolResult> {
        if !Self::ALLOWED.contains(&call.name.as_str()) {
            tracing::warn!(tool = %call.name, "summarizer tried to call restricted tool, blocking");
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("tool '{}' not available during compaction summarization", call.name)),
            });
        }
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            anyhow::anyhow!("tool '{}' not found in registry", call.name)
        })?;
        let args = parse_tool_args(&call.arguments);
        tool.execute(args, session).await
    }
}
