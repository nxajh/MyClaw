//! Task Manager — 统一的任务/目标管理
//!
//! 合并了 goal_manager、task_manager、update_plan 三个工具。
//! 支持树形结构：goal（无 parent）→ task（有 parent）→ sub-task（嵌套）。

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::providers::{Tool, ToolResult};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: String,
    pub parent_id: Option<String>,
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

impl TaskState {
    fn next_id(&mut self) -> String {
        self.next_id += 1;
        format!("task_{}", self.next_id)
    }

    fn find_task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn find_task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// 收集一个 task 及其所有后代的 id
    fn collect_descendant_ids(&self, id: &str) -> Vec<String> {
        let mut result = vec![id.to_string()];
        let mut stack = vec![id.to_string()];
        while let Some(current) = stack.pop() {
            for task in &self.tasks {
                if task.parent_id.as_deref() == Some(&current) {
                    result.push(task.id.clone());
                    stack.push(task.id.clone());
                }
            }
        }
        result
    }
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
        "Manage tasks and goals in a tree structure. Supports: create, list, update, delete, progress.\n\n\
         - **create**: Create a goal (no parent) or a task (with parent). Supports batch creation by passing an array of subjects.\n\
         - **list**: List tasks. Filter by parent to see sub-tasks of a goal.\n\
         - **update**: Change task status (pending/in_progress/completed/cancelled).\n\
         - **delete**: Delete a task and all its sub-tasks.\n\
         - **progress**: Get completion progress of a goal (x/y completed).\n\n\
         Use tasks to track multi-step work and maintain progress across context compactions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "update", "delete", "progress"],
                    "description": "Operation to perform"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID (required for update/delete/progress)"
                },
                "subject": {
                    "oneOf": [
                        { "type": "string", "description": "A single task/goal subject" },
                        { "type": "array", "items": { "type": "string" }, "description": "Multiple subjects for batch creation" }
                    ],
                    "description": "Brief title. Pass a string for single creation, or an array for batch creation."
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description (optional for create)"
                },
                "parent": {
                    "type": "string",
                    "description": "Parent task ID (optional for create). If omitted, creates a top-level goal."
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

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'action'"))?;

        match action {
            "create" => self.handle_create(&args).await,
            "list" => self.handle_list(&args).await,
            "update" => self.handle_update(&args).await,
            "delete" => self.handle_delete(&args).await,
            "progress" => self.handle_progress(&args).await,
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("unknown action: {}", action)),
            }),
        }
    }
}

impl TaskManagerTool {
    async fn handle_create(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let parent = args["parent"].as_str();
        let description = args["description"].as_str().unwrap_or("");

        let mut state = self.state.write().await;

        // 校验 parent 存在
        if let Some(parent_id) = parent {
            if state.find_task(parent_id).is_none() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("parent task not found: {}", parent_id)),
                });
            }
        }

        let kind = if parent.is_some() { "task" } else { "goal" };

        // 支持 string 或 array
        let subjects: Vec<String> = match &args["subject"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(arr) => {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            }
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("subject must be a string or array of strings".to_string()),
                })
            }
        };

        if subjects.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("subject cannot be empty".to_string()),
            });
        }

        let mut created = Vec::new();
        for subject in &subjects {
            let id = state.next_id();
            let task = Task {
                id: id.clone(),
                parent_id: parent.map(String::from),
                subject: subject.clone(),
                description: if subjects.len() == 1 {
                    description.to_string()
                } else {
                    String::new()
                },
                status: "pending".to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            created.push(json!({
                "task_id": id,
                "subject": subject
            }));
            state.tasks.push(task);
        }

        let result = if created.len() == 1 {
            json!({
                "ok": true,
                "kind": kind,
                "task_id": created[0]["task_id"],
                "subject": created[0]["subject"]
            })
        } else {
            json!({
                "ok": true,
                "kind": kind,
                "tasks": created,
                "count": created.len()
            })
        };

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&result)?,
            error: None,
        })
    }

    async fn handle_list(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let parent = args["parent"].as_str();
        let state = self.state.read().await;

        let filtered: Vec<Value> = state
            .tasks
            .iter()
            .filter(|t| match parent {
                Some(pid) => t.parent_id.as_deref() == Some(pid),
                None => t.parent_id.is_none(), // 无 parent = 只列 goals
            })
            .map(|t| {
                json!({
                    "id": t.id,
                    "subject": t.subject,
                    "status": t.status,
                    "has_children": state.tasks.iter().any(|c| c.parent_id.as_deref() == Some(&t.id))
                })
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({
                "ok": true,
                "tasks": filtered,
                "total": filtered.len()
            }))?,
            error: None,
        })
    }

    async fn handle_update(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'task_id' for update"))?;
        let status = args["status"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'status' for update"))?;

        let valid_statuses = ["pending", "in_progress", "completed", "cancelled"];
        if !valid_statuses.contains(&status) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "invalid status: {}. Must be one of: {:?}",
                    status, valid_statuses
                )),
            });
        }

        let mut state = self.state.write().await;
        match state.find_task_mut(task_id) {
            Some(task) => {
                task.status = status.to_string();
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string(&json!({
                        "ok": true,
                        "task_id": task_id,
                        "subject": task.subject,
                        "new_status": status
                    }))?,
                    error: None,
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("task not found: {}", task_id)),
            }),
        }
    }

    async fn handle_delete(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'task_id' for delete"))?;

        let mut state = self.state.write().await;

        if state.find_task(task_id).is_none() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("task not found: {}", task_id)),
            });
        }

        // 收集所有要删除的 id（自身 + 所有后代）
        let ids_to_remove = state.collect_descendant_ids(task_id);
        let count = ids_to_remove.len();

        state.tasks.retain(|t| !ids_to_remove.contains(&t.id));

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({
                "ok": true,
                "deleted": ids_to_remove,
                "count": count
            }))?,
            error: None,
        })
    }

    async fn handle_progress(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'task_id' for progress"))?;

        let state = self.state.read().await;

        let task = state
            .find_task(task_id)
            .ok_or_else(|| anyhow::anyhow!("task not found: {}", task_id))?;

        // 收集直接子任务
        let children: Vec<&Task> = state
            .tasks
            .iter()
            .filter(|t| t.parent_id.as_deref() == Some(task_id))
            .collect();

        if children.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string(&json!({
                    "ok": true,
                    "task_id": task_id,
                    "subject": task.subject,
                    "status": task.status,
                    "children": 0,
                    "message": "No sub-tasks"
                }))?,
                error: None,
            });
        }

        let completed = children
            .iter()
            .filter(|t| t.status == "completed")
            .count();
        let in_progress = children
            .iter()
            .filter(|t| t.status == "in_progress")
            .count();
        let pending = children
            .iter()
            .filter(|t| t.status == "pending")
            .count();
        let cancelled = children
            .iter()
            .filter(|t| t.status == "cancelled")
            .count();
        let total = children.len();

        let current = children
            .iter()
            .find(|t| t.status == "in_progress")
            .map(|t| t.subject.as_str())
            .unwrap_or("none");

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({
                "ok": true,
                "task_id": task_id,
                "subject": task.subject,
                "status": task.status,
                "progress": format!("{}/{} completed", completed, total),
                "completed": completed,
                "in_progress": in_progress,
                "pending": pending,
                "cancelled": cancelled,
                "total": total,
                "current_step": current
            }))?,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_batch_create() {
        let tool = TaskManagerTool::new(TaskManagerTool::shared_state());

        // 批量创建 goals
        let result = tool
            .execute(json!({
                "action": "create",
                "subject": ["Goal A", "Goal B", "Goal C"]
            }))
            .await
            .unwrap();

        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(output["ok"].as_bool().unwrap());
        assert_eq!(output["count"].as_u64().unwrap(), 3);
        assert_eq!(output["tasks"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_batch_create_subtasks() {
        let tool = TaskManagerTool::new(TaskManagerTool::shared_state());

        // 先创建 goal
        let goal = tool
            .execute(json!({
                "action": "create",
                "subject": "My Goal"
            }))
            .await
            .unwrap();
        let goal_output: Value = serde_json::from_str(&goal.output).unwrap();
        let goal_id = goal_output["task_id"].as_str().unwrap();

        // 批量创建子任务
        let result = tool
            .execute(json!({
                "action": "create",
                "subject": ["Task 1", "Task 2"],
                "parent": goal_id
            }))
            .await
            .unwrap();

        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(output["ok"].as_bool().unwrap());
        assert_eq!(output["count"].as_u64().unwrap(), 2);
    }
}
