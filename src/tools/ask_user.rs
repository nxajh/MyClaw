//! Ask user tool — pauses the agent loop to ask the user a question and waits for a response.
//!
//! When the AgentLoop has an `AskUserHandler` wired (set by the Orchestrator),
//! `ask_user` calls are intercepted before reaching this fallback implementation.
//!
//! This fallback exists for scenarios where the agent is run without a channel
//! (e.g. CLI mode, tests). It returns the question text so the LLM can surface
//! it to the user in its own response.

use async_trait::async_trait;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

pub struct AskUserTool;

impl AskUserTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their response. Use this when you need clarification or confirmation before proceeding."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user."
                }
            },
            "required": ["question"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        1_000
    }

    /// Fallback: returns the question so the LLM can surface it to the user.
    ///
    /// When the Orchestrator is active, this code path is NOT reached —
    /// the `AgentLoop` intercepts `ask_user` and uses the `AskUserHandler`
    /// to send the question through the channel and wait for a real reply.
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

        Ok(ToolResult {
            success: true,
            output: format!(
                "Please answer this question: {} (Note: direct channel delivery is not available, \
                 please respond in the conversation.)",
                question
            ),
            error: None,
        })
    }
}
