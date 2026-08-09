//! Agent list tool — lists running sub-agents in the current session.

use serde_json::json;
use std::sync::Arc;

use crate::agents::DelegationCoordinator;
use crate::providers::{Tool, ToolResult};

/// The agent_list tool — shows running sub-agents.
pub struct AgentListTool {
    delegator: Arc<DelegationCoordinator>,
}

impl AgentListTool {
    pub fn new(delegator: Arc<DelegationCoordinator>) -> Self {
        Self { delegator }
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

    async fn execute(
        &self,
        _args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let records = self.delegator.running_records();

        let items: Vec<serde_json::Value> = records
            .into_iter()
            .map(|r| {
                json!({
                    "task_id": r.task_id,
                    "agent_name": r.agent_name,
                    "status": r.status,
                    "elapsed_secs": r.elapsed_secs
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
