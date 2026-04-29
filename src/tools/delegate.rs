//! Delegate task tool — allows the router agent to delegate tasks to sub-agents.
//!
//! This tool is the core of the multi-agent orchestration pattern.
//! The router agent calls `delegate_task(agent="coder", task="...")` and the tool:
//! 1. Looks up the sub-agent by name
//! 2. Creates a temporary AgentLoop with the sub-agent's system prompt and tools
//! 3. Runs the sub-agent to completion
//! 4. Returns the result to the router agent

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

/// The delegate_task tool — injectable delegator for runtime dispatch.
pub struct DelegateTaskTool {
    delegator: Arc<dyn TaskDelegator>,
}

impl DelegateTaskTool {
    pub fn new(delegator: Arc<dyn TaskDelegator>) -> Self {
        Self { delegator }
    }
}

#[async_trait]
impl Tool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn description(&self) -> &str {
        "Delegate a task to a specialized sub-agent. Each sub-agent has its own system prompt and tool set. \
         Use this to break complex tasks into specialized sub-tasks that are handled by experts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // Build enum of available agent names for the schema.
        let agents: Vec<serde_json::Value> = self
            .delegator
            .available_agents()
            .into_iter()
            .map(|(name, desc)| {
                if desc.is_empty() {
                    json!(name)
                } else {
                    json!(format!("{}: {}", name, desc))
                }
            })
            .collect();

        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": format!("Name of the sub-agent to delegate to. Available: {}", agents.join(", "))
                },
                "task": {
                    "type": "string",
                    "description": "A clear description of the task to delegate."
                }
            },
            "required": ["agent", "task"]
        })
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
