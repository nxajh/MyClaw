//! Goal Manager 工具 — 会话级目标管理
//!
//! 灵感来自 Jarvis 的 goal_manager。
//! 在上下文压缩后可以恢复目标。

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::providers::{Tool, ToolResult};

/// 共享的目标状态
#[derive(Debug, Clone, Default)]
pub struct GoalState {
    pub current_goal: Option<String>,
}

pub struct GoalManagerTool {
    state: Arc<RwLock<GoalState>>,
}

impl GoalManagerTool {
    pub fn new(state: Arc<RwLock<GoalState>>) -> Self {
        Self { state }
    }

    pub fn shared_state() -> Arc<RwLock<GoalState>> {
        Arc::new(RwLock::new(GoalState::default()))
    }
}

#[async_trait]
impl Tool for GoalManagerTool {
    fn name(&self) -> &str {
        "goal_manager"
    }

    fn description(&self) -> &str {
        "Manage the current session goal. Use 'set' to update the goal when the \
         overall objective changes, or 'get' to retrieve the current goal. \
         This is especially useful after context compaction to restore the goal."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "get"],
                    "description": "Operation: set (update goal) or get (retrieve goal)"
                },
                "goal": {
                    "type": "string",
                    "description": "The goal text (required when action=set)"
                }
            },
            "required": ["action"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        1_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'action' parameter"))?;

        match action {
            "set" => {
                let goal = args["goal"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing 'goal' for action=set"))?;
                if goal.trim().is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("goal cannot be empty".to_string()),
                    });
                }
                let mut state = self.state.write().await;
                state.current_goal = Some(goal.trim().to_string());
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string(&json!({
                        "ok": true,
                        "goal": goal.trim()
                    }))?,
                    error: None,
                })
            }
            "get" => {
                let state = self.state.read().await;
                match &state.current_goal {
                    Some(goal) => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string(&json!({
                            "ok": true,
                            "goal": goal
                        }))?,
                        error: None,
                    }),
                    None => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string(&json!({
                            "ok": true,
                            "goal": null,
                            "message": "No goal set for this session"
                        }))?,
                        error: None,
                    }),
                }
            }
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("unknown action: {}", action)),
            }),
        }
    }
}
