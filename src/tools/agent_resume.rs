//! Agent resume tool — revives a timed-out sub-agent with a fresh budget.
//!
//! Layer 3 of the sub-agent timeout fix: a timeout kills the *task*, not the
//! *work*. The sub-session history survives on disk, so the parent can
//! resume the delegation from where it was interrupted instead of
//! re-delegating from scratch (losing all context).

use serde_json::json;
use std::sync::Arc;

use crate::agents::DelegationCoordinator;
use crate::providers::{Tool, ToolResult};

pub struct AgentResumeTool {
    delegator: Arc<DelegationCoordinator>,
}

impl AgentResumeTool {
    pub fn new(delegator: Arc<DelegationCoordinator>) -> Self {
        Self { delegator }
    }
}

#[async_trait::async_trait]
impl Tool for AgentResumeTool {
    fn name(&self) -> &str {
        "agent_resume"
    }

    fn description(&self) -> &str {
        "Resume a timed-out sub-agent from where it was interrupted, with a fresh wall-clock budget. \
         The sub-agent's session history and partial work are preserved — it continues the same task \
         instead of restarting from scratch. Only delegations that ended in `timed_out` can be resumed. \
         The tool returns immediately; the resumed sub-agent's result arrives later as a delegation \
         completion notice (the current turn suspends awaiting it)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "sub_session_id": {
                    "type": "string",
                    "description": "The session id of the timed-out sub-agent (from the timeout notice)."
                },
                "extra_secs": {
                    "type": "integer",
                    "description": "Optional fresh budget in seconds. Defaults to the delegation's original timeout; clamped to the agent's max_timeout."
                }
            },
            "required": ["sub_session_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        4_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let sub_session_id = args["sub_session_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'sub_session_id' is required"))?;
        let extra_secs = args["extra_secs"].as_u64();

        match self.delegator.resume_timed_out(sub_session_id, extra_secs) {
            Ok(resumed_id) => Ok(ToolResult {
                success: true,
                output: format!(
                    r#"{{"status": "resumed", "sub_session_id": "{}"}}"#,
                    resumed_id
                ),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("resume failed: {:#}", e)),
            }),
        }
    }
}
