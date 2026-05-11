use std::time::Duration;

use crate::providers::capability_tool::ToolResult;
use crate::providers::ToolCall;

use super::AgentLoop;
use super::types::is_write_tool;

impl AgentLoop {
    /// Build tool specs from the skills manager.
    pub(crate) fn build_tool_specs(&self) -> Vec<crate::providers::capability_chat::ToolSpec> {
        use crate::providers::capability_chat::ToolSpec;
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
    /// Special-cases `ask_user` and `agent_delegate` to use handlers when available.
    /// Applies framework-level truncation based on the tool's `max_output_tokens()`.
    pub(crate) async fn execute_tool(&mut self, call: &ToolCall) -> anyhow::Result<ToolResult> {
        // Autonomy enforcement: block write-capable tools in ReadOnly mode.
        if let Some(ref autonomy) = self.session.session_override.autonomy {
            if matches!(autonomy, crate::config::agent::AutonomyLevel::ReadOnly) && is_write_tool(&call.name) {
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

        // Special handling for ask_user tool.
        if call.name == "ask_user" {
            if let Some(ref handler) = self.ask_user_handler {
                let args: serde_json::Value = if call.arguments.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&call.arguments).unwrap_or_else(|_| {
                        serde_json::json!({ "raw": &call.arguments })
                    })
                };
                let question = args["question"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

                // Record the assistant's question in session history.
                self.session.add_assistant_text(question.to_string());

                let answer = handler(self.session.id.clone(), question.to_string()).await?;

                // Record the user's answer in session history.
                self.session.add_user_text(answer.clone());

                return Ok(ToolResult {
                    success: true,
                    output: answer,
                    error: None,
                });
            }
        }

        // Special handling for agent_delegate: route based on mode parameter.
        // mode="async" + delegate_handler → background execution, returns task_id.
        // mode="sync" (default) + sub_delegator → blocks until sub-agent completes.
        if call.name == "agent_delegate" {
            let args: serde_json::Value = if call.arguments.is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&call.arguments).unwrap_or_else(|_| {
                    serde_json::json!({ "raw": &call.arguments })
                })
            };
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
                let parent_id = self.session.id.clone();
                let result = delegator.delegate_with_parent(agent_name, task, &parent_id).await;
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

        let tool = self.tools.get(&call.name).ok_or_else(|| {
            anyhow::anyhow!("Unknown tool: '{}'", call.name)
        })?;

        let args: serde_json::Value = if call.arguments.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&call.arguments).unwrap_or_else(|_| {
                serde_json::json!({ "raw": &call.arguments })
            })
        };

        let result = if self.config.tool_timeout_secs > 0 {
            let timeout = Duration::from_secs(self.config.tool_timeout_secs);
            tokio::time::timeout(timeout, tool.execute(args))
                .await
                .unwrap_or_else(|_| {
                    Ok(crate::providers::capability_tool::ToolResult {
                        success: false,
                        output: format!(
                            "Tool '{}' timed out after {}s",
                            call.name, self.config.tool_timeout_secs
                        ),
                        error: Some("timeout".to_string()),
                    })
                })?
        } else {
            tool.execute(args).await?
        };

        // Framework-level truncation based on tool's declared limit.
        let max_tokens = tool.max_output_tokens();
        let truncated_output = crate::tools::truncation::truncate_tool_result(
            &result.output,
            max_tokens,
        );
        if truncated_output.len() != result.output.len() {
            tracing::debug!(
                tool = %call.name,
                original_len = result.output.len(),
                truncated_len = truncated_output.len(),
                max_tokens,
                "tool output truncated by framework"
            );
        }

        Ok(ToolResult {
            success: result.success,
            output: truncated_output,
            error: result.error,
        })
    }
}
