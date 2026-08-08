//! Scheduler — Cron job scheduling, storage, and execution events.
//!
//! The Scheduler is the single owner of all cron job data. It:
//!   - Loads and persists jobs from `jobs.json`
//!   - Hot-reloads when the file changes on disk
//!   - Sends timing events (heartbeat, cron) via mpsc channel
//!   - Provides CRUD methods for cronjob_tool
//!   - Records run results
//!
//! External code interacts through `SharedScheduler` (Arc<Scheduler>).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Timelike;
use parking_lot::{Mutex as ParkMutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::agents::orchestrator::SchedulerEvent;
use crate::agents::scheduling::cron_types::{DeliveryConfig, RunRecord, RunStatus, ScheduleKind};
use crate::agents::webhook_loader::{WebhookAuth, WebhookJobDef, render_template};
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};
use crate::config::scheduler::{HeartbeatConfig, WebhookConfig};

/// Shared handle to the Scheduler for concurrent access.
pub type SharedScheduler = Arc<Scheduler>;

// ── JobEntry ────────────────────────────────────────────────────────────────

/// A single cron job stored in `jobs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEntry {
    /// Unique ID (12-char hex).
    pub id: String,
    /// Cron expression (6-field: sec min hour day month weekday).
    /// e.g. "0 0 9 * * *" = every day at 09:00.
    pub schedule: String,
    /// Prompt to send to the agent when triggered.
    pub prompt: String,
    /// Where to send output: "last" | "none" | channel name.
    #[serde(default = "default_target")]
    pub target: String,
    /// Optional friendly name.
    #[serde(default)]
    pub name: Option<String>,
    /// Per-job IANA timezone override (e.g. "Asia/Shanghai").
    #[serde(default)]
    pub tz: Option<String>,
    /// Active hours restriction, e.g. "08:00-24:00". None = always active.
    #[serde(default)]
    pub active_hours: Option<String>,
    /// Whether this job is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// ISO 8601 timestamp of last successful run. None = never run.
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// ISO 8601 timestamp of next scheduled run.
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// ISO 8601 timestamp of job creation.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Per-job delivery configuration (overrides target when set).
    #[serde(default)]
    pub delivery: Option<DeliveryConfig>,
    /// Run history (most recent entries, also persisted to run log file).
    #[serde(default)]
    pub last_runs: Vec<RunRecord>,
    /// Tool whitelist. If set, only these tools are available for this job.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    /// Tool blacklist. These tools are disabled for this job.
    #[serde(default)]
    pub disabled_tools: Option<Vec<String>>,
    /// Schedule kind override (every/at). If None, use schedule string as cron.
    #[serde(default)]
    pub schedule_kind: Option<ScheduleKind>,
    // ── New fields ──────────────────────────────────────────────────────────
    /// Per-job retry policy for transient errors.
    #[serde(default)]
    pub retry: Option<crate::agents::scheduling::cron_types::RetryConfig>,
    /// Per-job failure alert configuration.
    #[serde(default)]
    pub failure_alert: Option<crate::agents::scheduling::cron_types::FailureAlertConfig>,
    /// Consecutive error count (reset on success).
    #[serde(default)]
    pub consecutive_errors: u32,
    /// Consecutive skip count.
    #[serde(default)]
    pub consecutive_skipped: u32,
    /// Max number of successful runs before auto-disable (None = unlimited).
    #[serde(default)]
    pub max_runs: Option<u32>,
    /// Number of completed runs (successful or not).
    #[serde(default)]
    pub completed_runs: u32,
    /// Auto-delete after max_runs reached (default: false = just disable).
    #[serde(default)]
    pub delete_after_run: bool,
    /// Per-job model override (e.g. "claude-sonnet-4-20250514").
    #[serde(default)]
    pub model: Option<String>,
    /// Per-job provider override (e.g. "anthropic").
    #[serde(default)]
    pub provider: Option<String>,
    /// ISO 8601 timestamp of last failure alert sent.
    #[serde(default)]
    pub last_failure_alert_at: Option<String>,
    /// Context policy: inject into user session or run isolated.
    /// Defaults to Inject for cron jobs.
    #[serde(default = "default_context_policy")]
    pub context_policy: crate::config::scheduler::ContextPolicy,
}

fn default_context_policy() -> crate::config::scheduler::ContextPolicy {
    crate::config::scheduler::ContextPolicy::Inject
}

fn default_target() -> String {
    "last".to_string()
}
fn default_true() -> bool {
    true
}

/// Update fields for a cron job (all optional, only set fields are updated).
#[derive(Debug, Clone, Default)]
pub struct JobUpdate {
    pub name: Option<String>,
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub target: Option<String>,
    pub tz: Option<String>,
    pub active_hours: Option<String>,
    pub enabled: Option<bool>,
    pub delivery: Option<DeliveryConfig>,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Option<Vec<String>>,
    pub retry: Option<crate::agents::scheduling::cron_types::RetryConfig>,
    pub failure_alert: Option<crate::agents::scheduling::cron_types::FailureAlertConfig>,
    pub max_runs: Option<Option<u32>>,
    pub delete_after_run: Option<bool>,
    pub model: Option<Option<String>>,
    pub provider: Option<Option<String>>,
}

/// The top-level JSON structure of `jobs.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobsFile {
    pub jobs: Vec<JobEntry>,
}

// ── Scheduler ───────────────────────────────────────────────────────────────

/// Manages cron job scheduling, storage, and event dispatch.
/// All data access is through interior mutability (RwLock).
pub struct Scheduler {
    /// Jobs data protected by RwLock for concurrent access.
    jobs: RwLock<JobsFile>,
    /// Path to jobs.json on disk.
    path: PathBuf,
    /// Last known mtime (for hot-reload detection).
    last_mtime: ParkMutex<Option<SystemTime>>,
    /// Global IANA timezone.
    timezone: String,
    /// Heartbeat config.
    heartbeat_config: Option<HeartbeatConfig>,
    /// Event channel to orchestrator.
    event_tx: tokio::sync::mpsc::Sender<SchedulerEvent>,
    /// Last channel that received a user message (format
    /// `channel_type:account_id`). Read by heartbeat / cron output
    /// dispatch when the job's target is "last".
    pub last_channel: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Path to persist `last_channel` across restarts.
    last_channel_file: PathBuf,
    /// Last recipient (reply_target) that received a user message.
    pub last_recipient: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Path to persist `last_recipient` across restarts.
    last_recipient_file: PathBuf,
}

impl Scheduler {
    /// Create a new Scheduler. Returns a SharedScheduler (Arc<Self>).
    /// Loads existing jobs from disk if the file exists.
    pub fn new(
        path: PathBuf,
        timezone: String,
        heartbeat_config: Option<HeartbeatConfig>,
        event_tx: tokio::sync::mpsc::Sender<SchedulerEvent>,
        last_channel_file: PathBuf,
        last_recipient_file: PathBuf,
    ) -> SharedScheduler {
        let mut data = JobsFile::default();
        let mut last_mtime = None;

        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(parsed) = serde_json::from_str::<JobsFile>(&content) {
                    data = parsed;
                    last_mtime = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                }
            }
        }

        let job_count = data.jobs.len();
        tracing::info!(
            count = job_count,
            "scheduler loaded cron jobs from JSON store"
        );

        let last_channel_value = std::fs::read_to_string(&last_channel_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let last_recipient_value = std::fs::read_to_string(&last_recipient_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Arc::new(Self {
            jobs: RwLock::new(data),
            path,
            last_mtime: ParkMutex::new(last_mtime),
            timezone,
            heartbeat_config,
            event_tx,
            last_channel: Arc::new(tokio::sync::Mutex::new(last_channel_value)),
            last_channel_file,
            last_recipient: Arc::new(tokio::sync::Mutex::new(last_recipient_value)),
            last_recipient_file,
        })
    }

    /// Record the most recent (channel_type, account_id, reply_target)
    /// the orchestrator routed to. Called once per inbound UserMessage
    /// so heartbeat / cron jobs configured with `target = "last"` know
    /// where to send their output.
    pub async fn record_user_message(&self, channel_key: &str, reply_target: &str) {
        {
            let mut lc = self.last_channel.lock().await;
            if lc.as_deref() != Some(channel_key) {
                *lc = Some(channel_key.to_string());
                let _ = std::fs::write(&self.last_channel_file, channel_key);
            }
        }
        let mut lr = self.last_recipient.lock().await;
        if lr.as_deref() != Some(reply_target) {
            *lr = Some(reply_target.to_string());
            let _ = std::fs::write(&self.last_recipient_file, reply_target);
        }
    }

    /// Whether the scheduler should run (has heartbeat or cron jobs).
    pub fn should_run(&self) -> bool {
        self.heartbeat_config.is_some() || !self.jobs.read().jobs.is_empty()
    }

    /// Run the scheduler loop — sends events via mpsc.
    pub async fn run(&self) {
        let mut heartbeat_ticker = self.heartbeat_config.as_ref().and_then(|cfg| {
            parse_interval(&cfg.every).map(|interval| {
                let mut t = tokio::time::interval(interval);
                t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                t
            })
        });

        let mut cron_ticker = {
            let mut t = tokio::time::interval(Duration::from_secs(60));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        };

        tracing::info!(
            heartbeat = heartbeat_ticker.is_some(),
            cron_jobs = self.jobs.read().jobs.len(),
            "scheduler started (JSON store mode)"
        );

        loop {
            tokio::select! {
                _ = async {
                    if let Some(t) = heartbeat_ticker.as_mut() { t.tick().await; }
                    else { std::future::pending::<()>().await; }
                }, if heartbeat_ticker.is_some() => {
                    tracing::debug!("heartbeat tick fired");
                    let config = self.heartbeat_config.as_ref().unwrap();
                    if !is_active_hours(&config.active_hours, &self.timezone) {
                        tracing::debug!("heartbeat skipped: outside active hours");
                        continue;
                    }
                    match self.event_tx.send(SchedulerEvent::Heartbeat {
                        target_channel: parse_target_channel(&config.target),
                        target_account: parse_target_account(&config.target),
                    }).await {
                        Ok(()) => tracing::debug!("heartbeat event sent to orchestrator"),
                        Err(e) => tracing::warn!(err = %e, "failed to send heartbeat event"),
                    }
                }
                _ = cron_ticker.tick() => {
                    // Clean up one-shot jobs that reached max_runs + delete_after_run.
                    self.drain_auto_delete();
                    self.maybe_reload();

                    // Find due jobs (clone to release read lock before sending).
                    let due_jobs: Vec<JobEntry> = {
                        let data = self.jobs.read();
                        let now = chrono::Utc::now();
                        data.jobs.iter()
                            .filter(|j| j.enabled)
                            .filter(|j| {
                                j.next_run_at.as_ref()
                                    .and_then(|n| chrono::DateTime::parse_from_rfc3339(n).ok())
                                    .map(|dt| dt.with_timezone(&chrono::Utc) <= now)
                                    .unwrap_or(false)
                            })
                            .filter(|j| is_active_hours(&j.active_hours, j.tz.as_deref().unwrap_or(&self.timezone)))
                            .cloned()
                            .collect()
                    };

                    let mut due_job_ids = Vec::new();
                    for j in &due_jobs {
                        tracing::info!(
                            job_id = %j.id,
                            schedule = %j.schedule,
                            target = %j.target,
                            "cron job triggered"
                        );
                        let _ = self.event_tx.send(SchedulerEvent::Cron(crate::agents::orchestrator::CronTrigger {
                            session_key: format!("_cron_{}", j.id),
                            prompt: j.prompt.clone(),
                            target_channel: parse_target_channel(&j.target),
                            target_account: parse_target_account(&j.target),
                            target_recipient: j.delivery.as_ref().and_then(|d| d.to.clone()),
                            job_id: j.id.clone(),
                            model: j.model.clone(),
                            context_policy: j.context_policy,
                        })).await;
                        due_job_ids.push(j.id.clone());
                    }

                    // Mark jobs as run (updates last_run_at + next_run_at).
                    if !due_job_ids.is_empty() {
                        let mut data = self.jobs.write();
                        for id in &due_job_ids {
                            if let Some(job) = data.jobs.iter_mut().find(|j| j.id == *id) {
                                let now = chrono::Utc::now().to_rfc3339();
                                job.last_run_at = Some(now);
                                job.next_run_at = compute_next_run(
                                    &job.schedule,
                                    job.last_run_at.as_deref(),
                                    job.tz.as_deref().unwrap_or(&self.timezone),
                                );
                            }
                        }
                        let _ = self.save_to_disk_inner(&data);
                    }
                }
            }
        }
    }
}

// ── CRUD operations ─────────────────────────────────────────────────────────

impl Scheduler {
    /// Get all jobs (cloned).
    pub fn jobs(&self) -> Vec<JobEntry> {
        self.jobs.read().jobs.clone()
    }

    /// Number of jobs.
    pub fn job_count(&self) -> usize {
        self.jobs.read().jobs.len()
    }

    /// Add a new job. Returns the generated ID.
    pub fn add_job(&self, mut entry: JobEntry) -> anyhow::Result<String> {
        if entry.id.is_empty() {
            entry.id = generate_id();
        }
        if entry.created_at.is_none() {
            entry.created_at = Some(chrono::Utc::now().to_rfc3339());
        }
        entry.next_run_at = compute_next_run(
            &entry.schedule,
            None,
            entry.tz.as_deref().unwrap_or(&self.timezone),
        );
        let id = entry.id.clone();
        {
            let mut data = self.jobs.write();
            data.jobs.push(entry);
            self.save_to_disk_inner(&data)?;
        }
        Ok(id)
    }

    /// Update a job's fields. Returns true if found and updated.
    pub fn update_job(&self, id: &str, update: JobUpdate) -> anyhow::Result<bool> {
        let mut data = self.jobs.write();
        if let Some(job) = data.jobs.iter_mut().find(|j| j.id == id) {
            if let Some(name) = update.name {
                job.name = Some(name);
            }
            if let Some(schedule) = update.schedule {
                job.schedule = schedule;
                job.next_run_at = compute_next_run(
                    &job.schedule,
                    job.last_run_at.as_deref(),
                    job.tz.as_deref().unwrap_or(&self.timezone),
                );
            }
            if let Some(prompt) = update.prompt {
                job.prompt = prompt;
            }
            if let Some(target) = update.target {
                job.target = target;
            }
            if let Some(tz) = update.tz {
                job.tz = Some(tz);
                job.next_run_at = compute_next_run(
                    &job.schedule,
                    job.last_run_at.as_deref(),
                    job.tz.as_deref().unwrap_or(&self.timezone),
                );
            }
            if let Some(active_hours) = update.active_hours {
                job.active_hours = Some(active_hours);
            }
            if let Some(enabled) = update.enabled {
                job.enabled = enabled;
                if !enabled {
                    job.next_run_at = None;
                } else {
                    job.next_run_at = compute_next_run(
                        &job.schedule,
                        job.last_run_at.as_deref(),
                        job.tz.as_deref().unwrap_or(&self.timezone),
                    );
                }
            }
            if let Some(delivery) = update.delivery {
                job.delivery = Some(delivery);
            }
            if let Some(enabled_tools) = update.enabled_tools {
                job.enabled_tools = Some(enabled_tools);
            }
            if let Some(disabled_tools) = update.disabled_tools {
                job.disabled_tools = Some(disabled_tools);
            }
            if let Some(retry) = update.retry {
                job.retry = Some(retry);
            }
            if let Some(failure_alert) = update.failure_alert {
                job.failure_alert = Some(failure_alert);
            }
            if let Some(max_runs) = update.max_runs {
                job.max_runs = max_runs;
            }
            if let Some(delete_after_run) = update.delete_after_run {
                job.delete_after_run = delete_after_run;
            }
            if let Some(model) = update.model {
                job.model = model;
            }
            if let Some(provider) = update.provider {
                job.provider = provider;
            }
            self.save_to_disk_inner(&data)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Remove a job. Returns true if found and removed.
    pub fn remove_job(&self, id: &str) -> anyhow::Result<bool> {
        let mut data = self.jobs.write();
        let len_before = data.jobs.len();
        data.jobs.retain(|j| j.id != id);
        if data.jobs.len() < len_before {
            self.save_to_disk_inner(&data)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Set enabled/disabled state.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<bool> {
        self.update_job(
            id,
            JobUpdate {
                enabled: Some(enabled),
                ..Default::default()
            },
        )
    }

    /// Record a run result for a job.
    /// Returns Some(alert_message) if failure alert should be sent.
    pub fn mark_run_result(&self, id: &str, record: RunRecord) -> Option<String> {
        let mut alert_msg = None;
        let mut data = self.jobs.write();
        if let Some(job) = data.jobs.iter_mut().find(|j| j.id == id) {
            job.last_run_at = Some(record.run_at.clone());
            job.next_run_at = compute_next_run(
                &job.schedule,
                job.last_run_at.as_deref(),
                job.tz.as_deref().unwrap_or(&self.timezone),
            );

            // Track consecutive failures.
            match record.status {
                RunStatus::Ok => {
                    job.consecutive_errors = 0;
                    job.consecutive_skipped = 0;
                }
                RunStatus::Error | RunStatus::Timeout => {
                    job.consecutive_errors += 1;
                    job.consecutive_skipped = 0;
                }
                RunStatus::Skipped => {
                    job.consecutive_skipped += 1;
                    // Optionally count skipped as failure for alerting.
                    if job
                        .failure_alert
                        .as_ref()
                        .is_some_and(|a| a.include_skipped)
                    {
                        job.consecutive_errors += 1;
                    }
                }
            }

            // Check failure alert.
            if let Some(alert_cfg) = &job.failure_alert {
                if job.consecutive_errors >= alert_cfg.after {
                    let should_alert = match &job.last_failure_alert_at {
                        None => true,
                        Some(last) => {
                            let last_dt = chrono::DateTime::parse_from_rfc3339(last)
                                .map(|dt| dt.with_timezone(&chrono::Utc))
                                .unwrap_or_else(|_| {
                                    chrono::Utc::now() - chrono::Duration::hours(24)
                                });
                            let cooldown =
                                chrono::Duration::seconds(alert_cfg.cooldown_secs as i64);
                            chrono::Utc::now() - last_dt >= cooldown
                        }
                    };
                    if should_alert {
                        alert_msg = Some(format!(
                            "⚠️ Cron job '{}' ({}) has failed {} consecutive times. Last error: {}",
                            job.name.as_deref().unwrap_or(&job.id),
                            job.id,
                            job.consecutive_errors,
                            record.error.as_deref().unwrap_or("unknown"),
                        ));
                        job.last_failure_alert_at = Some(chrono::Utc::now().to_rfc3339());
                    }
                }
            }

            // Append to in-memory run history.
            job.last_runs.push(record.clone());
            // Keep only the most recent 10 entries in-memory.
            if job.last_runs.len() > 10 {
                let drain_count = job.last_runs.len() - 10;
                job.last_runs.drain(0..drain_count);
            }

            // Track completed runs.
            job.completed_runs += 1;

            // Check max_runs: auto-disable or auto-delete.
            if let Some(max) = job.max_runs {
                if job.completed_runs >= max {
                    if job.delete_after_run {
                        // Will be removed after releasing the lock.
                        // For now, mark for deletion.
                        tracing::info!(job_id = %job.id, completed = job.completed_runs, max, "job reached max_runs, marked for deletion");
                    } else {
                        job.enabled = false;
                        job.next_run_at = None;
                        tracing::info!(job_id = %job.id, completed = job.completed_runs, max, "job reached max_runs, auto-disabled");
                    }
                }
            }

            // One-shot "at" jobs auto-disable after execution.
            if matches!(job.schedule_kind, Some(ScheduleKind::At { .. })) {
                job.enabled = false;
            }

            let job_id_for_log = job.id.clone();
            let _ = self.save_to_disk_inner(&data);
            drop(data);

            // Append to JSONL run log file (outside the write lock).
            self.append_run_log_inner(&job_id_for_log, &record);
        }
        alert_msg
    }

    /// Get jobs that should be auto-deleted (reached max_runs with delete_after_run).
    pub fn drain_auto_delete(&self) -> Vec<String> {
        let mut data = self.jobs.write();
        let to_delete: Vec<String> = data
            .jobs
            .iter()
            .filter(|j| j.max_runs.is_some_and(|max| j.completed_runs >= max) && j.delete_after_run)
            .map(|j| j.id.clone())
            .collect();
        if !to_delete.is_empty() {
            data.jobs.retain(|j| !to_delete.contains(&j.id));
            let _ = self.save_to_disk_inner(&data);
            tracing::info!(
                count = to_delete.len(),
                "auto-deleted completed one-shot jobs"
            );
        }
        to_delete
    }

    /// Read recent run log entries from the JSONL file.
    /// Returns entries in reverse chronological order (newest first).
    pub fn read_run_log(&self, job_id: &str, limit: usize) -> Vec<RunRecord> {
        let log_path = self.run_log_path(job_id);
        let content = match std::fs::read_to_string(&log_path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut records: Vec<RunRecord> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        records.reverse();
        records.truncate(limit);
        records
    }

    /// Path to the run log JSONL file for a given job.
    fn run_log_path(&self, job_id: &str) -> PathBuf {
        self.path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("run_logs")
            .join(format!("{}.jsonl", job_id))
    }

    /// Append a run record to the job's JSONL log file.
    fn append_run_log_inner(&self, job_id: &str, record: &RunRecord) {
        let path = self.run_log_path(job_id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(line) = serde_json::to_string(record) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(f, "{}", line);
            }
        }
    }
}

// ── Persistence ─────────────────────────────────────────────────────────────

impl Scheduler {
    /// Atomic save: write to .tmp then rename.
    fn save_to_disk_inner(&self, data: &JobsFile) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(data)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Hot-reload: check if jobs.json changed on disk and reload if so.
    pub fn maybe_reload(&self) {
        let meta = std::fs::metadata(&self.path).ok();
        let mtime = meta.and_then(|m| m.modified().ok());
        let mut last = self.last_mtime.lock();
        if mtime == *last {
            return;
        }
        if let Ok(content) = std::fs::read_to_string(&self.path) {
            if let Ok(parsed) = serde_json::from_str::<JobsFile>(&content) {
                let mut data = self.jobs.write();
                *data = parsed;
                *last = mtime;
                tracing::info!(count = data.jobs.len(), "hot-reloaded cron jobs from disk");
            }
        }
    }

    /// Migrate jobs from old markdown files in the cron directory.
    pub fn migrate_from_markdown(&self, cron_dir: &Path) -> usize {
        if !cron_dir.exists() {
            return 0;
        }

        let entries = match std::fs::read_dir(cron_dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };

        let mut migrated = 0;
        let mut data = self.jobs.write();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let (front_matter, body) = crate::str_utils::parse_front_matter(&content);
            let schedule = match crate::str_utils::extract_yaml_string(&front_matter, "schedule") {
                Some(s) => s,
                None => continue,
            };

            let target = crate::str_utils::extract_yaml_string(&front_matter, "target")
                .unwrap_or_else(|| "last".to_string());

            let prompt = body.trim().to_string();
            if prompt.is_empty() {
                continue;
            }

            let active_hours = crate::str_utils::extract_yaml_string(&front_matter, "active_hours");

            let already_exists = data
                .jobs
                .iter()
                .any(|j| j.schedule == schedule && j.prompt == prompt);
            if already_exists {
                continue;
            }

            let entry = JobEntry {
                id: generate_id(),
                schedule,
                prompt,
                target,
                name: path.file_stem().map(|s| s.to_string_lossy().to_string()),
                tz: None,
                active_hours,
                enabled: true,
                last_run_at: None,
                next_run_at: None,
                created_at: None,
                delivery: None,
                last_runs: Vec::new(),
                enabled_tools: None,
                disabled_tools: None,
                schedule_kind: None,
                retry: None,
                failure_alert: None,
                consecutive_errors: 0,
                consecutive_skipped: 0,
                max_runs: None,
                completed_runs: 0,
                delete_after_run: false,
                model: None,
                provider: None,
                last_failure_alert_at: None,
            };

            data.jobs.push(entry);
            migrated += 1;
        }

        if migrated > 0 {
            let _ = self.save_to_disk_inner(&data);
        }
        migrated
    }
}

// ── Prompt injection scanner ────────────────────────────────────────────────

/// Scan a prompt for common injection patterns.
/// Returns Ok(()) if safe, Err(reason) if injection detected.
pub fn scan_prompt_injection(prompt: &str) -> Result<(), String> {
    let lower = prompt.to_lowercase();

    let role_hijack = [
        "ignore previous",
        "ignore all instructions",
        "you are now",
        "system prompt",
        "忽略之前",
        "忽略所有",
        "你现在是",
        "你的新角色",
        "disregard your",
        "override your instructions",
        "forget your instructions",
        "new instructions",
    ];
    for pattern in &role_hijack {
        if lower.contains(pattern) {
            return Err(format!(
                "prompt injection detected (role hijack): '{}'",
                pattern
            ));
        }
    }

    let exfiltration = [
        "send to http",
        "post to http",
        "curl -x post",
        "wget --post",
        "发送到http",
        "上传到http",
    ];
    for pattern in &exfiltration {
        if lower.contains(pattern) {
            return Err(format!(
                "prompt injection detected (exfiltration): '{}'",
                pattern
            ));
        }
    }

    Ok(())
}

// ── Schedule validation ─────────────────────────────────────────────────────

/// Normalize a Unix-style cron expression (0=Sun) to Quartz-style (1=Sun).
/// `cron` crate v0.15 uses 1-7 for Sunday-Saturday.
/// This translates the 6th field (weekday) of a 6-field cron expression.
pub fn normalize_weekday_unix(schedule: &str) -> String {
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    if parts.len() != 6 {
        return schedule.to_string();
    }
    let weekday_part = parts[5];
    if weekday_part == "*" {
        return schedule.to_string();
    }

    let normalized = weekday_part
        .split('/')
        .enumerate()
        .map(|(i, segment)| {
            if i == 1 {
                segment.to_string() // step part: leave unchanged
            } else {
                shift_weekday_segment(segment)
            }
        })
        .collect::<Vec<_>>()
        .join("/");

    format!(
        "{} {} {} {} {} {}",
        parts[0], parts[1], parts[2], parts[3], parts[4], normalized
    )
}

/// Shift weekday numbers in a comma-separated segment (e.g. "1,3,5" → "2,4,6").
fn shift_weekday_segment(segment: &str) -> String {
    segment
        .split(',')
        .map(shift_weekday_item)
        .collect::<Vec<_>>()
        .join(",")
}

/// Shift a single weekday item: single number, range, or named day.
fn shift_weekday_item(item: &str) -> String {
    if let Some(dash) = item.find('-') {
        let (start, end) = (&item[..dash], &item[dash + 1..]);
        if let (Ok(s), Ok(e)) = (start.parse::<u32>(), end.parse::<u32>()) {
            return format!("{}-{}", shift_weekday_num(s), shift_weekday_num(e));
        }
        return item.to_string(); // named range like MON-FRI
    }
    if let Ok(n) = item.parse::<u32>() {
        return shift_weekday_num(n).to_string();
    }
    item.to_string()
}

/// Shift a weekday number from Unix (0=Sun..7=Sun) to cron-crate (1=Sun..7=Sat).
fn shift_weekday_num(n: u32) -> u32 {
    match n {
        0 | 7 => 1,
        1..=6 => n + 1,
        _ => n,
    }
}

/// Validate a schedule string (cron expression).
/// Returns Ok(()) if valid, Err(reason) if invalid.
pub fn validate_schedule(schedule: &str) -> Result<(), String> {
    let normalized = normalize_weekday_unix(schedule);
    let parsed: Result<cron::Schedule, _> = normalized.parse();
    match parsed {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("invalid cron expression '{}': {}", schedule, e)),
    }
}

/// Validate a one-shot "at" timestamp.
/// Returns Ok(()) if valid and in the future, Err(reason) otherwise.
pub fn validate_at_timestamp(at: &str) -> Result<(), String> {
    let dt = chrono::DateTime::parse_from_rfc3339(at)
        .map_err(|e| format!("invalid ISO 8601 timestamp '{}': {}", at, e))?;
    let now = chrono::Utc::now();
    if dt.with_timezone(&chrono::Utc) <= now {
        Err(format!("one-shot timestamp '{}' is in the past", at))
    } else {
        Ok(())
    }
}

/// Validate a timezone string.
/// Returns Ok(()) if valid IANA timezone, Err(reason) otherwise.
pub fn validate_tz(tz: &str) -> Result<(), String> {
    if tz.parse::<chrono_tz::Tz>().is_err() {
        Err(format!("invalid IANA timezone '{}'", tz))
    } else {
        Ok(())
    }
}

/// Validate active_hours format "HH:MM-HH:MM".
/// Returns Ok(()) if valid, Err(reason) otherwise.
pub fn validate_active_hours(hours: &str) -> Result<(), String> {
    match parse_hours(hours) {
        Some((start, end)) => {
            if start >= end {
                Err(format!(
                    "active_hours start ({}) must be before end ({})",
                    start, end
                ))
            } else {
                Ok(())
            }
        }
        None => Err(format!(
            "invalid active_hours format '{}', expected 'HH:MM-HH:MM'",
            hours
        )),
    }
}

// ── Schedule computation ────────────────────────────────────────────────────

/// Resolve an IANA timezone name to a `chrono_tz::Tz`.
/// Falls back to UTC if the name is invalid.
pub fn resolve_tz(name: &str) -> chrono_tz::Tz {
    name.parse::<chrono_tz::Tz>().unwrap_or_else(|_| {
        tracing::warn!(tz = %name, "invalid IANA timezone, falling back to UTC");
        chrono_tz::UTC
    })
}

/// Compute the next run time for a job.
/// Supports both legacy cron expressions and new ScheduleKind.
pub fn compute_next_run(schedule: &str, last_run: Option<&str>, tz_name: &str) -> Option<String> {
    compute_next_run_inner(None, schedule, last_run, tz_name)
}

/// Full compute with ScheduleKind support.
pub fn compute_next_run_full(
    kind: Option<&ScheduleKind>,
    schedule: &str,
    last_run: Option<&str>,
    tz_name: &str,
) -> Option<String> {
    compute_next_run_inner(kind, schedule, last_run, tz_name)
}

fn compute_next_run_inner(
    kind: Option<&ScheduleKind>,
    schedule: &str,
    last_run: Option<&str>,
    tz_name: &str,
) -> Option<String> {
    match kind {
        Some(ScheduleKind::Every { interval_ms }) => {
            let base_ms = last_run
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.timestamp_millis() as u64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64);
            let next_ms = base_ms + interval_ms;
            chrono::DateTime::from_timestamp_millis(next_ms as i64)
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        Some(ScheduleKind::At { at }) => {
            if last_run.is_some() {
                return None; // Already executed
            }
            chrono::DateTime::parse_from_rfc3339(at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        Some(ScheduleKind::Cron { expr }) => {
            let normalized = normalize_weekday_unix(expr);
            let cron_schedule: cron::Schedule = match normalized.parse() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(schedule = %expr, err = %e, "invalid cron expression");
                    return None;
                }
            };
            let tz = resolve_tz(tz_name);
            let base_utc = match last_run {
                Some(ts) => chrono::DateTime::parse_from_rfc3339(ts)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                None => chrono::Utc::now(),
            };
            let base_local = base_utc.with_timezone(&tz);
            cron_schedule
                .after(&base_local)
                .next()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        None => {
            // Legacy cron expression from schedule string.
            let normalized = normalize_weekday_unix(schedule);
            let cron_schedule: cron::Schedule = match normalized.parse() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(schedule = %schedule, err = %e, "invalid cron expression");
                    return None;
                }
            };
            let tz = resolve_tz(tz_name);
            let base_utc = match last_run {
                Some(ts) => chrono::DateTime::parse_from_rfc3339(ts)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                None => chrono::Utc::now(),
            };
            let base_local = base_utc.with_timezone(&tz);
            cron_schedule
                .after(&base_local)
                .next()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
    }
}

/// Generate a random 12-char hex ID.
fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:012x}", nanos & 0xfffffffffff)
}

// ── Webhook app state ──────────────────────────────────────────────────────

/// Axum app state for the webhook server. Holds the shared
/// [`OrchestratorCtx`](crate::agents::OrchestratorCtx) (SessionManager /
/// AgentRuntime / channel map / scheduler) plus the webhook-specific timezone
/// for cron parsing.
pub struct WebhookContext {
    /// Shared orchestrator dependency bundle.
    pub ctx: Arc<crate::agents::OrchestratorCtx>,
    /// Timezone string used for cron evaluation in the webhook server.
    pub timezone: String,
}

// ── Interval parsing ───────────────────────────────────────────────────────

/// Parse interval string like "5m", "30m", "1h" to Duration.
pub fn parse_interval(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s == "0" {
        return None;
    }

    let (num_part, multiplier) = if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else {
        // Default to minutes if no suffix
        (s, 60)
    };

    let num: u64 = num_part.parse().ok()?;
    Some(Duration::from_secs(num * multiplier))
}

// ── Active hours ───────────────────────────────────────────────────────────

/// Parse target string "channel:account" into channel part.
/// Returns None for "last", "none", or empty strings.
fn parse_target_channel(target: &str) -> Option<String> {
    match target {
        "last" | "none" | "" => None,
        _ => target
            .split_once(':')
            .map(|(ch, _)| ch.to_string())
            .or_else(|| Some(target.to_string())),
    }
}

/// Parse target string "channel:account" into account part.
/// Returns None for "last", "none", or empty strings.
fn parse_target_account(target: &str) -> Option<String> {
    match target {
        "last" | "none" | "" => None,
        _ => target.split_once(':').map(|(_, acc)| acc.to_string()),
    }
}

/// Check if current time is within active hours.
/// Format: "HH:MM-HH:MM" e.g. "08:00-24:00".
/// `tz_name` is the IANA timezone (e.g. "Asia/Shanghai").
pub fn is_active_hours(active_hours: &Option<String>, tz_name: &str) -> bool {
    let Some(hours) = active_hours else {
        return true; // No restriction = always active
    };

    let (start_mins, end_mins) = match parse_hours(hours) {
        Some(h) => h,
        None => return true, // Invalid format = always active
    };

    let tz = resolve_tz(tz_name);
    let now_local = chrono::Utc::now().with_timezone(&tz);
    let now_mins = now_local.hour() * 60 + now_local.minute();

    now_mins >= start_mins && now_mins < end_mins
}

/// Parse "HH:MM-HH:MM" → (start_minutes, end_minutes).
fn parse_hours(s: &str) -> Option<(u32, u32)> {
    let (start, end) = s.split_once('-')?;
    Some((parse_hhmm(start.trim())?, parse_hhmm(end.trim())?))
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let hours: u32 = h.trim().parse().ok()?;
    let mins: u32 = m.trim().parse().ok()?;
    if hours > 24 || mins >= 60 || (hours == 24 && mins > 0) {
        return None;
    }
    Some(hours * 60 + mins)
}

// ── Webhook execution helpers ──────────────────────────────────────────────

/// Execute one webhook turn. Delegates to the orchestrator's shared
/// `run_scheduled_turn` (the single scheduled-turn entry point) — scheduled
/// output (no channel during the turn) is dispatched by the webhook caller via
/// `send_to_target` after this returns.
pub async fn run_scheduled_task(
    ctx: &WebhookContext,
    session_key: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    crate::agents::orchestrator::run_scheduled_turn(&ctx.ctx, session_key, prompt, None).await
}

/// Send a response to the configured target channel.
pub async fn send_to_target(ctx: &WebhookContext, target: &str, content: &str) {
    let (ch_type, acc_id) = match target {
        "none" => return,
        "last" => {
            let last: Option<String> = match ctx.ctx.scheduler.as_ref() {
                Some(s) => s.last_channel.lock().await.clone(),
                None => None,
            };
            match last {
                Some(ref key) => match key.split_once(':') {
                    Some((ch, acc)) => (ch.to_string(), acc.to_string()),
                    None => {
                        tracing::warn!(key = %key, "invalid last_channel format");
                        return;
                    }
                },
                None => {
                    tracing::warn!("no target channel for scheduled response");
                    return;
                }
            }
        }
        name => {
            // Parse "channel:account" or just "channel" (default account)
            match name.split_once(':') {
                Some((ch, acc)) => (ch.to_string(), acc.to_string()),
                None => (name.to_string(), "default".to_string()),
            }
        }
    };

    let channel = match ctx.ctx.channels.get(&(ch_type.clone(), acc_id.clone())) {
        Some(ch) => ch.clone(),
        None => {
            tracing::warn!(channel = %ch_type, account = %acc_id, "target channel not found");
            return;
        }
    };

    let msg = ChannelOutboundMessage {
        receiver: MessageReceiver::new(String::new()),
        content: ChannelMessageContent::text(content),
        options: Default::default(),
    };

    if let Err(e) = channel.send_message(&msg).await {
        tracing::warn!(channel = %ch_type, account = %acc_id, err = %e, "failed to send scheduled response");
    }
}

// ── Webhook ────────────────────────────────────────────────────────────────

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

/// Run the webhook HTTP server.
///
/// If `pre_bound` is `Some`, use the pre-bound `SO_REUSEPORT` listener instead
/// of binding a fresh socket.  This is used during hot switch so the new process
/// can accept connections on the same port before the old process releases it.
pub async fn run_webhook_server(
    ctx: Arc<WebhookContext>,
    config: WebhookConfig,
    jobs: Vec<WebhookJobDef>,
    pre_bound: Option<std::net::TcpListener>,
) {
    let listener = if let Some(std_listener) = pre_bound {
        let _ = std_listener.set_nonblocking(true);
        match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(port = config.port, err = %e, "webhook: failed to convert pre-bound listener");
                return;
            }
        }
    } else {
        match tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(port = config.port, err = %e, "webhook: failed to bind");
                return;
            }
        }
    };

    let global_secret = config.secret.clone();
    let jobs = Arc::new(jobs);

    tracing::info!(
        port = config.port,
        routes = jobs.len(),
        "webhook server started"
    );

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(err = %e, "webhook: accept failed");
                continue;
            }
        };

        let io = TokioIo::new(stream);
        let ctx = ctx.clone();
        let jobs = jobs.clone();
        let global_secret = global_secret.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                let jobs = jobs.clone();
                let global_secret = global_secret.clone();
                async move { handle_request(req, ctx, &jobs, &global_secret).await }
            });

            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                tracing::debug!(err = %e, "webhook: connection error");
            }
        });
    }
}

/// Main request dispatcher — routes to built-in endpoints or custom webhook jobs.
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<WebhookContext>,
    jobs: &[WebhookJobDef],
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    if req.method() != Method::POST {
        return ok_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }

    let path = req.uri().path().to_string();

    // ── Built-in endpoints ────────────────────────────────────────────
    match path.as_str() {
        "/hooks/agent" => return handle_hooks_agent(req, ctx, global_secret).await,
        "/hooks/wake" => return handle_hooks_wake(req, global_secret).await,
        _ => {}
    }

    // ── Custom webhook routes ─────────────────────────────────────────
    let job = match jobs.iter().find(|j| j.path == path) {
        Some(j) => j,
        None => return ok_response(StatusCode::NOT_FOUND, "no webhook at this path"),
    };

    // Extract auth headers before consuming body.
    let sig_header = req
        .headers()
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Collect body bytes.
    let body_bytes = match collect_body(req.into_body()).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(err = %e, "webhook: failed to read body");
            return ok_response(StatusCode::BAD_REQUEST, "failed to read body");
        }
    };

    // Verify auth per-route.
    if let Some(ref secret) = job.secret {
        match job.auth {
            WebhookAuth::Hmac => match sig_header {
                Some(ref sig) if !verify_hmac_signature(&body_bytes, secret, sig) => {
                    tracing::warn!(path = %path, "webhook: HMAC verification failed");
                    return ok_response(StatusCode::UNAUTHORIZED, "invalid signature");
                }
                None => {
                    tracing::warn!(path = %path, "webhook: missing signature header");
                    return ok_response(StatusCode::UNAUTHORIZED, "missing signature");
                }
                _ => {}
            },
            WebhookAuth::Bearer => {
                let expected = format!("Bearer {}", secret);
                match auth_header {
                    Some(ref h) if h.as_str() == expected => {}
                    _ => {
                        tracing::warn!(path = %path, "webhook: Bearer auth failed");
                        return ok_response(StatusCode::UNAUTHORIZED, "invalid token");
                    }
                }
            }
        }
    }

    tracing::info!(path = %path, "webhook triggered");

    // Parse payload as JSON for template rendering.
    let payload: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);

    // Render template with payload.
    let prompt = render_template(&job.prompt_template, &payload);

    let session_key = format!(
        "_webhook_{}",
        path.trim_start_matches('/').replace('/', "_")
    );
    let result = run_scheduled_task(&ctx, &session_key, &prompt).await;

    match result {
        Ok(response) => {
            if !response.trim().is_empty() && job.target != "none" {
                send_to_target(&ctx, &job.target, &response).await;
            }
            ok_response(StatusCode::OK, "ok")
        }
        Err(e) => {
            tracing::warn!(err = %e, "webhook: agent run failed");
            ok_response(StatusCode::INTERNAL_SERVER_ERROR, "agent error")
        }
    }
}

/// `POST /hooks/agent` — Run an isolated agent turn.
/// Body: `{"message": "...", "target": "last"}`
async fn handle_hooks_agent(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<WebhookContext>,
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    // Verify global Bearer token.
    if let Some(secret) = global_secret {
        let expected = format!("Bearer {}", secret);
        match req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
        {
            Some(h) if h == expected => {}
            _ => return ok_response(StatusCode::UNAUTHORIZED, "invalid token"),
        }
    }

    let body_bytes = collect_body(req.into_body()).await?;
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(err = %e, "/hooks/agent: invalid JSON body");
            return ok_response(StatusCode::BAD_REQUEST, "invalid JSON");
        }
    };

    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if message.is_empty() {
        return ok_response(StatusCode::BAD_REQUEST, "missing 'message' field");
    }

    let target = payload
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("last");

    tracing::info!(target = target, "/hooks/agent triggered");

    let result = run_scheduled_task(&ctx, "_hooks_agent", &message).await;

    match result {
        Ok(response) => {
            if !response.trim().is_empty() && target != "none" {
                send_to_target(&ctx, target, &response).await;
            }
            ok_response(StatusCode::OK, "ok")
        }
        Err(e) => {
            tracing::warn!(err = %e, "/hooks/agent: agent run failed");
            ok_response(StatusCode::INTERNAL_SERVER_ERROR, "agent error")
        }
    }
}

/// `POST /hooks/wake` — Trigger an immediate heartbeat.
/// Body: `{"text": "..."}`
async fn handle_hooks_wake(
    req: Request<hyper::body::Incoming>,
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    // Verify global Bearer token.
    if let Some(secret) = global_secret {
        let expected = format!("Bearer {}", secret);
        match req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
        {
            Some(h) if h == expected => {}
            _ => return ok_response(StatusCode::UNAUTHORIZED, "invalid token"),
        }
    }

    let body_bytes = collect_body(req.into_body()).await?;
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(err = %e, "/hooks/wake: invalid JSON body");
            return ok_response(StatusCode::BAD_REQUEST, "invalid JSON");
        }
    };

    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    tracing::info!(text = %text, "/hooks/wake triggered");

    // TODO: integrate with heartbeat wakeup mechanism (enqueue system event)
    // For now, just acknowledge.
    ok_response(StatusCode::OK, "wake acknowledged")
}

// ── Auth helpers ───────────────────────────────────────────────────────────

/// Verify HMAC-SHA256 signature against the `X-Hub-Signature-256` header value.
fn verify_hmac_signature(body: &[u8], secret: &str, header_value: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let result = mac.finalize();
    let expected_hex = format!("sha256={}", hex::encode(result.into_bytes()));

    // Constant-time comparison.
    let a = expected_hex.as_bytes();
    let b = header_value.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ── HTTP helpers ───────────────────────────────────────────────────────────

/// Collect full body bytes from an incoming body stream.
async fn collect_body<B>(body: B) -> anyhow::Result<Bytes>
where
    B: hyper::body::Body,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    use http_body_util::BodyExt;
    let collected = body.collect().await?;
    Ok(collected.to_bytes())
}

fn ok_response(status: StatusCode, body: &str) -> anyhow::Result<Response<Full<Bytes>>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .map_err(Into::into)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::orchestrator::is_silent_ok;

    #[test]
    fn parse_interval_minutes() {
        assert_eq!(parse_interval("30m"), Some(Duration::from_secs(30 * 60)));
        assert_eq!(parse_interval("5m"), Some(Duration::from_secs(5 * 60)));
    }

    #[test]
    fn parse_interval_hours() {
        assert_eq!(parse_interval("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_interval("2h"), Some(Duration::from_secs(7200)));
    }

    #[test]
    fn parse_interval_seconds() {
        assert_eq!(parse_interval("30s"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_interval_zero_disables() {
        assert_eq!(parse_interval("0"), None);
    }

    #[test]
    fn parse_interval_invalid() {
        assert_eq!(parse_interval("abc"), None);
    }

    #[test]
    fn parse_hours_valid() {
        assert_eq!(parse_hhmm("08:00"), Some(480));
        assert_eq!(parse_hhmm("24:00"), Some(1440));
        assert_eq!(parse_hhmm("13:30"), Some(810));
    }

    #[test]
    fn is_active_hours_no_restriction() {
        assert!(is_active_hours(&None, "Asia/Shanghai"));
    }

    #[test]
    fn is_active_hours_invalid_format_always_active() {
        assert!(is_active_hours(&Some("bad".to_string()), "Asia/Shanghai"));
    }

    #[test]
    fn silent_heartbeat_ok() {
        assert!(is_silent_ok("heartbeat_ok", "heartbeat"));
        assert!(is_silent_ok("Heartbeat_OK", "heartbeat"));
        assert!(is_silent_ok(" heartbeat_ok ", "heartbeat"));
        assert!(!is_silent_ok("I found something", "heartbeat"));
    }

    #[test]
    fn verify_hmac_signature_valid() {
        let body = b"test payload";
        let secret = "my-secret";
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_hmac_signature(body, secret, &sig));
    }

    #[test]
    fn verify_hmac_signature_invalid() {
        assert!(!verify_hmac_signature(
            b"test payload",
            "secret",
            "sha256=bad_hex"
        ));
    }

    #[test]
    fn verify_hmac_signature_wrong_length() {
        assert!(!verify_hmac_signature(b"body", "secret", "sha256=abc"));
    }

    #[test]
    fn test_normalize_weekday_unix() {
        assert_eq!(normalize_weekday_unix("0 0 9 * * 0"), "0 0 9 * * 1"); // Sun
        assert_eq!(normalize_weekday_unix("0 0 9 * * 1"), "0 0 9 * * 2"); // Mon
        assert_eq!(normalize_weekday_unix("0 0 9 * * 6"), "0 0 9 * * 7"); // Sat
        assert_eq!(normalize_weekday_unix("0 0 9 * * 7"), "0 0 9 * * 1"); // Sun
        assert_eq!(normalize_weekday_unix("0 0 9 * * 1-5"), "0 0 9 * * 2-6"); // Mon-Fri
        assert_eq!(normalize_weekday_unix("0 0 9 * * 0,6"), "0 0 9 * * 1,7"); // Sun,Sat
        assert_eq!(normalize_weekday_unix("0 0 9 * * */2"), "0 0 9 * * */2"); // step unchanged
        assert_eq!(normalize_weekday_unix("0 0 9 * * 1-5/2"), "0 0 9 * * 2-6/2");
        assert_eq!(normalize_weekday_unix("0 0 9 * * MON-FRI"), "0 0 9 * * MON-FRI");
        assert_eq!(normalize_weekday_unix("0 0 9 * * MON,5"), "0 0 9 * * MON,6");
        assert_eq!(normalize_weekday_unix("not a cron"), "not a cron");
    }

    #[test]
    fn validate_schedule_valid() {
        assert!(validate_schedule("0 0 9 * * *").is_ok());
        assert!(validate_schedule("0 */30 * * * *").is_ok());
    }

    #[test]
    fn validate_schedule_invalid() {
        assert!(validate_schedule("not a cron").is_err());
    }

    #[test]
    fn validate_tz_valid() {
        assert!(validate_tz("Asia/Shanghai").is_ok());
        assert!(validate_tz("UTC").is_ok());
    }

    #[test]
    fn validate_tz_invalid() {
        assert!(validate_tz("Invalid/Zone").is_err());
    }

    #[test]
    fn validate_active_hours_valid() {
        assert!(validate_active_hours("08:00-24:00").is_ok());
        assert!(validate_active_hours("00:00-23:59").is_ok());
    }

    #[test]
    fn validate_active_hours_invalid_format() {
        assert!(validate_active_hours("bad").is_err());
        assert!(validate_active_hours("25:00-26:00").is_err());
    }

    #[test]
    fn validate_active_hours_start_ge_end() {
        assert!(validate_active_hours("18:00-08:00").is_err());
        assert!(validate_active_hours("12:00-12:00").is_err());
    }
}
