//! Agent delegate tool — allows the main agent to delegate tasks to sub-agents.
//!
//! This tool is the core of the multi-agent orchestration pattern.
//! The main agent calls `agent_delegate(agent="coder", task="...", mode="sync")` and the tool:
//! 1. Looks up the sub-agent by name
//! 2. Creates a temporary AgentLoop with the sub-agent's system prompt and tools
//! 3. Runs the sub-agent to completion (sync) or in background (async)
//! 4. Returns the result (sync) or task_id (async) to the main agent
//!
//! H47: this tool now talks to [`AgentDelegator`] (the RFC v2 trait that
//! carries `&Session`); the legacy `TaskDelegator` trait was deleted.

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use crate::agents::AgentDelegator;
use crate::providers::{Tool, ToolResult};

/// The agent_delegate tool — injectable delegator for runtime dispatch.
pub struct AgentDelegateTool {
    delegator: Arc<dyn AgentDelegator>,
    /// Cached list of available sub-agents `(name, description)`.
    agents: Vec<(String, Option<String>)>,
}

impl AgentDelegateTool {
    pub fn new(delegator: Arc<dyn AgentDelegator>) -> Self {
        let agents = delegator.list_available();
        Self { delegator, agents }
    }
}

#[async_trait]
impl Tool for AgentDelegateTool {
    fn name(&self) -> &str {
        "agent_delegate"
    }

    fn description(&self) -> &str {
        "Delegate a task to a specialized sub-agent. Each sub-agent has its own system prompt and tool set. \
         Use this to break complex tasks into specialized sub-tasks that are handled by experts. \
         mode='sync' (default) blocks until the sub-agent finishes; mode='async' returns a task_id immediately \
         and the sub-agent runs in the background — you will be notified when it completes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let agent_names: Vec<&str> = self.agents.iter().map(|(n, _)| n.as_str()).collect();
        let agent_descs: Vec<String> = self
            .agents
            .iter()
            .map(|(n, d)| {
                format!(
                    "- {}: {}",
                    n,
                    d.as_deref().unwrap_or("No description")
                )
            })
            .collect();

        let agent_description = if agent_names.is_empty() {
            "Name of the sub-agent to delegate to.".to_string()
        } else {
            format!(
                "Name of the sub-agent to delegate to. Available agents:\n{}",
                agent_descs.join("\n")
            )
        };

        let mut schema = json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": agent_description
                },
                "task": {
                    "type": "string",
                    "description": "A clear description of the task to delegate."
                },
                "mode": {
                    "type": "string",
                    "enum": ["sync", "async"],
                    "description": "Execution mode. 'sync' (default) blocks until completion. 'async' runs in the background and returns a task_id."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Maximum wall-clock seconds for the sub-agent. Overrides the agent config default (600s). Hard ceiling is 1800s."
                }
            },
            "required": ["agent", "task"]
        });

        if !agent_names.is_empty() {
            schema["properties"]["agent"]["enum"] = json!(agent_names);
        }

        schema
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let agent_name = args["agent"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'agent' is required"))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'task' is required"))?;
        let mode = args["mode"].as_str().unwrap_or("sync");
        let timeout = args["timeout"].as_u64();

        tracing::info!(agent = %agent_name, task_len = task.len(), mode = %mode, timeout = ?timeout, "delegating task to sub-agent");

        if mode == "async" {
            match self.delegator.delegate_async(agent_name, task, session, timeout) {
                Ok(task_id) => Ok(ToolResult {
                    success: true,
                    output: json!({
                        "ok": true,
                        "mode": "async",
                        "task_id": task_id,
                        "message": "Sub-agent spawned in background. Use agent_list to check status."
                    })
                    .to_string(),
                    error: None,
                }),
                Err(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to spawn sub-agent '{}': {}", agent_name, e)),
                }),
            }
        } else {
            match self.delegator.delegate(agent_name, task, session, timeout).await {
                Ok(result) => Ok(ToolResult {
                    success: true,
                    output: result,
                    error: None,
                }),
                Err(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Sub-agent '{}' failed: {}", agent_name, e)),
                }),
            }
        }
    }
}
