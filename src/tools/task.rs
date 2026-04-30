//! Task Manager 工具 — 统一的任务 CRUD
//!
//! 灵感来自 Claude Code 的 Task*Tool 系列和 Jarvis 的 task_list_manager。
//! 支持创建、列出、更新、删除任务。

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::providers::{Tool, ToolResult};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String, // "pending", "in_progress", "completed", "cancelled"
    pub created_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct TaskState {
    pub tasks: Vec<Task>,
    pub next_id: u32,
}

pub struct TaskManagerTool {
    state: Arc<RwLock<TaskState>>,
}

impl TaskManagerTool {
    pub fn new(state: Arc<RwLock<TaskState>>) -> Self {
        Self { state }
    }

    pub fn shared_state() -> Arc<RwLock<TaskState>> {
        Arc::new(RwLock::new(TaskState::default()))
    }
}

#[async_trait]
impl Tool for TaskManagerTool {
    fn name(&self) -> &str {
        "task_manager"
    }

    fn description(&self) -> &str {
        "Manage tasks. Supports: create, list, update, delete. \
         Use tasks to track multi-step work and maintain progress across context compactions."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "update", "delete"],
                    "description": "Operation to perform"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID (required for update/delete)"
                },
                "subject": {
                    "type": "string",
                    "description": "Brief title (required for create)"
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description (optional for create)"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "cancelled"],
                    "description": "New status (required for update)"
                }
            },
            "required": ["action"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'action'"))?;

        match action {
            "create" => {
                let subject = args["subject"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing 'subject' for create"))?;
                let description = args["description"].as_str().unwrap_or("");

                let mut state = self.state.write().await;
                state.next_id += 1;
                let id = format!("task_{}", state.next_id);

                let task = Task {
                    id: id.clone(),
                    subject: subject.to_string(),
                    description: description.to_string(),
                    status: "pending".to_string(),
                    created_at: Utc::now().to_rfc3339(),
                };
                state.tasks.push(task);

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string(&json!({
                        "ok": true,
                        "task_id": id,
                        "subject": subject
                    }))?,
                    error: None,
                })
            }
            "list" => {
                let state = self.state.read().await;
                let tasks: Vec<Value> = state
                    .tasks
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "subject": t.subject,
                            "status": t.status
                        })
                    })
                    .collect();

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string(&json!({
                        "ok": true,
                        "tasks": tasks,
                        "total": tasks.len()
                    }))?,
                    error: None,
                })
            }
            "update" => {
                let task_id = args["task_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing 'task_id' for update"))?;
                let status = args["status"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing 'status' for update"))?;

                let mut state = self.state.write().await;
                if let Some(task) = state.tasks.iter_mut().find(|t| t.id == task_id) {
                    task.status = status.to_string();
                    Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string(&json!({
                            "ok": true,
                            "task_id": task_id,
                            "new_status": status
                        }))?,
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("task not found: {}", task_id)),
                    })
                }
            }
            "delete" => {
                let task_id = args["task_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing 'task_id' for delete"))?;

                let mut state = self.state.write().await;
                let before = state.tasks.len();
                state.tasks.retain(|t| t.id != task_id);

                if state.tasks.len() < before {
                    Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string(&json!({"ok": true, "deleted": task_id}))?,
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("task not found: {}", task_id)),
                    })
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
