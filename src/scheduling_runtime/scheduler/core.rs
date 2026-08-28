//! Scheduler core（P2 自 scheduler.rs 拆出，纯移动）：Scheduler /
//! SharedScheduler / OrchestratorHook / DistillConfig / JobsFile 类型、
//! SchedulerApi facade、事件循环（run / maybe_fire_distill）、CRUD、
//! 投递解析与持久化（save_to_disk_inner / maybe_reload /
//! migrate_from_markdown）。

use super::jobs_file::{
    delete_job_dir, fold_schedule_kind, fold_target_delivery, load_jobs_from_dirs,
    normalize_schedule_specs,
};
use super::*;

// ── #151 Phase 8+ SchedulerApi facade impl ───────────────────────────────────
impl crate::scheduling_types::job_types::SchedulerApi for Scheduler {
    fn add_job(&self, entry: JobEntry) -> anyhow::Result<String> {
        Scheduler::add_job(self, entry)
    }

    fn update_job(&self, id: &str, update: JobUpdate) -> anyhow::Result<bool> {
        Scheduler::update_job(self, id, update)
    }

    fn jobs(&self) -> Vec<JobEntry> {
        Scheduler::jobs(self)
    }

    fn set_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<bool> {
        Scheduler::set_enabled(self, id, enabled)
    }

    fn remove_job(&self, id: &str) -> anyhow::Result<Option<JobRemovalAudit>> {
        Scheduler::remove_job(self, id)
    }

    fn read_run_log(&self, job_id: &str, limit: usize) -> Vec<RunRecord> {
        Scheduler::read_run_log(self, job_id, limit)
    }
}

/// Shared handle to the Scheduler for concurrent access.
pub type SharedScheduler = Arc<Scheduler>;

/// Orchestrator-side callbacks the webhook/scheduling runtime needs
/// (#151 Phase 3d, SCC 解环: direction inversion).
///
/// Previously `WebhookContext` held a direct `Arc<OrchestratorCtx>`
/// directly and called `agents::orchestrator::run_scheduled_turn`, making
/// scheduling_runtime depend on agents while agents re-exported
/// scheduling_runtime — a same-layer SCC (found in the #160 review). This
/// trait is defined HERE (the consumer side) and implemented by the
/// orchestrator (the producer side); the daemon wires the implementation in
/// at assembly time. Same functions, same data flow — only the dependency
/// arrow flips.
#[async_trait::async_trait]
pub trait OrchestratorHook: Send + Sync {
    /// Run one scheduled turn (`agents::orchestrator::run_scheduled_turn`).
    async fn run_scheduled_turn(
        &self,
        session_key: &str,
        prompt: &str,
    ) -> anyhow::Result<String>;

    /// Look up a live outbound channel by `(channel_type, account_id)`.
    fn outbound_channel(
        &self,
        channel_type: &str,
        account_id: &str,
    ) -> Option<Arc<dyn Channel>>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobsFile {
    pub jobs: Vec<JobEntry>,
}

// ── Scheduler ───────────────────────────────────────────────────────────────

/// Idle-time memory distillation scheduling config.
#[derive(Debug, Clone)]
pub struct DistillConfig {
    /// No inbound user message for this many seconds before a pass may run.
    pub idle_secs: u64,
    /// How often the scheduler fires a distill check (seconds).
    pub interval_secs: u64,
}

/// Manages cron job scheduling, storage, and event dispatch.
/// All data access is through interior mutability (RwLock).
pub struct Scheduler {
    /// Jobs data protected by RwLock for concurrent access.
    jobs: RwLock<JobsFile>,
    /// P1-B2: jobs entity root (`{base_dir}/jobs`). Each job lives at
    /// `{root}/{dir_name(id)}/meta.json`; the legacy single-file
    /// `{root}/jobs.json` is read as a fallback (pre-migration layouts).
    jobs_root: PathBuf,
    /// Legacy single-file store path (read fallback only).
    legacy_path: PathBuf,
    /// Identity namespace for job FQIDs (`<ns>/job/<uuidv7>`).
    namespace: String,
    /// Last known mtime (for hot-reload detection).
    last_mtime: ParkMutex<Option<SystemTime>>,
    /// Global IANA timezone.
    timezone: String,
    /// Idle-time distillation config (None = disabled).
    distill_config: Option<DistillConfig>,
    /// Unix seconds of the last inbound user message. 0 = never.
    last_inbound: AtomicU64,
    /// Event channel to orchestrator.
    event_tx: tokio::sync::mpsc::Sender<SchedulerEvent>,
    /// Last channel that received a user message (format
    /// `channel_type:account_id`). Read by cron output
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: PathBuf,
        namespace: &str,
        timezone: String,
        distill_config: Option<DistillConfig>,
        event_tx: tokio::sync::mpsc::Sender<SchedulerEvent>,
        last_channel_file: PathBuf,
        last_recipient_file: PathBuf,
    ) -> SharedScheduler {
        // P1-B2: `path` is the jobs root dir ({base_dir}/jobs). Accept a
        // jobs.json path too (older embedders): normalize to its parent.
        let (jobs_root, legacy_path) = if path.extension().is_some_and(|e| e == "json") {
            let legacy = path.clone();
            (
                path.parent().map(|p| p.to_path_buf()).unwrap_or(path),
                legacy,
            )
        } else {
            (path.clone(), path.join("jobs.json"))
        };

        let mut data = JobsFile::default();
        let mut last_mtime = None;

        // Directory-based store wins; fall back to the legacy single file
        // only when no per-job meta.json exists yet (pre-migration layout).
        let mut from_dirs = load_jobs_from_dirs(&jobs_root);
        if from_dirs.is_empty() && legacy_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&legacy_path) {
                // Value-level fold first: JobEntry no longer carries
                // schedule_kind, so a direct struct parse would silently drop
                // every/at discriminators and misread their display strings
                // as cron expressions (review finding).
                let folded: Option<JobsFile> = serde_json::from_str::<serde_json::Value>(&content)
                    .ok()
                    .map(fold_schedule_kind)
                    .map(fold_target_delivery)
                    .and_then(|v| serde_json::from_value(v).ok());
                if let Some(mut parsed) = folded {
                    normalize_schedule_specs(&mut parsed.jobs);
                    from_dirs = parsed.jobs;
                }
            }
        }
        if !from_dirs.is_empty() {
            data.jobs = from_dirs;
            last_mtime = std::fs::metadata(&jobs_root)
                .ok()
                .and_then(|m| m.modified().ok());
        }

        let job_count = data.jobs.len();
        tracing::info!(
            count = job_count,
            root = %jobs_root.display(),
            "scheduler loaded cron jobs from jobs store"
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
            jobs_root,
            legacy_path,
            namespace: namespace.to_string(),
            last_mtime: ParkMutex::new(last_mtime),
            timezone,
            distill_config,
            last_inbound: AtomicU64::new(0),
            event_tx,
            last_channel: Arc::new(tokio::sync::Mutex::new(last_channel_value)),
            last_channel_file,
            last_recipient: Arc::new(tokio::sync::Mutex::new(last_recipient_value)),
            last_recipient_file,
        })
    }

    /// Record the most recent (channel_type, account_id, reply_target)
    /// the orchestrator routed to. Called once per inbound UserMessage
    /// so cron jobs configured with `target = "last"` know
    /// where to send their output. Also stamps `last_inbound` — the idle
    /// clock for memory distillation.
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
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_inbound.store(now, Ordering::Relaxed);
    }

    /// Whether the scheduler should run (has distill or cron jobs).
    pub fn should_run(&self) -> bool {
        self.distill_config.is_some() || !self.jobs.read().jobs.is_empty()
    }

    /// Distill tick: fire a `Distill` event when the system has been idle
    /// (no inbound user message) for `idle_secs`. The orchestrator decides
    /// whether there is actually anything new to distill.
    async fn maybe_fire_distill(&self) {
        let Some(cfg) = self.distill_config.as_ref() else {
            return;
        };
        let last_inbound = self.last_inbound.load(Ordering::Relaxed);
        if last_inbound > 0 {
            let now = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if now.saturating_sub(last_inbound) < cfg.idle_secs {
                tracing::debug!(
                    idle_secs = cfg.idle_secs,
                    elapsed_secs = now.saturating_sub(last_inbound),
                    "memory_distill: skipped, system not idle"
                );
                return;
            }
        }
        match self.event_tx.send(SchedulerEvent::Distill).await {
            Ok(()) => tracing::debug!("memory_distill: distill event sent to orchestrator"),
            Err(e) => tracing::warn!(err = %e, "failed to send distill event"),
        }
    }

    /// Run the scheduler loop — sends events via mpsc.
    pub async fn run(&self) {
        let mut cron_ticker = {
            let mut t = tokio::time::interval(Duration::from_secs(60));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        };

        let mut distill_ticker = self.distill_config.as_ref().map(|cfg| {
            let mut t = tokio::time::interval(Duration::from_secs(cfg.interval_secs.max(60)));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        });

        tracing::info!(
            distill = distill_ticker.is_some(),
            cron_jobs = self.jobs.read().jobs.len(),
            "scheduler started (JSON store mode)"
        );

        loop {
            tokio::select! {
                _ = async {
                    if let Some(t) = distill_ticker.as_mut() { t.tick().await; }
                    else { std::future::pending::<()>().await; }
                }, if distill_ticker.is_some() => {
                    self.maybe_fire_distill().await;
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
                            .filter(|j| j.schedule.is_some())
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
                            schedule = ?j.schedule,
                            delivery_mode = ?j.delivery.mode,
                            "cron job triggered"
                        );
                        let (target_channel, target_account, target_recipient, target_thread, delivery_suppressed) =
                            cron_delivery_fields(&j.delivery);
                        let _ = self.event_tx.send(SchedulerEvent::Cron(crate::scheduling_types::event::CronTrigger {
                            session_key: format!("_job_{}", crate::ids::bare_dir_name(&j.id)),
                            prompt: j.prompt.clone(),
                            target_channel,
                            target_account,
                            target_recipient,
                            target_thread,
                            delivery_suppressed,
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
                                    job.schedule.as_ref(),
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
        // §3.4: name is required — user-facing identity and (for webhook
        // channels) the route segment.
        if entry.name.as_deref().map(str::trim).unwrap_or("").is_empty() {
            anyhow::bail!("job name is required (design §3.4)");
        }
        if entry.id.is_empty() {
            entry.id = self.generate_id();
        }
        if entry.created_at.is_none() {
            entry.created_at = Some(chrono::Utc::now().to_rfc3339());
        }
        entry.next_run_at = compute_next_run(
            entry.schedule.as_ref(),
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
            if update.schedule_changed {
                job.schedule = update.schedule.flatten();
                job.next_run_at = compute_next_run(
                    job.schedule.as_ref(),
                    job.last_run_at.as_deref(),
                    job.tz.as_deref().unwrap_or(&self.timezone),
                );
            }
            if let Some(prompt) = update.prompt {
                job.prompt = prompt;
            }
            if let Some(tz) = update.tz {
                job.tz = Some(tz);
                job.next_run_at = compute_next_run(
                    job.schedule.as_ref(),
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
                        job.schedule.as_ref(),
                        job.last_run_at.as_deref(),
                        job.tz.as_deref().unwrap_or(&self.timezone),
                    );
                }
            }
            if let Some(delivery) = update.delivery {
                job.delivery = delivery;
            }
            if update.webhook_changed {
                job.webhook = update.webhook.clone().flatten();
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
            // Manual trigger: force due-now AFTER the field updates above so
            // a schedule recompute in this same update cannot overwrite it.
            if update.trigger_now {
                if !job.enabled {
                    anyhow::bail!("job '{}' is paused; resume it first", id);
                }
                if job.schedule.is_none() {
                    anyhow::bail!(
                        "job '{}' has no schedule (webhook-only): 'run' needs a timer channel — POST its /hooks/{} route instead",
                        id,
                        job.name.as_deref().unwrap_or("?")
                    );
                }
                job.next_run_at = Some(chrono::Utc::now().to_rfc3339());
            }
            self.save_to_disk_inner(&data)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Remove a job, deleting its per-job directory (meta.json + run_logs/)
    /// along with it. Returns the removal audit if the job was found.
    pub fn remove_job(&self, id: &str) -> anyhow::Result<Option<JobRemovalAudit>> {
        // Capture the audit trail (name + run history) before the run log
        // file is deleted — it's the only evidence left once the directory
        // is gone (#77).
        let audit = {
            let data = self.jobs.read();
            data.jobs.iter().find(|j| j.id == id).map(|j| {
                JobRemovalAudit::capture(j.id.clone(), j.name.clone(), self.read_run_log(id, usize::MAX))
            })
        };
        let Some(audit) = audit else {
            return Ok(None);
        };

        let mut data = self.jobs.write();
        let len_before = data.jobs.len();
        data.jobs.retain(|j| j.id != id);
        if data.jobs.len() < len_before {
            self.save_to_disk_inner(&data)?;
            drop(data);
            audit.log("job removed");
            Ok(Some(audit))
        } else {
            // Raced with a concurrent removal between the read snapshot and
            // the write lock — nothing to delete.
            Ok(None)
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
                job.schedule.as_ref(),
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
            if job.schedule.as_ref().is_some_and(ScheduleSpec::is_at) {
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
        let to_delete: Vec<(String, Option<String>)> = data
            .jobs
            .iter()
            .filter(|j| j.max_runs.is_some_and(|max| j.completed_runs >= max) && j.delete_after_run)
            .map(|j| (j.id.clone(), j.name.clone()))
            .collect();
        if to_delete.is_empty() {
            return Vec::new();
        }
        // Capture each job's run history before its directory disappears
        // (#77) — this path is silent to any delivery channel by design (the
        // job's own output was already delivered), so the journal audit
        // line below is the only record.
        let audits: Vec<JobRemovalAudit> = to_delete
            .iter()
            .map(|(id, name)| JobRemovalAudit::capture(id.clone(), name.clone(), self.read_run_log(id, usize::MAX)))
            .collect();
        let ids: Vec<String> = to_delete.into_iter().map(|(id, _)| id).collect();
        data.jobs.retain(|j| !ids.contains(&j.id));
        let _ = self.save_to_disk_inner(&data);
        drop(data);
        tracing::info!(count = ids.len(), "auto-deleted completed one-shot jobs");
        for audit in &audits {
            audit.log("job auto-deleted (max_runs + delete_after_run)");
        }
        ids
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
    /// P1-B2: per-job `{jobs_root}/{id}/run_logs/{id}.jsonl` (log lives
    /// beside its meta.json). Falls back to the shared `run_logs/` dir
    /// when a legacy log exists there (pre-migration reads keep working).
    fn run_log_path(&self, job_id: &str) -> PathBuf {
        let file = format!("{}.jsonl", crate::ids::dir_name(job_id));
        let per_job = self
            .jobs_root
            .join(crate::ids::dir_name(job_id))
            .join("run_logs")
            .join(&file);
        if per_job.exists() {
            return per_job;
        }
        let shared = self.jobs_root.join("run_logs").join(&file);
        if shared.exists() {
            return shared;
        }
        per_job
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

    /// Append a webhook-triggered run to the job's history WITHOUT touching
    /// scheduler state (last_run_at / next_run_at belong to the timer
    /// channel — a webhook hit must not shift an "every 30m" cadence, and a
    /// webhook-only job has no cadence at all). §3.4: webhook runs land in
    /// the same single history source with `trigger: "webhook"`.
    pub fn record_webhook_run(&self, job_id: &str, record: RunRecord) {
        self.append_run_log_inner(job_id, &record);
    }
    // ── Webhook projection on Scheduler (#151 Phase 8a: moved back from
    // webhook.rs — an inherent impl in another module cannot touch the
    // private `jobs` field; reachable as `scheduler.webhook_jobs()` unchanged)
    // ─────────────────────────────────────────────────────────────────────
    pub fn webhook_jobs(&self) -> Vec<WebhookJobDef> {
        let mut out: Vec<WebhookJobDef> = Vec::new();
        let mut seen_routes: std::collections::HashSet<String> = std::collections::HashSet::new();
        for j in self.jobs.read().jobs.iter() {
            let Some(wh) = j.webhook.as_ref() else { continue };
            let route = match j.name.as_deref() {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    tracing::warn!(job_id = %j.id, "webhook job without a name: no route");
                    continue;
                }
            };
            if route == "agent" || route == "wake" {
                continue;
            }
            if !is_route_slug(&route) {
                tracing::warn!(job_id = %j.id, route = %route, "webhook route name is not a URL-safe slug [a-z0-9-]: skipped");
                continue;
            }
            if !seen_routes.insert(route.clone()) {
                tracing::warn!(route = %route, "duplicate webhook route (job names must be unique): keeping first");
                continue;
            }
            if wh.secret.is_empty() {
                tracing::warn!(job_id = %j.id, route = %route, "webhook job without a secret: rejected at load (design §3.4.1)");
                continue;
            }
            out.push(WebhookJobDef {
                id: j.id.clone(),
                route,
                secret: wh.secret.clone(),
                auth: wh.auth_kind(),
                delivery: j.delivery.clone(),
                prompt_template: j.prompt.clone(),
                events: wh.events.clone(),
                filters: wh.filters.clone(),
                payload_off: wh.payload_off,
            });
        }
        out
    }
    /// Resolve a delivery config to (channel_type, account_id, recipient),
    /// or `None` to skip delivery. Single resolution path shared by the
    /// cron and webhook dispatch routes (#78) — previously duplicated with
    /// diverging behavior (the webhook route ignored `delivery` entirely).
    pub async fn resolve_delivery(
        &self,
        delivery: &DeliveryConfig,
    ) -> Option<(String, String, Option<String>)> {
        let (ch_type, acc_id) = match delivery.mode {
            DeliveryMode::None => return None,
            DeliveryMode::Fixed => {
                let channel = delivery.channel.clone()?;
                let account = delivery
                    .account_id
                    .clone()
                    .unwrap_or_else(|| "default".to_string());
                (channel, account)
            }
            DeliveryMode::Last => {
                let last = self.last_channel.lock().await.clone();
                match last {
                    Some(key) => match key.split_once(':') {
                        Some((ch, acc)) => (ch.to_string(), acc.to_string()),
                        None => {
                            tracing::warn!(key = %key, "invalid last_channel format");
                            return None;
                        }
                    },
                    None => {
                        tracing::warn!("no target channel for scheduled response");
                        return None;
                    }
                }
            }
        };
        let recipient = match &delivery.to {
            Some(to) => Some(to.clone()),
            None => self.last_recipient.lock().await.clone(),
        };
        Some((ch_type, acc_id, recipient))
    }

    /// Generate a new job FQID (`<ns>/job/<uuidv7>`).
    fn generate_id(&self) -> String {
        crate::ids::Fqid::new(&self.namespace, crate::ids::TYPE_JOB).to_string()
    }
}

impl Scheduler {
    /// Per-job meta.json path: `{jobs_root}/{dir_name(id)}/meta.json`.
    fn job_meta_path(&self, id: &str) -> PathBuf {
        self.jobs_root
            .join(crate::ids::dir_name(id))
            .join("meta.json")
    }

    /// P1-B2: persist each job as its own `{jobs_root}/{id}/meta.json`.
    /// Jobs removed from `data` get their meta.json deleted. The legacy
    /// single-file jobs.json is never written anymore (read fallback only).
    fn save_to_disk_inner(&self, data: &JobsFile) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.jobs_root)?;
        let mut seen_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for job in &data.jobs {
            let meta = self.job_meta_path(&job.id);
            let dir = meta
                .parent()
                .ok_or_else(|| anyhow::anyhow!("job meta path without parent"))?
                .to_path_buf();
            std::fs::create_dir_all(&dir)?;
            let json = serde_json::to_string_pretty(job)?;
            let tmp = dir.join("meta.json.tmp");
            std::fs::write(&tmp, &json)?;
            std::fs::rename(&tmp, &meta)?;
            seen_dirs.insert(dir);
        }
        // Delete per-job dirs whose job is gone from the dataset. #77: this
        // used to keep run_logs/ around (deleting only meta.json) so
        // debugging a deleted job's last run stayed possible — but that
        // left an orphan directory with no meta.json behind forever (worse
        // than the residue it was meant to avoid; see #77's rejected
        // alternative). Callers that need the run log for audit purposes
        // (remove_job / drain_auto_delete) now capture it *before* calling
        // this function, so the directory can be removed outright here.
        if let Ok(rd) = std::fs::read_dir(&self.jobs_root) {
            for entry in rd.flatten() {
                let p = entry.path();
                if !p.is_dir() || !p.join("meta.json").is_file() {
                    continue; // run_logs/ and unknown dirs are left alone
                }
                if !seen_dirs.contains(&p) {
                    if let Err(e) = delete_job_dir(&p) {
                        tracing::warn!(err = %e, dir = %p.display(), "jobs store: failed to prune removed job's directory");
                    }
                }
            }
        }
        Ok(())
    }

    /// Hot-reload: check the jobs store changed on disk and reload if so.
    pub fn maybe_reload(&self) {
        let dir_meta = std::fs::metadata(&self.jobs_root).ok();
        let mtime = dir_meta.and_then(|m| m.modified().ok());
        let mut last = self.last_mtime.lock();
        if mtime == *last {
            return;
        }
        let mut jobs = load_jobs_from_dirs(&self.jobs_root);
        // Legacy fallback: only when the dir store is empty.
        if jobs.is_empty() && self.legacy_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&self.legacy_path) {
                // Same Value-level fold as Scheduler::new — see note there.
                let folded: Option<JobsFile> = serde_json::from_str::<serde_json::Value>(&content)
                    .ok()
                    .map(fold_schedule_kind)
                    .map(fold_target_delivery)
                    .and_then(|v| serde_json::from_value(v).ok());
                if let Some(mut parsed) = folded {
                    normalize_schedule_specs(&mut parsed.jobs);
                    jobs = parsed.jobs;
                }
            }
        }
        let mut data = self.jobs.write();
        *data = JobsFile { jobs };
        *last = mtime;
        tracing::info!(count = data.jobs.len(), "hot-reloaded cron jobs from jobs store");
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

            let already_exists = data.jobs.iter().any(|j| {
                j.schedule.as_ref().map(ScheduleSpec::describe).as_deref() == Some(schedule.as_str())
                    && j.prompt == prompt
            });
            if already_exists {
                continue;
            }

            let entry = JobEntry {
                id: self.generate_id(),
                schedule: Some(ScheduleSpec::cron(schedule)),
                webhook: None,
                prompt,
                name: path.file_stem().map(|s| s.to_string_lossy().to_string()),
                tz: None,
                active_hours,
                enabled: true,
                last_run_at: None,
                next_run_at: None,
                created_at: None,
                delivery: parse_target_string(&target),
                last_runs: Vec::new(),
                enabled_tools: None,
                disabled_tools: None,
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
                context_policy: crate::config::scheduler::ContextPolicy::Inject,
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

