//! Agent list tool — lists running sub-agents in the current session.

use serde_json::json;
use std::sync::Arc;

use crate::agents::DelegationManager;
use crate::providers::{Tool, ToolResult};

/// The agent_list tool — shows running sub-agents.
pub struct AgentListTool {
    delegation_manager: Arc<DelegationManager>,
}

impl AgentListTool {
    pub fn new(delegation_manager: Arc<DelegationManager>) -> Self {
        Self { delegation_manager }
    }
}

#[async_trait::async_trait]
impl Tool for AgentListTool {
    fn name(&self) -> &str {
        "agent_list"
    }

    fn description(&self) -> &str {
        "List all sub-agents currently running in the background for this session. \
         Shows task_id, agent name, and status."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn max_output_tokens(&self) -> usize {
        2000
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let running = self.delegation_manager.running_snapshot();

        let items: Vec<serde_json::Value> = running
            .into_iter()
            .map(|task_id| {
                json!({
                    "task_id": task_id,
                    "status": "running"
                })
            })
            .collect();

        let output = if items.is_empty() {
            "No sub-agents currently running.".to_string()
        } else {
            serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string())
        };

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}