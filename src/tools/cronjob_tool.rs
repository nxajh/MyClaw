//! CronJobTool — LLM 通过此工具管理定时任务。
//!
//! 支持 create / list / pause / resume / run / remove / log 七个 action。

use async_trait::async_trait;

use crate::agents::SharedScheduler;
use crate::agents::scheduling::cron_types::{DeliveryConfig, ScheduleKind};
use crate::agents::scheduling::scheduler::JobEntry;
use crate::providers::{Tool, ToolResult};

pub struct CronJobTool {
    scheduler: SharedScheduler,
}

impl CronJobTool {
    pub fn new(scheduler: SharedScheduler) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for CronJobTool {
    fn name(&self) -> &str {
        "cronjob"
    }

    fn description(&self) -> &str {
        "Manage scheduled cron jobs. Actions: create, list, pause, resume, run, remove, log."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "pause", "resume", "run", "remove", "log"],
                    "description": "The operation to perform."
                },
                "id": {
                    "type": "string",
                    "description": "Job ID (required for pause, resume, run, remove, log)."
                },
                "schedule": {
                    "type": "string",
                    "description": "Schedule: cron expression 'sec min hour day month weekday', or 'every 30m', or 'at 2026-05-15T09:00:00+08:00'."
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt to send to the agent when the job fires."
                },
                "target": {
                    "type": "string",
                    "description": "Where to deliver output: 'last', 'none', or channel name. Default: 'last'."
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
                },
                "delivery": {
                    "type": "object",
                    "description": "Delivery config: { channel, to?, thread_id? }",
                    "properties": {
                        "channel": { "type": "string" },
                        "to": { "type": "string" },
                        "thread_id": { "type": "string" }
                    }
                },
                "enabled_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool whitelist for this job (overrides disabled_tools)."
                },
                "disabled_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool blacklist for this job."
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
            "log" => self.handle_log(&args),
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown action '{}'. Use: create, list, pause, resume, run, remove, log", action)),
            }),
        }
    }
}

impl CronJobTool {
    fn handle_create(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let schedule_input = match args.get("schedule").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return Ok(err_result("Missing required field: schedule")),
        };
        let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return Ok(err_result("Missing required field: prompt")),
        };

        // Prompt injection scan.
        if let Err(e) = crate::agents::scheduling::scheduler::scan_prompt_injection(&prompt) {
            return Ok(err_result(&format!("Prompt injection detected: {}", e)));
        }

        let target = args.get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("last")
            .to_string();
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let active_hours = args.get("active_hours").and_then(|v| v.as_str()).map(|s| s.to_string());
        let tz = args.get("tz").and_then(|v| v.as_str()).map(|s| s.to_string());

        // Parse schedule: supports cron expressions, "every 30m", "at <ISO>".
        let (schedule, schedule_kind) = parse_schedule_input(&schedule_input)
            .map_err(|e| anyhow::anyhow!(e))?;

        // Parse delivery config.
        let delivery = args.get("delivery").and_then(|v| {
            let channel = v.get("channel")?.as_str()?;
            Some(DeliveryConfig {
                channel: channel.to_string(),
                account_id: v.get("account_id").and_then(|a| a.as_str()).map(|s| s.to_string()),
                to: v.get("to").and_then(|t| t.as_str()).map(|s| s.to_string()),
                thread_id: v.get("thread_id").and_then(|t| t.as_str()).map(|s| s.to_string()),
            })
        });

        // Parse tool filters.
        let enabled_tools = parse_string_array(args.get("enabled_tools"));
        let disabled_tools = parse_string_array(args.get("disabled_tools"));

        let entry = JobEntry {
            id: String::new(),
            schedule: schedule.clone(),
            prompt,
            target,
            name: name.clone(),
            tz,
            active_hours,
            delivery,
            enabled_tools,
            disabled_tools,
            schedule_kind,
            enabled: true,
            last_run_at: None,
            next_run_at: None,
            created_at: None,
            last_runs: Vec::new(),
        };

        match self.scheduler.add_job(entry) {
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
        let jobs = self.scheduler.jobs();

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
            let delivery_info = match &job.delivery {
                Some(d) => format!(", delivery: {}→{}", d.channel, d.to.as_deref().unwrap_or("*")),
                None => String::new(),
            };
            let tool_info = match (&job.enabled_tools, &job.disabled_tools) {
                (Some(whitelist), _) => format!(", tools: whitelist({})", whitelist.len()),
                (None, Some(blacklist)) => format!(", tools: blacklist({})", blacklist.len()),
                _ => String::new(),
            };
            let runs_info = if job.last_runs.is_empty() {
                String::new()
            } else {
                let last_status = job.last_runs.last().map(|r| r.status.as_str()).unwrap_or("?");
                format!(", last_run: {}", last_status)
            };
            lines.push(format!(
                "{} [{}] {} — schedule: \"{}\", target: {}, next: {}, last: {}{}{}{}",
                status, job.id, name, job.schedule, job.target, next, last,
                delivery_info, tool_info, runs_info,
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

        match self.scheduler.set_enabled(&id, enabled) {
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

        let jobs = self.scheduler.jobs();
        let job = jobs.iter().find(|j| j.id == id);
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

        match self.scheduler.remove_job(&id) {
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

    fn handle_log(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let id = match args.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(err_result("Missing required field: id")),
        };

        let jobs = self.scheduler.jobs();
        let job = jobs.iter().find(|j| j.id == id);
        match job {
            Some(j) if j.last_runs.is_empty() => Ok(ToolResult {
                success: true,
                output: format!("Job '{}' has no run history.", j.name.as_deref().unwrap_or(&j.id)),
                error: None,
            }),
            Some(j) => {
                let mut output = format!("📋 Run log for '{}':\n\n", j.name.as_deref().unwrap_or(&j.id));
                for (i, run) in j.last_runs.iter().enumerate().rev() {
                    let error_info = if let Some(ref e) = run.error {
                        format!(" — {}", e)
                    } else {
                        String::new()
                    };
                    output.push_str(&format!(
                        "{}. [{}] {} — {}ms{}\n",
                        i + 1,
                        &run.run_at[..19.min(run.run_at.len())],
                        run.status.as_str(),
                        run.duration_ms,
                        error_info,
                    ));
                }
                Ok(ToolResult { success: true, output, error: None })
            }
            None => Ok(err_result(&format!("Job '{}' not found", id))),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn err_result(msg: &str) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(msg.to_string()),
    }
}

/// Parse schedule input: cron expression, "every 30m", or "at <ISO>".
fn parse_schedule_input(input: &str) -> Result<(String, Option<ScheduleKind>), String> {
    let trimmed = input.trim();

    // "every 30m" / "every 1h" / "every 90s"
    if let Some(rest) = trimmed.strip_prefix("every ") {
        let ms = parse_duration_to_ms(rest)?;
        return Ok((trimmed.to_string(), Some(ScheduleKind::Every { interval_ms: ms })));
    }

    // "at 2026-05-15T09:00:00+08:00"
    if let Some(rest) = trimmed.strip_prefix("at ") {
        chrono::DateTime::parse_from_rfc3339(rest)
            .map_err(|e| format!("invalid datetime '{}': {}", rest, e))?;
        return Ok((trimmed.to_string(), Some(ScheduleKind::At { at: rest.to_string() })));
    }

    // Standard cron expression (6-field).
    trimmed.parse::<cron::Schedule>()
        .map_err(|e| format!("invalid cron expression '{}': {}", trimmed, e))?;
    Ok((trimmed.to_string(), None))
}

/// Parse duration string to milliseconds.
fn parse_duration_to_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        n.parse::<u64>().map_err(|_| format!("invalid ms value: '{}'", s))
    } else if let Some(n) = s.strip_suffix("s") {
        n.parse::<u64>().map(|v| v * 1000).map_err(|_| format!("invalid seconds: '{}'", s))
    } else if let Some(n) = s.strip_suffix("m") {
        n.parse::<u64>().map(|v| v * 60_000).map_err(|_| format!("invalid minutes: '{}'", s))
    } else if let Some(n) = s.strip_suffix("h") {
        n.parse::<u64>().map(|v| v * 3_600_000).map_err(|_| format!("invalid hours: '{}'", s))
    } else {
        Err(format!("expected duration like '30s', '5m', '1h', got: '{}'", s))
    }
}

/// Parse a JSON array of strings.
fn parse_string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    value.and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
}
