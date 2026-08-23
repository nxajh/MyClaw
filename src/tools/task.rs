//! Task tools — split into four independent tools sharing the same state.
//!
//! `task_create`, `task_list`, `task_update`, `task_delete` each have their
//! own struct so that `required` fields can be expressed per-tool without
//! relying on `oneOf`/`anyOf` (which Gemini and grok don't support).
//!
//! P1-B1: the task board is **per-session** — persisted at
//! `{sessions_root}/{uuid}/tasks.json` and resolved from the `Session` passed
//! to `execute`. The previous global board (`workspace/.state/tasks.json`,
//! shared by every session) was an isolation defect and is no longer read.

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::ids::{bare_dir_name, DEFAULT_NAMESPACE, Fqid, TYPE_TASK};
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskState {
    pub tasks: Vec<Task>,
    /// Namespace for generated task FQIDs (`<ns>/t/<uuidv7>`).
    /// Skipped in serialized form — bound at load time; `Default` (ephemeral
    /// state) falls back to `DEFAULT_NAMESPACE`.
    #[serde(skip)]
    pub namespace: String,
    /// Optional JSON persistence target; every mutation is saved to this file.
    /// Skipped in serialized form — bound at load time, not stored inside it.
    #[serde(skip)]
    pub save_path: Option<std::path::PathBuf>,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            namespace: DEFAULT_NAMESPACE.to_string(),
            save_path: None,
        }
    }
}

impl TaskState {
    /// Load persisted state from disk; returns defaults when missing or unparsable.
    /// `namespace` is bound to the loaded state for subsequent task FQID generation.
    pub fn load(path: &std::path::Path, namespace: &str) -> TaskState {
        let mut state = std::fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_json::from_str::<TaskState>(&content).ok())
            .unwrap_or_default();
        state.save_path = Some(path.to_path_buf());
        state.namespace = namespace.to_string();
        state
    }

    /// Persist to `save_path` when configured; failures are logged, not fatal
    /// (the in-memory state remains authoritative for the current process).
    pub fn save(&self) {
        let Some(path) = self.save_path.as_deref() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(content) => {
                if let Err(e) = std::fs::write(path, content) {
                    tracing::warn!(err = %e, path = %path.display(), "task_state: failed to save");
                }
            }
            Err(e) => tracing::warn!(err = %e, "task_state: failed to serialize"),
        }
    }

    fn next_id(&self) -> String {
        Fqid::new(&self.namespace, TYPE_TASK).to_string()
    }

    fn find_task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn find_task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Collect the id of a task and all its descendants.
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

    /// Render the task tree as a text block suitable for injection after
    /// context compaction, so the model retains its planning state.
    pub fn format_for_injection(&self) -> Option<String> {
        let goals: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|t| t.parent_id.is_none())
            .collect();
        if goals.is_empty() {
            return None;
        }

        let mut lines =
            vec!["[Your active task list was preserved across context compaction]".to_string()];
        for goal in &goals {
            lines.push(format!(
                "- [{}] {} ({})",
                goal.status, goal.id, goal.subject
            ));

            // Show direct children.
            let children: Vec<&Task> = self
                .tasks
                .iter()
                .filter(|t| t.parent_id.as_deref() == Some(&goal.id))
                .collect();
            for child in &children {
                lines.push(format!(
                    "    - [{}] {} ({})",
                    child.status, child.id, child.subject
                ));
            }
        }
        Some(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Per-session task boards (P1-B1)
// ---------------------------------------------------------------------------

pub type SharedTaskState = Arc<RwLock<TaskState>>;

/// Per-session task board locator (P1-B1).
///
/// Resolves each session's task board to `{sessions_root}/{uuid}/tasks.json`
/// (same bare-uuid session directory the session backend uses). Boards are
/// loaded from disk on demand and saved back after every mutation — no
/// process-global shared state, so sessions can never see each other's tasks.
#[derive(Debug, Clone)]
pub struct TaskBoards {
    /// Sessions storage root (`{base_dir}/sessions`).
    sessions_root: PathBuf,
    /// Namespace for generated task FQIDs (`<ns>/t/<uuidv7>`).
    namespace: String,
}

impl TaskBoards {
    pub fn new(sessions_root: PathBuf, namespace: impl Into<String>) -> Self {
        Self {
            sessions_root,
            namespace: namespace.into(),
        }
    }

    /// Task board file for a session: `{sessions_root}/{uuid}/tasks.json`.
    pub fn board_path(&self, session_id: &str) -> PathBuf {
        self.sessions_root
            .join(bare_dir_name(session_id))
            .join("tasks.json")
    }

    /// Load the session's board from disk (defaults when absent/corrupt).
    pub fn load(&self, session_id: &str) -> TaskState {
        TaskState::load(&self.board_path(session_id), &self.namespace)
    }

    /// Load the session's board wrapped in the shared handle the tools use
    /// (mutations inside go through [`TaskState::save`]).
    pub fn board(&self, session_id: &str) -> SharedTaskState {
        Arc::new(RwLock::new(self.load(session_id)))
    }
}

// ---------------------------------------------------------------------------
// Unknown-task-id listing (issue #132, P1)
// ---------------------------------------------------------------------------

/// Cap on how many tasks a not-found error enumerates, and how much of each
/// subject it previews — mirrors `shell.rs`'s `format_unknown_process_listing`
/// (issue #130): a copied or hallucinated id gets a real, capped list to
/// self-correct against instead of a bare "not found".
const UNKNOWN_TASK_LISTING_CAP: usize = 20;
const UNKNOWN_TASK_SUBJECT_PREVIEW_CHARS: usize = 60;

/// Build the "here's what's actually on this board" listing appended to a
/// task_create/task_update/task_delete not-found error. The board is already
/// per-session (`TaskState` is loaded from that session's own `tasks.json`),
/// so no session filtering is needed here, unlike the shell process table.
/// Ids are shown in full (not a shortened tail) to match `task_list`'s own
/// display convention — a listing in a different id format would be its own
/// source of confusion.
///
/// Reviewer note (PR #133): capping over `state.tasks`' insertion order would
/// keep the *oldest* tasks and drop the newest ones once a board exceeds the
/// cap — backwards, since a copied or hallucinated id is most likely to
/// reference a task that was just created (both production incidents were).
/// Shown newest-first instead, mirroring #130's shell listing.
fn format_unknown_task_listing(state: &TaskState) -> String {
    let total = state.tasks.len();
    if total == 0 {
        return " This session's task board is empty.".to_string();
    }
    let shown: Vec<String> = state
        .tasks
        .iter()
        .rev()
        .take(UNKNOWN_TASK_LISTING_CAP)
        .map(|t| {
            let preview =
                crate::str_utils::truncate_line(&t.subject, UNKNOWN_TASK_SUBJECT_PREVIEW_CHARS);
            format!("  {} status={} subject={:?}", t.id, t.status, preview)
        })
        .collect();
    let omitted = total.saturating_sub(shown.len());
    let omitted_note = if omitted > 0 {
        format!("\n  ... and {omitted} more")
    } else {
        String::new()
    };
    format!(
        " This session's tasks (use task_list to browse the tree):\n{}{}",
        shown.join("\n"),
        omitted_note
    )
}

// ---------------------------------------------------------------------------
// task_create
// ---------------------------------------------------------------------------

pub struct TaskCreateTool {
    boards: TaskBoards,
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }

    fn description(&self) -> &str {
        "Create a goal (no parent) or a task (with parent). Supports batch creation by passing an array of subjects.\n\n\
         Use tasks to track multi-step work and maintain progress across context compactions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": {
                    "oneOf": [
                        { "type": "string", "description": "A single task/goal subject" },
                        { "type": "array", "items": { "type": "string" }, "description": "Multiple subjects for batch creation" }
                    ],
                    "description": "Brief title. Pass a string for single creation, or an array for batch creation."
                },
                "details": {
                    "type": "string",
                    "description": "Detailed description (optional)"
                },
                "parent": {
                    "type": "string",
                    "description": "Parent task ID (optional). If omitted, creates a top-level goal."
                }
            },
            "required": ["subject"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(
        &self,
        args: Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let parent = args["parent"].as_str();
        let description = args["details"].as_str().unwrap_or("");

        let board = self.boards.board(&session.id);
        let mut state = board.write().await;

        // Verify parent exists
        if let Some(parent_id) = parent {
            if state.find_task(parent_id).is_none() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "parent task not found: {}.{}",
                        parent_id,
                        format_unknown_task_listing(&state)
                    )),
                });
            }
        }

        let kind = if parent.is_some() { "task" } else { "goal" };

        // Support string or array
        let subjects: Vec<String> = match &args["subject"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("subject must be a string or array of strings".to_string()),
                });
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
        state.save();

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
}

// ---------------------------------------------------------------------------
// task_list
// ---------------------------------------------------------------------------

pub struct TaskListTool {
    boards: TaskBoards,
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }

    fn description(&self) -> &str {
        "List tasks. Filter by parent to see sub-tasks of a goal. Without a parent filter, lists only top-level goals."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent": {
                    "type": "string",
                    "description": "Parent task ID. If set, lists sub-tasks of that goal. If omitted, lists top-level goals."
                }
            },
            "required": []
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(
        &self,
        args: Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let parent = args["parent"].as_str();
        let board = self.boards.board(&session.id);
        let state = board.read().await;

        let filtered: Vec<Value> = state
            .tasks
            .iter()
            .filter(|t| match parent {
                Some(pid) => t.parent_id.as_deref() == Some(pid),
                None => t.parent_id.is_none(),
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
}

// ---------------------------------------------------------------------------
// task_update
// ---------------------------------------------------------------------------

pub struct TaskUpdateTool {
    boards: TaskBoards,
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "task_update"
    }

    fn description(&self) -> &str {
        "Change task status (pending/in_progress/completed/cancelled).\n\n\
         Use tasks to track multi-step work and maintain progress across context compactions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task ID to update"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "cancelled"],
                    "description": "New status"
                }
            },
            "required": ["task_id", "status"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(
        &self,
        args: Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'task_id'"))?;
        let status = args["status"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'status'"))?;

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

        let board = self.boards.board(&session.id);
        let mut state = board.write().await;
        match state.find_task_mut(task_id) {
            Some(task) => {
                task.status = status.to_string();
                let subject = task.subject.clone();
                state.save();
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string(&json!({
                        "ok": true,
                        "task_id": task_id,
                        "subject": subject,
                        "new_status": status
                    }))?,
                    error: None,
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "task not found: {}.{}",
                    task_id,
                    format_unknown_task_listing(&state)
                )),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// task_delete
// ---------------------------------------------------------------------------

pub struct TaskDeleteTool {
    boards: TaskBoards,
}

#[async_trait]
impl Tool for TaskDeleteTool {
    fn name(&self) -> &str {
        "task_delete"
    }

    fn description(&self) -> &str {
        "Delete a task and all its sub-tasks."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task ID to delete"
                }
            },
            "required": ["task_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(
        &self,
        args: Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'task_id'"))?;

        let board = self.boards.board(&session.id);
        let mut state = board.write().await;

        if state.find_task(task_id).is_none() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "task not found: {}.{}",
                    task_id,
                    format_unknown_task_listing(&state)
                )),
            });
        }

        // Collect all ids to delete (self + all descendants)
        let ids_to_remove = state.collect_descendant_ids(task_id);
        let count = ids_to_remove.len();

        state.tasks.retain(|t| !ids_to_remove.contains(&t.id));
        state.save();

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
}

// ---------------------------------------------------------------------------
// Helpers for daemon registration
// ---------------------------------------------------------------------------

/// Build the four task tools over per-session boards rooted at
/// `sessions_root` (P1-B1: each session gets `{sessions_root}/{uuid}/tasks.json`).
pub fn new_tools(boards: TaskBoards) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(TaskCreateTool { boards: boards.clone() }),
        Arc::new(TaskListTool { boards: boards.clone() }),
        Arc::new(TaskUpdateTool { boards: boards.clone() }),
        Arc::new(TaskDeleteTool { boards }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(id: &str) -> crate::agents::session::Session {
        crate::agents::session::Session::new(id.to_string())
    }

    fn boards(dir: &tempfile::TempDir) -> TaskBoards {
        TaskBoards::new(dir.path().join("sessions"), DEFAULT_NAMESPACE)
    }

    #[tokio::test]
    async fn test_batch_create() {
        let dir = tempfile::tempdir().unwrap();
        let tool = TaskCreateTool { boards: boards(&dir) };

        let result = tool
            .execute(
                json!({
                    "subject": ["Goal A", "Goal B", "Goal C"]
                }),
                &make_session("test"),
            )
            .await
            .unwrap();

        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(output["ok"].as_bool().unwrap());
        assert_eq!(output["count"].as_u64().unwrap(), 3);
    }

    #[tokio::test]
    async fn test_batch_create_subtasks() {
        let dir = tempfile::tempdir().unwrap();
        let create = TaskCreateTool { boards: boards(&dir) };

        // Create a goal
        let goal = create
            .execute(json!({"subject": "My Goal"}), &make_session("test"))
            .await
            .unwrap();
        let goal_output: Value = serde_json::from_str(&goal.output).unwrap();
        let goal_id = goal_output["task_id"].as_str().unwrap();

        // Batch create sub-tasks
        let result = create
            .execute(
                json!({
                    "subject": ["Task 1", "Task 2"],
                    "parent": goal_id
                }),
                &make_session("test"),
            )
            .await
            .unwrap();

        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(output["ok"].as_bool().unwrap());
        assert_eq!(output["count"].as_u64().unwrap(), 2);
    }

    #[tokio::test]
    async fn test_update_requires_status() {
        let dir = tempfile::tempdir().unwrap();
        let create = TaskCreateTool { boards: boards(&dir) };
        let _goal = create
            .execute(json!({"subject": "Goal"}), &make_session("test"))
            .await
            .unwrap();

        // Update without status should error
        let update = TaskUpdateTool { boards: boards(&dir) };
        let result = update
            .execute(
                json!({"task_id": "myclaw/t/legacy"}),
                &make_session("test"),
            )
            .await;

        assert!(result.is_err());
    }

    /// issue #132 (P1): an unknown task_id on task_update comes back with a
    /// listing of what's actually on this session's board, so a copied or
    /// hallucinated id can self-correct in one step.
    #[tokio::test]
    async fn test_update_unknown_task_id_lists_board() {
        let dir = tempfile::tempdir().unwrap();
        let create = TaskCreateTool { boards: boards(&dir) };
        create
            .execute(json!({"subject": "Real Goal"}), &make_session("test"))
            .await
            .unwrap();

        let update = TaskUpdateTool { boards: boards(&dir) };
        let result = update
            .execute(
                json!({"task_id": "myclaw/t/does-not-exist", "status": "completed"}),
                &make_session("test"),
            )
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("task not found: myclaw/t/does-not-exist"));
        assert!(err.contains("Real Goal"), "listing must show what does exist: {err}");
    }

    /// issue #132 (P1): same self-correction on task_delete.
    #[tokio::test]
    async fn test_delete_unknown_task_id_lists_board() {
        let dir = tempfile::tempdir().unwrap();
        let create = TaskCreateTool { boards: boards(&dir) };
        create
            .execute(json!({"subject": "Real Goal"}), &make_session("test"))
            .await
            .unwrap();

        let delete = TaskDeleteTool { boards: boards(&dir) };
        let result = delete
            .execute(
                json!({"task_id": "myclaw/t/does-not-exist"}),
                &make_session("test"),
            )
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("task not found: myclaw/t/does-not-exist"));
        assert!(err.contains("Real Goal"));
    }

    /// issue #132 (P1): task_create's parent-not-found gets the same
    /// treatment — the parent id is just as copyable/hallucinatable as any
    /// other task id.
    #[tokio::test]
    async fn test_create_unknown_parent_lists_board() {
        let dir = tempfile::tempdir().unwrap();
        let create = TaskCreateTool { boards: boards(&dir) };
        create
            .execute(json!({"subject": "Real Goal"}), &make_session("test"))
            .await
            .unwrap();

        let result = create
            .execute(
                json!({"subject": "Orphan", "parent": "myclaw/t/does-not-exist"}),
                &make_session("test"),
            )
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("parent task not found: myclaw/t/does-not-exist"));
        assert!(err.contains("Real Goal"));
    }

    /// issue #132 (P1): an empty board says so plainly instead of an empty list.
    #[tokio::test]
    async fn test_unknown_task_id_on_empty_board_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let update = TaskUpdateTool { boards: boards(&dir) };
        let result = update
            .execute(
                json!({"task_id": "myclaw/t/does-not-exist", "status": "completed"}),
                &make_session("test"),
            )
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("empty"), "empty board must say so: {err}");
    }

    /// issue #132 (P1): a long board caps the listing instead of dumping
    /// every task into the error (mirrors #130's cap-at-20 for shell procs).
    #[tokio::test]
    async fn test_unknown_task_listing_caps_long_lists() {
        let dir = tempfile::tempdir().unwrap();
        let create = TaskCreateTool { boards: boards(&dir) };
        let subjects: Vec<String> = (0..25).map(|i| format!("Goal {i}")).collect();
        create
            .execute(json!({"subject": subjects}), &make_session("test"))
            .await
            .unwrap();

        let update = TaskUpdateTool { boards: boards(&dir) };
        let result = update
            .execute(
                json!({"task_id": "myclaw/t/does-not-exist", "status": "completed"}),
                &make_session("test"),
            )
            .await
            .unwrap();

        let err = result.error.unwrap();
        assert!(err.contains("and 5 more"), "expected 25 tasks capped at 20: {err}");
        // Reviewer note (PR #133): the cap must keep the newest tasks, not
        // the oldest — a copied/hallucinated id is most likely to reference
        // something just created, and that's exactly what the oldest-first
        // cap used to drop first.
        assert!(err.contains("Goal 24"), "newest task must survive the cap: {err}");
        assert!(!err.contains("Goal 0\""), "oldest task must be the one dropped: {err}");
    }

    /// P1-B1: task boards are per-session — session A's tasks are invisible
    /// to session B, and each board lives under its own session directory.
    #[tokio::test]
    async fn test_boards_are_isolated_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let b = boards(&dir);
        let session_a = make_session("019fe342-6a03-7561-86de-0c2327a8c3de");
        let session_b = make_session("019fe342-6a03-7561-86de-0c2327a8ffff");

        // Session A creates a goal.
        let create = TaskCreateTool { boards: b.clone() };
        let goal = create
            .execute(json!({"subject": "A's Goal"}), &session_a)
            .await
            .unwrap();
        assert!(goal.success);
        let goal_id: Value = serde_json::from_str(&goal.output).unwrap();
        let goal_id = goal_id["task_id"].as_str().unwrap().to_string();

        // Session B's board is empty — and cannot see/update A's task.
        let list = TaskListTool { boards: b.clone() };
        let b_view = list
            .execute(json!({}), &session_b)
            .await
            .unwrap();
        let b_out: Value = serde_json::from_str(&b_view.output).unwrap();
        assert_eq!(b_out["total"].as_u64().unwrap(), 0);
        assert!(b_out["tasks"].as_array().unwrap().is_empty());

        let update = TaskUpdateTool { boards: b.clone() };
        let cross = update
            .execute(json!({"task_id": goal_id, "status": "completed"}), &session_b)
            .await
            .unwrap();
        assert!(!cross.success, "session B must not mutate A's tasks");

        // Session A still sees its pending goal.
        let a_view = list.execute(json!({}), &session_a).await.unwrap();
        let a_out: Value = serde_json::from_str(&a_view.output).unwrap();
        assert_eq!(a_out["total"].as_u64().unwrap(), 1);

        // Files landed under per-session directories.
        assert!(dir
            .path()
            .join("sessions")
            .join("019fe342-6a03-7561-86de-0c2327a8c3de")
            .join("tasks.json")
            .is_file());
        assert!(!dir
            .path()
            .join("sessions")
            .join("019fe342-6a03-7561-86de-0c2327a8ffff")
            .join("tasks.json")
            .exists());
    }

    #[tokio::test]
    async fn test_persist_survives_daemon_restart() {
        let dir = tempfile::tempdir().unwrap();
        let b = boards(&dir);
        let session = make_session("019fe342-6a03-7561-86de-0c2327a8c3de");
        let path = b.board_path(&session.id);

        // "Daemon A": create a goal and a child task — both must hit disk.
        let create = TaskCreateTool { boards: b.clone() };
        let goal = create
            .execute(json!({"subject": "Persisted Goal"}), &session)
            .await
            .unwrap();
        assert!(goal.success);
        let goal_output: Value = serde_json::from_str(&goal.output).unwrap();
        let goal_id = goal_output["task_id"].as_str().unwrap().to_string();

        let child = create
            .execute(
                json!({"subject": "Persisted Child", "parent": goal_id}),
                &session,
            )
            .await
            .unwrap();
        assert!(child.success);

        // "Daemon B" (restart): fresh state loaded from disk.
        let reloaded = TaskState::load(&path, DEFAULT_NAMESPACE);
        assert_eq!(reloaded.tasks.len(), 2);
        assert_eq!(reloaded.tasks[0].subject, "Persisted Goal");
        assert_eq!(reloaded.tasks[0].status, "pending");
        assert!(
            reloaded.tasks.iter().all(|t| t.id.starts_with("myclaw/t/")),
            "task ids must be FQIDs after restart: {:?}",
            reloaded.tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
        assert_ne!(
            reloaded.tasks[0].id, reloaded.tasks[1].id,
            "fresh uuidv7 ids must not collide"
        );

        // Update through a fresh tool instance persists as well.
        let updater = TaskUpdateTool { boards: b.clone() };
        let updated = updater
            .execute(
                json!({"task_id": goal_id, "status": "completed"}),
                &session,
            )
            .await
            .unwrap();
        assert!(updated.success);
        let reloaded_after_update = TaskState::load(&path, DEFAULT_NAMESPACE);
        assert_eq!(reloaded_after_update.tasks[0].status, "completed");

        // Delete persists too (goal + descendant child).
        let deleter = TaskDeleteTool { boards: b.clone() };
        let deleted = deleter
            .execute(json!({"task_id": goal_id}), &session)
            .await
            .unwrap();
        assert!(deleted.success);
        assert!(TaskState::load(&path, DEFAULT_NAMESPACE).tasks.is_empty());
    }

    #[test]
    fn test_load_defaults_when_file_missing_or_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file → defaults.
        let state = TaskState::load(&dir.path().join("nope.json"), DEFAULT_NAMESPACE);
        assert!(state.tasks.is_empty());
        assert_eq!(state.namespace, DEFAULT_NAMESPACE);

        // Corrupt content → defaults.
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        let state = TaskState::load(&bad, DEFAULT_NAMESPACE);
        assert!(state.tasks.is_empty());
        assert_eq!(state.namespace, DEFAULT_NAMESPACE);
    }
}
