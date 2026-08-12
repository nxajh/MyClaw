//! Agent kill tool — terminates a running sub-agent and returns partial result.

use serde_json::json;
use std::sync::Arc;

use crate::agents::DelegationCoordinator;
use crate::providers::{Tool, ToolResult};

/// The agent_kill tool — terminates a running sub-agent.
pub struct AgentKillTool {
    delegator: Arc<DelegationCoordinator>,
}

impl AgentKillTool {
    pub fn new(delegator: Arc<DelegationCoordinator>) -> Self {
        Self { delegator }
    }
}

#[async_trait::async_trait]
impl Tool for AgentKillTool {
    fn name(&self) -> &str {
        "agent_kill"
    }

    fn description(&self) -> &str {
        "Terminate a running sub-agent by its session id. Returns the partial result \
         captured before termination. Use agent_list first to find the session id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "sub_session_id": {
                    "type": "string",
                    "description": "The session id of the sub-agent to terminate."
                }
            },
            "required": ["sub_session_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let sub_session_id = args["sub_session_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'sub_session_id' is required"))?;

        let cancelled = self.delegator.cancel(sub_session_id).await;

        if cancelled {
            Ok(ToolResult {
                success: true,
                output: format!(
                    r#"{{"status": "cancelled", "sub_session_id": "{}"}}"#,
                    sub_session_id
                ),
                error: None,
            })
        } else {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Sub-agent '{}' not found or already completed",
                    sub_session_id
                )),
            })
        }
    }
}
