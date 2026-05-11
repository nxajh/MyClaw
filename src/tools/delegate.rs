//! Agent delegate tool — allows the main agent to delegate tasks to sub-agents.
//!
//! This tool is the core of the multi-agent orchestration pattern.
//! The main agent calls `agent_delegate(agent="coder", task="...", mode="sync")` and the tool:
//! 1. Looks up the sub-agent by name
//! 2. Creates a temporary AgentLoop with the sub-agent's system prompt and tools
//! 3. Runs the sub-agent to completion (sync) or in background (async)
//! 4. Returns the result (sync) or task_id (async) to the main agent

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use crate::providers::{Tool, ToolResult};

/// Shared trait for task delegation — implemented by the Agent/Orchestrator layer.
#[async_trait]
pub trait TaskDelegator: Send + Sync {
    /// Delegate a task to a named sub-agent and return its text response.
    async fn delegate(&self, agent_name: &str, task: &str) -> anyhow::Result<String>;

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

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let agent_name = args["agent"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'agent' is required"))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'task' is required"))?;

        tracing::info!(agent = %agent_name, task_len = task.len(), "delegating task to sub-agent");

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
