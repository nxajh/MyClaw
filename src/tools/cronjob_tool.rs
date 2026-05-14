//! CronJobTool — LLM 通过此工具管理定时任务。
//!
//! 支持 create / list / pause / resume / run / remove 六个 action。

use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::agents::scheduling::cron_store::{CronStore, JobEntry, JobUpdate};
use crate::providers::{Tool, ToolResult};

/// Shared reference to the cron store (same instance used by the scheduler).
pub type SharedCronStore = Arc<RwLock<CronStore>>;

pub struct CronJobTool {
    store: SharedCronStore,
}

impl CronJobTool {
    pub fn new(store: SharedCronStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for CronJobTool {
    fn name(&self) -> &str {
        "cronjob"
    }

    fn description(&self) -> &str {
        "Manage scheduled cron jobs. Actions: create, list, pause, resume, run, remove."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "pause", "resume", "run", "remove"],
                    "description": "The operation to perform."
                },
                "id": {
                    "type": "string",
                    "description": "Job ID (required for pause, resume, run, remove)."
                },
                "schedule": {
                    "type": "string",
                    "description": "Cron expression, 6-field: 'sec min hour day month weekday'. Example: '0 0 9 * * *' for daily at 09:00."
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt to send to the agent when the job fires."
                },
                "target": {
                    "type": "string",
                    "description": "Where to deliver output: 'last', 'none', or channel name (e.g. 'telegram', 'qqbot'). Default: 'last'."
                },
                "name": {
                    "type": "string",
                    "description": "Optional friendly name for the job."
                },
                "active_hours": {
                    "type": "string",
                    "description": "Active hours restriction, e.g. '08:00-24:00'. Omit for always active."
                },
                "tz": {
                    "type": "string",
                    "description": "Per-job IANA timezone (e.g. 'Asia/Shanghai'). Overrides global timezone."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args.get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match action {
            "create" => self.handle_create(&args),
            "list" => self.handle_list(),
            "pause" => self.handle_set_enabled(&args, false),
            "resume" => self.handle_set_enabled(&args, true),
            "run" => self.handle_run(&args),
            "remove" => self.handle_remove(&args),
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown action '{}'. Use: create, list, pause, resume, run, remove", action)),
            }),
        }
    }
}

impl CronJobTool {
    fn handle_create(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let schedule = match args.get("schedule").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return Ok(err_result("Missing required field: schedule")),
        };
        let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return Ok(err_result("Missing required field: prompt")),
        };
        let target = args.get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("last")
            .to_string();
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let active_hours = args.get("active_hours").and_then(|v| v.as_str()).map(|s| s.to_string());

        // Validate cron expression.
        if schedule.parse::<cron::Schedule>().is_err() {
            return Ok(err_result(&format!(
                "Invalid cron expression '{}'. Use 6-field format: sec min hour day month weekday",
                schedule
            )));
        }

        let tz = args.get("tz").and_then(|v| v.as_str()).map(|s| s.to_string());

        let entry = JobEntry {
            id: String::new(), // auto-generated
            schedule: schedule.clone(),
            prompt,
            target,
            name: name.clone(),
            tz,
            active_hours,
            enabled: true,
            last_run_at: None,
            next_run_at: None,
            created_at: None,
        };

        let mut store = self.store.write().unwrap();
        match store.add_job(entry) {
            Ok(id) => Ok(ToolResult {
                success: true,
                output: format!("Created cron job '{}' (id: {}, schedule: {})", name.as_deref().unwrap_or("unnamed"), id, schedule),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to create job: {}", e)),
            }),
        }
    }

    fn handle_list(&self) -> anyhow::Result<ToolResult> {
        let store = self.store.read().unwrap();
        let jobs = store.jobs();

        if jobs.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No cron jobs configured.".to_string(),
                error: None,
            });
        }

        let mut lines = Vec::new();
        for job in jobs {
            let status = if job.enabled { "✅" } else { "⏸️" };
            let name = job.name.as_deref().unwrap_or(&job.id);
            let next = job.next_run_at.as_deref().unwrap_or("none");
            let last = job.last_run_at.as_deref().unwrap_or("never");
            lines.push(format!(
                "{} [{}] {} — schedule: \"{}\", target: {}, next: {}, last: {}",
                status, job.id, name, job.schedule, job.target, next, last
            ));
        }

        Ok(ToolResult {
            success: true,
            output: lines.join("\n"),
            error: None,
        })
    }

    fn handle_set_enabled(&self, args: &serde_json::Value, enabled: bool) -> anyhow::Result<ToolResult> {
        let id = match args.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(err_result("Missing required field: id")),
        };

        let action_name = if enabled { "resumed" } else { "paused" };

        let mut store = self.store.write().unwrap();
        match store.update_job(&id, JobUpdate {
            name: None, schedule: None, prompt: None, target: None,
            tz: None, active_hours: None, enabled: Some(enabled),
        }) {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("Job {} {}.", id, action_name),
                error: None,
            }),
            Ok(false) => Ok(err_result(&format!("Job '{}' not found.", id))),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to {} job: {}", action_name, e)),
            }),
        }
    }

    fn handle_run(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let id = match args.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(err_result("Missing required field: id")),
        };

        let store = self.store.read().unwrap();
        let job = store.jobs().iter().find(|j| j.id == id);
        match job {
            Some(job) => Ok(ToolResult {
                success: true,
                output: format!(
                    "RUN_IMMEDIATE:{}\nschedule: {}\nprompt: {}\ntarget: {}",
                    id, job.schedule, job.prompt, job.target
                ),
                error: None,
            }),
            None => Ok(err_result(&format!("Job '{}' not found.", id))),
        }
    }

    fn handle_remove(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let id = match args.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(err_result("Missing required field: id")),
        };

        let mut store = self.store.write().unwrap();
        match store.remove_job(&id) {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("Job '{}' removed.", id),
                error: None,
            }),
            Ok(false) => Ok(err_result(&format!("Job '{}' not found.", id))),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to remove job: {}", e)),
            }),
        }
    }
}

fn err_result(msg: &str) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(msg.to_string()),
    }
}
