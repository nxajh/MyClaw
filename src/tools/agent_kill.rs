//! Agent kill tool — terminates a running sub-agent and returns partial result.

use serde_json::json;
use std::sync::Arc;

use crate::agents::DelegationManager;
use crate::providers::{Tool, ToolResult};

/// The agent_kill tool — terminates a running sub-agent.
pub struct AgentKillTool {
    delegation_manager: Arc<DelegationManager>,
}

impl AgentKillTool {
    pub fn new(delegation_manager: Arc<DelegationManager>) -> Self {
        Self { delegation_manager }
    }
}

#[async_trait::async_trait]
impl Tool for AgentKillTool {
    fn name(&self) -> &str {
        "agent_kill"
    }

    fn description(&self) -> &str {
        "Terminate a running sub-agent by its task_id. Returns the partial result \
         captured before termination. Use agent_list first to find the task_id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task_id of the sub-agent to terminate."
                }
            },
            "required": ["task_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'task_id' is required"))?;

        let cancelled = self.delegation_manager.cancel(task_id);

        if cancelled {
            Ok(ToolResult {
                success: true,
                output: format!(
                    r#"{{"status": "terminated", "task_id": "{}"}}"#,
                    task_id
                ),
                error: None,
            })
        } else {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Task '{}' not found or already completed", task_id)),
            })
        }
    }
}