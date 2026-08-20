//! CronJobTool — manage scheduled jobs from the LLM.
//!
//! Supports create / update / list / pause / resume / run / remove / log actions.
//! Includes schedule validation, retry strategy, failure alerts, per-job model/provider settings.

use async_trait::async_trait;

use crate::agents::SharedScheduler;
use crate::agents::scheduling::cron_types::{
    DeliveryConfig, FailureAlertConfig, RetryConfig, ScheduleKind,
};
use crate::agents::scheduling::scheduler::{
    self, JobEntry, validate_active_hours, validate_at_timestamp, validate_schedule, validate_tz,
};
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
        "Manage scheduled cron jobs. \
         Actions: 'create' (new job), 'update' (patch existing job fields), \
         'list' (show all jobs), 'pause' / 'resume' (toggle enabled), \
         'run' (trigger immediate execution), 'remove' (delete job), \
         'log' (view run history from persistent log file). \
         Supports cron expressions, fixed intervals ('every 30m'), \
         one-shot ('at 2026-05-15T09:00:00+08:00'), retry policies, \
         failure alerts, max run limits, per-job model/provider override."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "list", "pause", "resume", "run", "remove", "log"],
                    "description": "The operation to perform."
                },
                "id": {
                    "type": "string",
                    "description": "Job ID (required for update, pause, resume, run, remove, log)."
                },
                "schedule": {
                    "type": "string",
                    "description": "Schedule: cron expression 'sec min hour day month weekday' (Unix-style: 0=Sunday) (e.g. '0 0 9 * * *'), or 'every 30m', or 'at 2026-05-15T09:00:00+08:00'."
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
                    "description": "Delivery config: { channel, account_id?, to?, thread_id? }",
                    "properties": {
                        "channel": { "type": "string" },
                        "account_id": { "type": "string", "description": "Account ID for multi-instance channels." },
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
                },
                "max_runs": {
                    "type": "integer",
                    "description": "Auto-disable/delete after N completed runs. Omit for unlimited."
                },
                "delete_after_run": {
                    "type": "boolean",
                    "description": "If true, auto-delete the job when max_runs is reached (default: false = just disable)."
                },
                "model": {
                    "type": "string",
                    "description": "Per-job model override (e.g. 'claude-sonnet-4-20250514')."
                },
                "provider": {
                    "type": "string",
                    "description": "Per-job provider override (e.g. 'anthropic', 'openai')."
                },
                "retry": {
                    "type": "object",
                    "description": "Retry policy for transient errors.",
                    "properties": {
                        "max_attempts": { "type": "integer", "description": "Max retries (default: 3)." },
                        "backoff_ms": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Backoff delays in ms per retry (default: [30000, 60000, 300000])."
                        }
                    }
                },
                "failure_alert": {
                    "type": "object",
                    "description": "Alert configuration for consecutive failures.",
                    "properties": {
                        "after": { "type": "integer", "description": "Alert after N consecutive failures (default: 3)." },
                        "cooldown_secs": { "type": "integer", "description": "Min seconds between repeated alerts (default: 3600)." },
                        "include_skipped": { "type": "boolean", "description": "Count skipped runs as failures (default: false)." }
                    }
                },
                "limit": {
                    "type": "integer",
                    "description": "For 'list': max jobs to show (default: 20). For 'log': max log entries (default: 10)."
                },
                "offset": {
                    "type": "integer",
                    "description": "For 'list': skip first N jobs (default: 0)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        match action {
            "create" => self.handle_create(&args),
            "update" => self.handle_update(&args),
            "list" => self.handle_list(&args),
            "pause" => self.handle_set_enabled(&args, false),
            "resume" => self.handle_set_enabled(&args, true),
            "run" => self.handle_run(&args),
            "remove" => self.handle_remove(&args),
            "log" => self.handle_log(&args),
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{}'. Use: create, update, list, pause, resume, run, remove, log",
                    action
                )),
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
        if let Err(e) = scheduler::scan_prompt_injection(&prompt) {
            return Ok(err_result(&format!("Prompt rejected: {}", e)));
        }

        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("last")
            .to_string();
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let active_hours = args
            .get("active_hours")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tz = args
            .get("tz")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let model = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let provider = args
            .get("provider")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let max_runs = args
            .get("max_runs")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        let delete_after_run = args
            .get("delete_after_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Validate schedule.
        let (schedule, schedule_kind) = match parse_schedule_input(&schedule_input) {
            Ok(v) => v,
            Err(e) => return Ok(err_result(&e)),
        };

        // Validate one-shot timestamps are in the future.
        if let Some(ScheduleKind::At { ref at }) = schedule_kind {
            if let Err(e) = validate_at_timestamp(at) {
                return Ok(err_result(&e));
            }
        }

        // Validate timezone.
        if let Some(ref tz_str) = tz {
            if let Err(e) = validate_tz(tz_str) {
                return Ok(err_result(&e));
            }
        }

        // Validate active_hours.
        if let Some(ref hours) = active_hours {
            if let Err(e) = validate_active_hours(hours) {
                return Ok(err_result(&e));
            }
        }

        // Parse delivery config.
        let delivery = parse_delivery(args.get("delivery"));

        // Parse tool filters.
        let enabled_tools = parse_string_array(args.get("enabled_tools"));
        let disabled_tools = parse_string_array(args.get("disabled_tools"));

        // Parse retry policy.
        let retry = args.get("retry").and_then(parse_retry_config);

        // Parse failure alert config.
        let failure_alert = args.get("failure_alert").and_then(parse_failure_alert);

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
            retry,
            failure_alert,
            consecutive_errors: 0,
            consecutive_skipped: 0,
            max_runs,
            completed_runs: 0,
            delete_after_run,
            model: model.clone(),
            provider: provider.clone(),
            last_failure_alert_at: None,
            context_policy: crate::config::scheduler::ContextPolicy::Inject,
        };

        match self.scheduler.add_job(entry) {
            Ok(id) => {
                let mut details = vec![
                    format!(
                        "Created cron job '{}' (id: {})",
                        name.as_deref().unwrap_or("unnamed"),
                        id
                    ),
                    format!("  schedule: {}", schedule),
                ];
                if let Some(ref m) = model {
                    details.push(format!("  model: {}", m));
                }
                if let Some(ref p) = provider {
                    details.push(format!("  provider: {}", p));
                }
                if let Some(mr) = max_runs {
                    details.push(format!(
                        "  max_runs: {}{}",
                        mr,
                        if delete_after_run {
                            " (auto-delete)"
                        } else {
                            " (auto-disable)"
                        }
                    ));
                }
                Ok(ToolResult {
                    success: true,
                    output: details.join("\n"),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to create job: {}", e)),
            }),
        }
    }

    fn handle_update(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let id = match args.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(err_result("Missing required field: id")),
        };

        let mut update = scheduler::JobUpdate::default();

        if let Some(v) = args.get("name").and_then(|v| v.as_str()) {
            update.name = Some(v.to_string());
        }
        if let Some(v) = args.get("prompt").and_then(|v| v.as_str()) {
            // Prompt injection scan.
            if let Err(e) = scheduler::scan_prompt_injection(v) {
                return Ok(err_result(&format!("Prompt rejected: {}", e)));
            }
            update.prompt = Some(v.to_string());
        }
        if let Some(v) = args.get("target").and_then(|v| v.as_str()) {
            update.target = Some(v.to_string());
        }
        if let Some(v) = args.get("active_hours").and_then(|v| v.as_str()) {
            if let Err(e) = validate_active_hours(v) {
                return Ok(err_result(&e));
            }
            update.active_hours = Some(v.to_string());
        }
        if let Some(v) = args.get("tz").and_then(|v| v.as_str()) {
            if let Err(e) = validate_tz(v) {
                return Ok(err_result(&e));
            }
            update.tz = Some(v.to_string());
        }
        if let Some(v) = args.get("enabled").and_then(|v| v.as_bool()) {
            update.enabled = Some(v);
        }
        if let Some(v) = args.get("delivery") {
            update.delivery = parse_delivery(Some(v));
        }
        if let Some(v) = args.get("enabled_tools") {
            update.enabled_tools = parse_string_array(Some(v));
        }
        if let Some(v) = args.get("disabled_tools") {
            update.disabled_tools = parse_string_array(Some(v));
        }
        if let Some(v) = args.get("retry") {
            update.retry = parse_retry_config(v);
        }
        if let Some(v) = args.get("failure_alert") {
            update.failure_alert = parse_failure_alert(v);
        }
        if let Some(v) = args.get("max_runs").and_then(|v| v.as_u64()) {
            update.max_runs = Some(Some(v as u32));
        }
        if let Some(v) = args.get("delete_after_run").and_then(|v| v.as_bool()) {
            update.delete_after_run = Some(v);
        }
        if let Some(v) = args.get("model").and_then(|v| v.as_str()) {
            update.model = Some(Some(v.to_string()));
        }
        if let Some(v) = args.get("provider").and_then(|v| v.as_str()) {
            update.provider = Some(Some(v.to_string()));
        }

        // If schedule is being updated, validate it.
        if let Some(schedule_input) = args.get("schedule").and_then(|v| v.as_str()) {
            let (schedule, _kind) = match parse_schedule_input(schedule_input) {
                Ok(v) => v,
                Err(e) => return Ok(err_result(&e)),
            };
            update.schedule = Some(schedule);
            // Note: schedule_kind update is not supported via tool —
            // user should remove and recreate to change schedule kind.
        }

        // Check that at least one field is being updated.
        let has_update = update.name.is_some()
            || update.schedule.is_some()
            || update.prompt.is_some()
            || update.target.is_some()
            || update.tz.is_some()
            || update.active_hours.is_some()
            || update.enabled.is_some()
            || update.delivery.is_some()
            || update.enabled_tools.is_some()
            || update.disabled_tools.is_some()
            || update.retry.is_some()
            || update.failure_alert.is_some()
            || update.max_runs.is_some()
            || update.delete_after_run.is_some()
            || update.model.is_some()
            || update.provider.is_some();

        if !has_update {
            return Ok(err_result(
                "No fields to update. Provide at least one field besides 'id' and 'action'.",
            ));
        }

        match self.scheduler.update_job(&id, update) {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("Job '{}' updated.", id),
                error: None,
            }),
            Ok(false) => Ok(err_result(&format!("Job '{}' not found.", id))),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to update job: {}", e)),
            }),
        }
    }

    fn handle_list(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let jobs = self.scheduler.jobs();

        if jobs.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No cron jobs configured.".to_string(),
                error: None,
            });
        }

        let total = jobs.len();
        let page: Vec<&JobEntry> = jobs.iter().skip(offset).take(limit).collect();

        let mut lines = Vec::new();
        if offset > 0 || total > limit {
            lines.push(format!(
                "Showing {}-{} of {} jobs",
                offset + 1,
                offset + page.len(),
                total
            ));
            lines.push(String::new());
        }

        for job in page {
            let status = if job.enabled { "✅" } else { "⏸️" };
            let name = job.name.as_deref().unwrap_or(&job.id);
            let next = job.next_run_at.as_deref().unwrap_or("none");
            let last = job.last_run_at.as_deref().unwrap_or("never");
            let delivery_info = match &job.delivery {
                Some(d) => format!(
                    ", delivery: {}→{}",
                    d.channel,
                    d.to.as_deref().unwrap_or("*")
                ),
                None => String::new(),
            };
            let tool_info = match (&job.enabled_tools, &job.disabled_tools) {
                (Some(whitelist), _) => format!(", tools: whitelist({})", whitelist.len()),
                (None, Some(blacklist)) => format!(", tools: blacklist({})", blacklist.len()),
                _ => String::new(),
            };
            let runs_info = {
                // Single source: the per-job run log JSONL (last record).
                let last = self.scheduler.read_run_log(&job.id, 1);
                match last.first() {
                    Some(r) => format!(", last_run: {}", r.status.as_str()),
                    None => String::new(),
                }
            };
            let model_info = match &job.model {
                Some(m) => format!(", model: {}", m),
                None => String::new(),
            };
            let retry_info = match &job.retry {
                Some(r) => format!(", retry: max {}", r.max_attempts),
                None => String::new(),
            };
            let max_runs_info = match job.max_runs {
                Some(mr) => format!(", progress: {}/{}", job.completed_runs, mr),
                None => String::new(),
            };
            let fail_info = if job.consecutive_errors > 0 {
                format!(", ⚠️ {} consecutive errors", job.consecutive_errors)
            } else {
                String::new()
            };
            lines.push(format!(
                "{status} [{id}] {name} — schedule: {schedule:?}, target: {target}, next: {next}, last: {last}{delivery}{tools}{runs}{model}{retry}{max_runs}{fail}",
                status = status,
                id = job.id,
                name = name,
                schedule = job.schedule,
                target = job.target,
                next = next,
                last = last,
                delivery = delivery_info,
                tools = tool_info,
                runs = runs_info,
                model = model_info,
                retry = retry_info,
                max_runs = max_runs_info,
                fail = fail_info,
            ));
        }

        Ok(ToolResult {
            success: true,
            output: lines.join("\n"),
            error: None,
        })
    }

    fn handle_set_enabled(
        &self,
        args: &serde_json::Value,
        enabled: bool,
    ) -> anyhow::Result<ToolResult> {
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

        // Snapshot for pre-flight checks and reporting.
        let jobs = self.scheduler.jobs();
        let job = match jobs.iter().find(|j| j.id == id) {
            Some(j) => j.clone(),
            None => return Ok(err_result(&format!("Job '{}' not found.", id))),
        };
        let name = job.name.clone().unwrap_or_else(|| job.id.clone());

        // Force the job due now; the next scheduler tick (≤30s) fires it
        // through the regular execution path (run accounting, retry,
        // failure alerts, delete_after_run all apply).
        match self.scheduler.update_job(
            &id,
            scheduler::JobUpdate {
                trigger_now: true,
                ..Default::default()
            },
        ) {
            Ok(true) => {
                let mut note = String::new();
                if !scheduler::is_active_hours(
                    &job.active_hours,
                    job.tz.as_deref().unwrap_or("Asia/Shanghai"),
                ) {
                    note = format!(
                        "\nNote: active_hours ({}) excludes the current time — it stays queued until the window opens.",
                        job.active_hours.as_deref().unwrap_or("?")
                    );
                }
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Job '{}' ({}) scheduled for immediate execution: fires on the next scheduler tick (within ~30s).\nSchedule: {}\nTarget: {}{}",
                        name, job.id, job.schedule, job.target, note
                    ),
                    error: None,
                })
            }
            Ok(false) => Ok(err_result(&format!("Job '{}' not found.", id))),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to trigger job: {}", e)),
            }),
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
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        // Try persistent JSONL log first, fall back to in-memory.
        let records = self.scheduler.read_run_log(id, limit);
        if records.is_empty() {
            // Fall back to in-memory records from jobs list.
            let jobs = self.scheduler.jobs();
            let job = jobs.iter().find(|j| j.id == id);
            return match job {
                Some(j) if j.last_runs.is_empty() => Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Job '{}' has no run history.",
                        j.name.as_deref().unwrap_or(&j.id)
                    ),
                    error: None,
                }),
                Some(j) => {
                    let mut output = format!(
                        "📋 Run log for '{}':\n\n",
                        j.name.as_deref().unwrap_or(&j.id)
                    );
                    for (i, run) in j.last_runs.iter().rev().take(limit).enumerate() {
                        output.push_str(&format_run_record(i + 1, run));
                    }
                    Ok(ToolResult {
                        success: true,
                        output,
                        error: None,
                    })
                }
                None => Ok(err_result(&format!("Job '{}' not found", id))),
            };
        }

        let jobs = self.scheduler.jobs();
        let job_name = jobs
            .iter()
            .find(|j| j.id == id)
            .and_then(|j| j.name.clone())
            .unwrap_or_else(|| id.to_string());

        let mut output = format!("📋 Run log for '{}':\n\n", job_name);
        for (i, run) in records.iter().enumerate() {
            output.push_str(&format_run_record(i + 1, run));
        }
        output.push_str(&format!(
            "\n({} entries from persistent log)",
            records.len()
        ));
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
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

fn format_run_record(i: usize, run: &crate::agents::scheduling::cron_types::RunRecord) -> String {
    let error_info = if let Some(ref e) = run.error {
        format!(" — {}", e)
    } else {
        String::new()
    };
    let ts = &run.run_at[..19.min(run.run_at.len())];
    format!(
        "{}. [{}] {} — {}ms{}\n",
        i,
        ts,
        run.status.as_str(),
        run.duration_ms,
        error_info,
    )
}

/// Parse schedule input: cron expression, "every 30m", or "at <ISO>".
fn parse_schedule_input(input: &str) -> Result<(String, Option<ScheduleKind>), String> {
    let trimmed = input.trim();

    // "every 30m" / "every 1h" / "every 90s"
    if let Some(rest) = trimmed.strip_prefix("every ") {
        let ms = parse_duration_to_ms(rest)?;
        return Ok((
            trimmed.to_string(),
            Some(ScheduleKind::Every { interval_ms: ms }),
        ));
    }

    // "at 2026-05-15T09:00:00+08:00"
    if let Some(rest) = trimmed.strip_prefix("at ") {
        chrono::DateTime::parse_from_rfc3339(rest)
            .map_err(|e| format!("invalid datetime '{}': {}", rest, e))?;
        return Ok((
            trimmed.to_string(),
            Some(ScheduleKind::At {
                at: rest.to_string(),
            }),
        ));
    }

    // Standard cron expression (6-field). Validate upfront.
    validate_schedule(trimmed)?;
    Ok((trimmed.to_string(), None))
}

/// Parse duration string to milliseconds.
fn parse_duration_to_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        n.parse::<u64>()
            .map_err(|_| format!("invalid ms value: '{}'", s))
    } else if let Some(n) = s.strip_suffix("s") {
        n.parse::<u64>()
            .map(|v| v * 1000)
            .map_err(|_| format!("invalid seconds: '{}'", s))
    } else if let Some(n) = s.strip_suffix("m") {
        n.parse::<u64>()
            .map(|v| v * 60_000)
            .map_err(|_| format!("invalid minutes: '{}'", s))
    } else if let Some(n) = s.strip_suffix("h") {
        n.parse::<u64>()
            .map(|v| v * 3_600_000)
            .map_err(|_| format!("invalid hours: '{}'", s))
    } else {
        Err(format!(
            "expected duration like '30s', '5m', '1h', got: '{}'",
            s
        ))
    }
}

/// Parse delivery config from JSON value.
fn parse_delivery(value: Option<&serde_json::Value>) -> Option<DeliveryConfig> {
    value.and_then(|v| {
        let channel = v.get("channel")?.as_str()?;
        Some(DeliveryConfig {
            channel: channel.to_string(),
            account_id: v
                .get("account_id")
                .and_then(|a| a.as_str())
                .map(|s| s.to_string()),
            to: v.get("to").and_then(|t| t.as_str()).map(|s| s.to_string()),
            thread_id: v
                .get("thread_id")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
        })
    })
}

/// Parse retry config from JSON value.
fn parse_retry_config(v: &serde_json::Value) -> Option<RetryConfig> {
    Some(RetryConfig {
        max_attempts: v.get("max_attempts").and_then(|v| v.as_u64()).unwrap_or(3) as u32,
        backoff_ms: v
            .get("backoff_ms")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
            .unwrap_or_else(|| vec![30_000, 60_000, 300_000]),
    })
}

/// Parse failure alert config from JSON value.
fn parse_failure_alert(v: &serde_json::Value) -> Option<FailureAlertConfig> {
    Some(FailureAlertConfig {
        after: v.get("after").and_then(|v| v.as_u64()).unwrap_or(3) as u32,
        cooldown_secs: v
            .get("cooldown_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600),
        include_skipped: v
            .get("include_skipped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// Parse a JSON array of strings.
fn parse_string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    value.and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    })
}
