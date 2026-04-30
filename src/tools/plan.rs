//! Plan 工具 — 维护结构化执行计划
//!
//! 灵感来自 Codex 的 update_plan 工具。
//! Agent 可以维护一个步骤列表，追踪任务进度。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::providers::{Tool, ToolResult};

/// 计划步骤
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String, // "pending", "in_progress", "completed"
}

/// 共享的计划状态
#[derive(Debug, Clone, Default)]
pub struct PlanState {
    pub steps: Vec<PlanStep>,
    pub explanation: Option<String>,
}

/// Plan 工具
pub struct UpdatePlanTool {
    state: Arc<RwLock<PlanState>>,
}

impl UpdatePlanTool {
    pub fn new(state: Arc<RwLock<PlanState>>) -> Self {
        Self { state }
    }

    pub fn shared_state() -> Arc<RwLock<PlanState>> {
        Arc::new(RwLock::new(PlanState::default()))
    }
}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }

    fn description(&self) -> &str {
        "Update the task plan. Provide a list of steps with their status. \
         At most one step can be in_progress at a time. \
         Use this to track progress on multi-step tasks."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "explanation": {
                    "type": "string",
                    "description": "Brief explanation of the current plan or changes"
                },
                "plan": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": { "type": "string", "description": "Description of the step" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current status of the step"
                            }
                        },
                        "required": ["step", "status"]
                    },
                    "description": "The list of plan steps"
                }
            },
            "required": ["plan"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        1_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let plan: Vec<PlanStep> = serde_json::from_value(args["plan"].clone())
            .map_err(|e| anyhow::anyhow!("invalid plan: {}", e))?;

        let explanation = args["explanation"].as_str().map(String::from);

        // 校验：最多一个 in_progress
        let in_progress_count = plan.iter().filter(|s| s.status == "in_progress").count();
        if in_progress_count > 1 {
            return Ok(ToolResult {
                success: false,
                output: serde_json::to_string(&json!({
                    "error": "At most one step can be in_progress at a time",
                    "in_progress_count": in_progress_count
                }))?,
                error: Some("at most one step can be in_progress".to_string()),
            });
        }

        let completed = plan.iter().filter(|s| s.status == "completed").count();
        let total = plan.len();

        let mut state = self.state.write().await;
        state.steps = plan;
        state.explanation = explanation;

        let current = state
            .steps
            .iter()
            .find(|s| s.status == "in_progress")
            .map(|s| s.step.as_str())
            .unwrap_or("none");

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({
                "ok": true,
                "progress": format!("{}/{} completed", completed, total),
                "current": current
            }))?,
            error: None,
        })
    }
}
