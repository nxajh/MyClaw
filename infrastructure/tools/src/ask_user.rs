//! Ask user tool — pauses the agent loop to ask the user a question and waits for a response.
//!
//! In a real implementation, this would integrate with the channel layer to
//! send a question and wait for a response. For now, it returns the question
//! as output, signaling that the agent needs user input.

use async_trait::async_trait;
use capability::tool::{Tool, ToolResult};
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

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

        // TODO: Integrate with channel layer to actually pause and wait for user input.
        // For now, return the question as a prompt that the LLM should surface to the user.
        Ok(ToolResult {
            success: true,
            output: format!("[Question for user]: {}", question),
            error: None,
        })
    }
}
