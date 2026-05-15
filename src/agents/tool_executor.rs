use std::sync::Arc;
use std::time::Duration;

use crate::config::agent::AutonomyLevel;
use crate::providers::ToolCall;
use crate::providers::capability_tool::ToolResult;
use crate::providers::capability_chat::ToolSpec;
use super::tool_registry::ToolRegistry;
use super::session_manager::Session;
use super::agent_impl::{AskUserHandler, DelegateHandler};
use super::sub_agent::SubAgentDelegator;
use super::agent_impl::types::is_write_tool;

/// Executes tool calls on behalf of the main conversation loop.
///
/// Holds the tool registry and optional handlers for special tools (ask_user, agent_delegate).
/// Autonomy enforcement happens here: write tools are blocked in ReadOnly mode.
pub(crate) struct DefaultToolExecutor {
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) ask_user_handler: Option<AskUserHandler>,
    pub(crate) delegate_handler: Option<DelegateHandler>,
    pub(crate) sub_delegator: Option<Arc<SubAgentDelegator>>,
    pub(crate) timeout_secs: u64,
}

impl DefaultToolExecutor {
    pub(crate) fn new(tools: Arc<ToolRegistry>, timeout_secs: u64) -> Self {
        Self {
            tools,
            ask_user_handler: None,
            delegate_handler: None,
            sub_delegator: None,
            timeout_secs,
        }
    }

    /// Build tool specs from the registry.
    pub(crate) fn build_tool_specs(&self) -> Vec<ToolSpec> {
        self.tools
            .all_tools()
            .iter()
            .map(|t| {
                let spec = t.spec();
                ToolSpec {
                    name: spec.name,
                    description: Some(spec.description),
                    input_schema: spec.parameters,
                }
            })
            .collect()
    }

    /// Execute a single tool call.
    ///
    /// `autonomy` controls write-tool blocking. Special tools (ask_user, agent_delegate)
    /// are handled before reaching the generic tool dispatch.
    pub(crate) async fn execute(
        &self,
        call: &ToolCall,
        session: &mut Session,
        autonomy: Option<&AutonomyLevel>,
    ) -> anyhow::Result<ToolResult> {
        // Autonomy enforcement: block write tools in ReadOnly mode.
        if let Some(autonomy) = autonomy {
            if matches!(autonomy, AutonomyLevel::ReadOnly) && is_write_tool(&call.name) {
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

        // ask_user: records Q&A in session history and waits for user reply.
        if call.name == "ask_user" {
            if let Some(ref handler) = self.ask_user_handler {
                let args = parse_tool_args(&call.arguments);
                let question = args["question"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;
                session.add_assistant_text(question.to_string());
                let answer = handler(session.id.clone(), question.to_string()).await?;
                session.add_user_text(answer.clone());
                return Ok(ToolResult { success: true, output: answer, error: None });
            }
        }

        // agent_delegate: async (background) or sync (blocking) sub-agent.
        if call.name == "agent_delegate" {
            let args = parse_tool_args(&call.arguments);
            let agent_name = args["agent"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("'agent' is required"))?;
            let task = args["task"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("'task' is required"))?;
            let mode = args["mode"].as_str().unwrap_or("sync");

            if mode == "async" {
                if let Some(ref handler) = self.delegate_handler {
                    let task_id = handler(agent_name.to_string(), task.to_string())?;
                    return Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Task delegated to sub-agent '{}' (task_id: {}). \
                             The sub-agent is now running in the background. \
                             You will be notified when it completes.",
                            agent_name, task_id
                        ),
                        error: None,
                    });
                } else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("async mode not available: no delegate handler configured".to_string()),
                    });
                }
            }

            // sync mode (default)
            if let Some(ref delegator) = self.sub_delegator {
                let parent_id = session.id.clone();
                let result = delegator.delegate_with_parent(agent_name, task, &parent_id, None, None, None).await;
                return Ok(match result {
                    Ok(output) => ToolResult { success: true, output, error: None },
                    Err(e) => ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Sub-agent '{}' failed: {}", agent_name, e)),
                    },
                });
            }
        }

        // Generic tool dispatch.
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            anyhow::anyhow!("Unknown tool: '{}'", call.name)
        })?;
        let args = parse_tool_args(&call.arguments);
        self.run_tool(tool.as_ref(), &call.name, args).await
    }

    /// Execute a tool with timeout and framework-level output truncation.
    async fn run_tool(
        &self,
        tool: &dyn crate::providers::Tool,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let raw = if self.timeout_secs > 0 {
            let timeout = Duration::from_secs(self.timeout_secs);
            tokio::time::timeout(timeout, tool.execute(args))
                .await
                .unwrap_or_else(|_| Ok(ToolResult {
                    success: false,
                    output: format!("Tool '{}' timed out after {}s", name, self.timeout_secs),
                    error: Some("timeout".to_string()),
                }))?
        } else {
            tool.execute(args).await?
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
    const ALLOWED: &'static [&'static str] = &["file_read", "file_write", "file_edit", "shell"];

    pub(crate) fn new(tools: Arc<ToolRegistry>) -> Self {
        Self { tools }
    }

    pub(crate) async fn execute(&self, call: &ToolCall) -> anyhow::Result<ToolResult> {
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
        tool.execute(args).await
    }
}
