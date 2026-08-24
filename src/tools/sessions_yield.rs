//! Sessions yield tool — deterministic turn hand-off after spawning async
//! sub-agents.
//!
//! Ported from openclaw `sessions_yield` (docs/delegation-notice-queue-rfc.md
//! §3.1). The model calls this after `agent_delegate(mode="async")` when it
//! needs the sub-agent results before replying: the runtime ends the current
//! turn deterministically (see the `sessions_yield` check in `Agent::run`),
//! the sub-agent completion arrives as the next message, and `has_pending`
//! reuses the existing suspension semantics so the turn suspends instead of
//! ending when delegations are outstanding.
//!
//! The tool itself is stateless — `Agent::run` detects it by name; this
//! execute only records the (optional) yield note. A model that never calls
//! it is harmless: natural EndTurn behaves identically (the completion event
//! is still delivered). Yield is acceleration + determinism, not the only
//! path.

use serde_json::json;

use crate::providers::{Tool, ToolResult};

/// The sessions_yield tool — explicit, deterministic turn hand-off.
pub struct SessionsYieldTool;

impl SessionsYieldTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SessionsYieldTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for SessionsYieldTool {
    fn name(&self) -> &str {
        "sessions_yield"
    }

    fn description(&self) -> &str {
        "End the current turn after spawning async sub-agents; results arrive \
         as the next message. Call this when you need sub-agent results before \
         replying. Never poll (do not repeatedly query sub-agent status)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Optional note explaining why you are yielding (shown as progress commentary)."
                }
            },
            "required": []
        })
    }

    fn max_output_tokens(&self) -> usize {
        1024
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let note = args["message"].as_str().unwrap_or_default();
        let output = if note.is_empty() {
            r#"{"status": "yielded"}"#.to_string()
        } else {
            format!(r#"{{"status": "yielded", "message": {}}}"#, json!(note))
        };
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tool::ToolContext;

    fn test_ctx(id: &str) -> ToolContext {
        ToolContext {
            owner: "test".to_string(),
            session_id: id.to_string(),
            reply_target: None,
            last_message: None,
            parent_session_id: None,
            agent_name: "main".to_string(),
            turn_silenced: false,
            turn_headless: false,
            channel: None,
        }
    }

    #[test]
    fn schema_message_is_optional() {
        let tool = SessionsYieldTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"], json!([]));
        assert!(schema["properties"]["message"].is_object());
        assert_eq!(schema["properties"]["message"]["type"], "string");
    }

    #[tokio::test]
    async fn execute_returns_yielded_without_message() {
        let tool = SessionsYieldTool::new();
        let session = test_ctx("yield-test");
        let result = tool
            .execute(json!({}), &session)
            .await
            .expect("execute succeeds");
        assert!(result.success);
        assert_eq!(result.output, r#"{"status": "yielded"}"#);
    }

    #[tokio::test]
    async fn execute_echoes_message() {
        let tool = SessionsYieldTool::new();
        let session = test_ctx("yield-test");
        let result = tool
            .execute(json!({"message": "waiting for coder"}), &session)
            .await
            .expect("execute succeeds");
        assert!(result.success);
        assert!(result.output.contains("waiting for coder"));
        assert!(result.output.contains("yielded"));
    }
}
