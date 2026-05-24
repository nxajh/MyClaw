//! Agent delegate tool — allows the main agent to delegate tasks to sub-agents.
//!
//! F35: upgraded to handle both sync and async delegation modes.
//! The `TaskDelegator` trait is injected at construction time.

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use crate::providers::{Tool, ToolResult};

/// Shared trait for task delegation — implemented by the Agent/Orchestrator layer.
#[async_trait]
pub trait TaskDelegator: Send + Sync {
    /// Delegate a task to a named sub-agent and return its text response.
    async fn delegate(&self, agent_name: &str, task: &str) -> anyhow::Result<String>;

    /// Delegate asynchronously — returns a task_id immediately.
    async fn delegate_async(&self, agent_name: &str, task: &str) -> anyhow::Result<String>;

    /// List available sub-agent names and their descriptions.
    fn available_agents(&self) -> Vec<(String, String)>;
}

/// The agent_delegate tool — injectable delegator for runtime dispatch.
pub struct AgentDelegateTool {
    delegator: Arc<dyn TaskDelegator>,
}

impl AgentDelegateTool {
    pub fn new(delegator: Arc<dyn TaskDelegator>) -> Self {
        Self { delegator }
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
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Name of the sub-agent to delegate to."
                },
                "task": {
                    "type": "string",
                    "description": "A clear description of the task to delegate."
                },
                "mode": {
                    "type": "string",
                    "enum": ["sync", "async"],
                    "description": "Execution mode. 'sync' (default) blocks until completion. 'async' runs in the background and returns a task_id."
                }
            },
            "required": ["agent", "task"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult> {
        let agent_name = args["agent"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'agent' is required"))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'task' is required"))?;
        let mode = args["mode"].as_str().unwrap_or("sync");

        tracing::info!(agent = %agent_name, mode, task_len = task.len(), "delegating task to sub-agent");

        if mode == "async" {
            match self.delegator.delegate_async(agent_name, task).await {
                Ok(task_id) => Ok(ToolResult {
                    success: true,
                    output: format!("Task delegated asynchronously. task_id: {}", task_id),
                    error: None,
                }),
                Err(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Sub-agent '{}' async delegation failed: {}", agent_name, e)),
                }),
            }
        } else {
            match self.delegator.delegate(agent_name, task).await {
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
