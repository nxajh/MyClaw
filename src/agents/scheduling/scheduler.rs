//! Scheduler — Cron job scheduling, storage, and execution events.
//!
//! The Scheduler is the single owner of all cron job data. It:
//!   - Loads and persists jobs from `{jobs_root}/{id}/meta.json` (P1-B2)
//!   - Hot-reloads when the file changes on disk
//!   - Sends timing events (cron, distill) via mpsc channel
//!   - Provides CRUD methods for cronjob_tool
//!   - Records run results
//!
//! External code interacts through `SharedScheduler` (Arc<Scheduler>).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Timelike;
use parking_lot::{Mutex as ParkMutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::agents::orchestrator::SchedulerEvent;
use crate::agents::scheduling::cron_types::{DeliveryConfig, RunRecord, RunStatus, ScheduleKind};
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};
use crate::config::scheduler::WebhookConfig;

/// Shared handle to the Scheduler for concurrent access.
pub type SharedScheduler = Arc<Scheduler>;

// ── JobEntry ────────────────────────────────────────────────────────────────

/// A single cron job stored in `{jobs_root}/{id}/meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEntry {
    /// Unique ID — FQID `<ns>/job/<uuidv7>`; legacy 12-hex ids remain readable.
    pub id: String,
    /// Cron expression (6-field: sec min hour day month weekday).
    /// e.g. "0 0 9 * * *" = every day at 09:00.
    /// Orthogonal trigger model: optional — a job with no schedule never
    /// fires from the timer (webhook-only or archived jobs).
    #[serde(default)]
    pub schedule: Option<String>,
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
    /// Run history (in-memory cache of recent entries; the durable source is
    /// the per-job run log JSONL — `read_run_log`). Not serialized: meta.json
    /// no longer duplicates what the run log already stores.
    #[serde(default, skip_serializing)]
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
    /// Webhook trigger channel (orthogonal to `schedule` — a job may have
    /// either, both, or neither). When present, the HTTP server registers
    /// `POST /hooks/{name}` for this job; `name` is the URL-safe route slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookDef>,
}

/// Webhook trigger channel on a JobEntry (design doc §3.4 orthogonal model:
/// triggering is an optional capability, not a job type). Route derives from
/// the job name: `/hooks/{name}` — name is the external URL contract and must
/// be a URL-safe slug, unique across webhook-enabled jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDef {
    /// Auth method: "hmac" (default) or "bearer".
    #[serde(default = "default_webhook_auth")]
    pub auth: String,
    /// HMAC secret or Bearer token. Required — jobs with a webhook channel
    /// but no secret are rejected at load.
    pub secret: String,
    /// Event-type whitelist (e.g. ["issues", "issue_comment"]). Event type
    /// is extracted at request time via the header/payload fallback chain;
    /// non-matching events get 200 "ignored" + a skipped history entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    /// Simple condition filters, AND semantics: all must pass to trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Vec<WebhookFilter>>,
    /// Disable the automatic full-payload appendix (§3.4.1). Default false =
    /// render template placeholders, then append the pretty-printed payload
    /// (truncated to 4000 chars) for context.
    #[serde(default)]
    pub payload_off: bool,
}

/// A single condition in a webhook channel's `filters` list.
/// Field navigates the payload by dot path (e.g. "action", "issue.state").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookFilter {
    pub field: String,
    /// Value equality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    /// Regex match (unanchored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    /// Negate the matched condition (field must NOT equal/match).
    #[serde(default)]
    pub not: bool,
}

fn default_webhook_auth() -> String {
    "hmac".to_string()
}

impl WebhookDef {
    pub fn auth_kind(&self) -> WebhookAuth {
        match self.auth.as_str() {
            "bearer" => WebhookAuth::Bearer,
            _ => WebhookAuth::Hmac,
        }
    }
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
    /// New schedule string; None here means "unchanged" — pair with
    /// `schedule_changed` (Some(None) clears the timer channel).
    pub schedule: Option<String>,
    pub schedule_changed: bool,
    pub schedule_kind: Option<crate::agents::scheduling::cron_types::ScheduleKind>,
    /// Webhook channel; pair with `webhook_changed` (Some(None) removes it).
    pub webhook: Option<Option<WebhookDef>>,
    pub webhook_changed: bool,
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
    /// Force the job due now: sets `next_run_at` to the current time so the
    /// next scheduler tick fires it. Requires the job to be enabled.
    pub trigger_now: bool,
}

/// The legacy single-file jobs structure (P1-B2: read fallback only).
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
                if let Ok(parsed) = serde_json::from_str::<JobsFile>(&content) {
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
                            target = %j.target,
                            "cron job triggered"
                        );
                        let _ = self.event_tx.send(SchedulerEvent::Cron(crate::agents::orchestrator::CronTrigger {
                            session_key: format!("_job_{}", crate::ids::bare_dir_name(&j.id)),
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
                                job.next_run_at = compute_next_run_full(
                                    job.schedule_kind.as_ref(),
                                    job.schedule.as_deref(),
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

    /// Jobs with a webhook channel (orthogonal model: `webhook.is_some()`,
    /// independent of `schedule` — a job can be timer-only, webhook-only, or
    /// both). Projected into the server's `WebhookJobDef` view; route derives
    /// from the job name. Jobs whose name is not a URL-safe slug, or whose
    /// name collides with another webhook job, are skipped with a warning.
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
                tracing::warn!(job_id = %j.id, route = %route, "webhook route collides with a built-in /hooks endpoint: skipped");
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
                target: j.target.clone(),
                prompt_template: j.prompt.clone(),
                events: wh.events.clone(),
                filters: wh.filters.clone(),
                payload_off: wh.payload_off,
            });
        }
        out
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
        entry.next_run_at = compute_next_run_full(
            entry.schedule_kind.as_ref(),
            entry.schedule.as_deref(),
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
                job.schedule = update.schedule;
                job.schedule_kind = update.schedule_kind.clone();
                job.next_run_at = compute_next_run_full(
                    job.schedule_kind.as_ref(),
                    job.schedule.as_deref(),
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
                job.next_run_at = compute_next_run_full(
                    job.schedule_kind.as_ref(),
                    job.schedule.as_deref(),
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
                    job.next_run_at = compute_next_run_full(
                        job.schedule_kind.as_ref(),
                        job.schedule.as_deref(),
                        job.last_run_at.as_deref(),
                        job.tz.as_deref().unwrap_or(&self.timezone),
                    );
                }
            }
            if let Some(delivery) = update.delivery {
                job.delivery = Some(delivery);
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
            job.next_run_at = compute_next_run_full(
                job.schedule_kind.as_ref(),
                job.schedule.as_deref(),
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

    /// Generate a new job FQID (`<ns>/job/<uuidv7>`).
    fn generate_id(&self) -> String {
        crate::ids::Fqid::new(&self.namespace, crate::ids::TYPE_JOB).to_string()
    }
}

// ── Persistence ─────────────────────────────────────────────────────────────

/// Load every `{jobs_root}/{dir}/meta.json` (P1-B2 directory-based store).
/// Malformed entries are skipped. Sorted by id for stable ordering.
fn load_jobs_from_dirs(jobs_root: &Path) -> Vec<JobEntry> {
    let entries = match std::fs::read_dir(jobs_root) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut jobs = Vec::new();
    for entry in entries.flatten() {
        let meta = entry.path().join("meta.json");
        if !meta.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&meta) {
            Ok(c) => c,
            Err(_) => continue,
        };
        match serde_json::from_str::<JobEntry>(&content) {
            Ok(mut job) => {
                // §3.4: name is required. Legacy meta.json without a name
                // backfills with the bare uuid (which is a valid slug) and
                // warns; persisted back on the next save.
                if job.name.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    let fallback = crate::ids::bare_dir_name(&job.id);
                    tracing::warn!(job_id = %job.id, fallback = %fallback,
                        "jobs store: meta.json without name, backfilling with bare uuid");
                    job.name = Some(fallback);
                }
                jobs.push(job)
            }
            Err(e) => tracing::warn!(
                path = %meta.display(),
                err = %e,
                "jobs store: skipping malformed meta.json"
            ),
        }
    }
    jobs.sort_by(|a, b| a.id.cmp(&b.id));
    jobs
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
        // Delete per-job dirs whose job is gone from the dataset.
        if let Ok(rd) = std::fs::read_dir(&self.jobs_root) {
            for entry in rd.flatten() {
                let p = entry.path();
                if !p.is_dir() || !p.join("meta.json").is_file() {
                    continue; // run_logs/ and unknown dirs are left alone
                }
                if !seen_dirs.contains(&p) {
                    // Only remove when the dir holds no run logs (keep logs).
                    let has_logs = p.join("run_logs").is_dir();
                    if has_logs {
                        if let Err(e) = std::fs::remove_file(p.join("meta.json")) {
                            tracing::warn!(err = %e, "jobs store: failed to prune stale meta.json");
                        }
                    } else if let Err(e) = std::fs::remove_dir_all(&p) {
                        tracing::warn!(err = %e, "jobs store: failed to prune job dir");
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
                if let Ok(parsed) = serde_json::from_str::<JobsFile>(&content) {
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

            let already_exists = data
                .jobs
                .iter()
                .any(|j| j.schedule.as_deref() == Some(schedule.as_str()) && j.prompt == prompt);
            if already_exists {
                continue;
            }

            let entry = JobEntry {
                id: self.generate_id(),
                schedule: Some(schedule),
                webhook: None,
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
    compute_next_run_inner(None, Some(schedule), last_run, tz_name)
}

/// Full compute with ScheduleKind support.
pub fn compute_next_run_full(
    kind: Option<&ScheduleKind>,
    schedule: Option<&str>,
    last_run: Option<&str>,
    tz_name: &str,
) -> Option<String> {
    compute_next_run_inner(kind, schedule, last_run, tz_name)
}

fn compute_next_run_inner(
    kind: Option<&ScheduleKind>,
    schedule: Option<&str>,
    last_run: Option<&str>,
    tz_name: &str,
) -> Option<String> {
    // Orthogonal trigger model: no schedule = never timer-due (webhook-only
    // or archived jobs); the HTTP server handles their other channel.
    let schedule = schedule?;
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

// ── Webhook channel view + template rendering (migrated from the removed
// webhook_loader.rs per §4 checklist) ───────────────────────────────────────

/// Webhook job definition (server-facing view projected from a JobEntry's
/// `webhook` channel). Route derives from the job name: `POST /hooks/{route}`.
#[derive(Debug, Clone)]
pub struct WebhookJobDef {
    /// Owning job id — FQID `<ns>/job/<uuid>`; used for the `_job_{uuid}`
    /// session key.
    pub id: String,
    /// Route segment (the job's name, URL-safe slug): full route is
    /// `POST /hooks/{route}`.
    pub route: String,
    /// HMAC secret or Bearer token (required — validated at projection).
    pub secret: String,
    /// Auth method: hmac (default) or bearer.
    pub auth: WebhookAuth,
    /// Output delivery target: last | none | channel name (from the job).
    pub target: String,
    /// Prompt template (the job's prompt field), with `{{a.b.c}}`
    /// placeholders rendered from the payload.
    pub prompt_template: String,
    /// Event-type whitelist (optional; non-matching events are ignored).
    pub events: Option<Vec<String>>,
    /// Condition filters (AND semantics, optional).
    pub filters: Option<Vec<WebhookFilter>>,
    /// Disable the automatic full-payload appendix.
    pub payload_off: bool,
}

/// Webhook auth method.
#[derive(Debug, Clone, PartialEq)]
pub enum WebhookAuth {
    /// HMAC-SHA256 via the X-Hub-Signature-256 header.
    Hmac,
    /// Bearer token via the Authorization header.
    Bearer,
}

/// Route-segment validity: lowercase URL-safe slug `[a-z0-9-]`, 1-64 chars.
/// Names are user-facing, so validation is strict (no `_`, no unicode).
pub fn is_route_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Render a prompt template, replacing `{{path.to.field}}` placeholders with
/// values from the JSON payload.
///
/// - `{{issue.title}}` → reads `issue.title` from the payload
/// - `{{commits[0].message}}` → array indexing supported
/// - missing fields render as an empty string
pub fn render_template(template: &str, payload: &serde_json::Value) -> String {
    let mut result = template.to_string();
    let mut start = 0;

    while let Some(open) = result[start..].find("{{") {
        let abs_open = start + open;
        let Some(close) = result[abs_open..].find("}}") else {
            break;
        };
        let abs_close = abs_open + close;

        let key = result[abs_open + 2..abs_close].trim();
        let replacement = match navigate_json_value(payload, key) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::Bool(b)) => b.to_string(),
            Some(serde_json::Value::Null) => String::new(),
            Some(other) => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
            None => String::new(),
        };

        let placeholder_len = abs_close + 2 - abs_open; // includes {{ and }}
        result.replace_range(abs_open..abs_open + placeholder_len, &replacement);
        // Move past the replacement to avoid infinite loops
        start = abs_open + replacement.len();
    }

    result
}

/// Navigate a JSON value by dot-separated path with array index support.
fn navigate_json_value<'a>(
    val: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = val;
    for segment in path.split('.') {
        if let Some(bracket) = segment.find('[') {
            let field = &segment[..bracket];
            if !field.is_empty() {
                current = current.get(field)?;
            }
            let rest = &segment[bracket..];
            for idx_str in rest.split(']').filter(|s| !s.is_empty()) {
                let idx: usize = idx_str.trim_start_matches('[').parse().ok()?;
                current = current.get(idx)?;
            }
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current)
}

// ── Webhook safety stack (§3.4.1) ──────────────────────────────────────────

/// Per-route + per-IP request guard: rate limit, in-flight cap, delivery-id
/// idempotency cache. Cheap checks first; body-size caps live in
/// `collect_body_capped`.
#[derive(Default)]
pub struct WebhookGuard {
    /// (route, ip) → request timestamps within the sliding 60s window.
    rate: std::sync::Mutex<std::collections::HashMap<(String, String), std::collections::VecDeque<std::time::Instant>>>,
    /// route → in-flight request count (cap 8 per route).
    inflight: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    /// delivery id → first-seen instant (dedupe window 1h).
    deliveries: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// RAII in-flight slot release.
pub struct InflightGuard {
    route: String,
    guard: Arc<WebhookGuard>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.guard.inflight.lock() {
            if let Some(c) = m.get_mut(&self.route) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    m.remove(&self.route);
                }
            }
        }
    }
}

const WEBHOOK_RATE_MAX: usize = 120;
const WEBHOOK_RATE_WINDOW_SECS: u64 = 60;
const WEBHOOK_CONCURRENCY_MAX: usize = 8;
const WEBHOOK_DELIVERY_TTL_SECS: u64 = 3600;
/// Hard body cap for custom webhook routes.
pub const WEBHOOK_BODY_LIMIT: usize = 256 * 1024;
/// Body read timeout.
pub const WEBHOOK_BODY_TIMEOUT_SECS: u64 = 15;
/// Allowed clock skew for V2 timestamped signatures.
const WEBHOOK_V2_MAX_SKEW_SECS: i64 = 300;

impl WebhookGuard {
    /// Sliding-window rate check: ≤120 requests / 60s per (route, ip).
    pub fn check_rate(&self, route: &str, ip: &str) -> bool {
        let now = std::time::Instant::now();
        let Ok(mut m) = self.rate.lock() else { return true };
        let key = (route.to_string(), ip.to_string());
        let win = m.entry(key).or_default();
        let cutoff = now - std::time::Duration::from_secs(WEBHOOK_RATE_WINDOW_SECS);
        while win.front().is_some_and(|t| *t < cutoff) {
            win.pop_front();
        }
        if win.len() >= WEBHOOK_RATE_MAX {
            return false;
        }
        win.push_back(now);
        true
    }

    /// Acquire an in-flight slot; None when the route already has 8 running.
    pub fn acquire(&self, route: &str, guard: Arc<WebhookGuard>) -> Option<InflightGuard> {
        let Ok(mut m) = self.inflight.lock() else { return None };
        let c = m.entry(route.to_string()).or_insert(0);
        if *c >= WEBHOOK_CONCURRENCY_MAX {
            return None;
        }
        *c += 1;
        Some(InflightGuard { route: route.to_string(), guard })
    }

    /// Delivery-id idempotency: true when this id is seen for the first time
    /// within the 1h TTL (and is now recorded); false = duplicate.
    pub fn check_delivery(&self, delivery_id: &str) -> bool {
        let now = std::time::Instant::now();
        let Ok(mut m) = self.deliveries.lock() else { return true };
        m.retain(|_, t| now.duration_since(*t).as_secs() < WEBHOOK_DELIVERY_TTL_SECS);
        if m.contains_key(delivery_id) {
            return false;
        }
        m.insert(delivery_id.to_string(), now);
        true
    }
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
    let guard = Arc::new(WebhookGuard::default());

    tracing::info!(
        port = config.port,
        routes = jobs.len(),
        "webhook server started"
    );

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(err = %e, "webhook: accept failed");
                continue;
            }
        };
        let remote_ip = addr.ip().to_string();

        let io = TokioIo::new(stream);
        let ctx = ctx.clone();
        let jobs = jobs.clone();
        let global_secret = global_secret.clone();
        let guard = guard.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                let jobs = jobs.clone();
                let global_secret = global_secret.clone();
                let guard = guard.clone();
                let ip = remote_ip.clone();
                async move { handle_request(req, ctx, &jobs, &global_secret, &guard, ip).await }
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
    guard: &Arc<WebhookGuard>,
    remote_ip: String,
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

    // ── Custom webhook routes: /hooks/{name}, name = job name slug ────
    let Some(route_name) = path.strip_prefix("/hooks/") else {
        return ok_response(StatusCode::NOT_FOUND, "no webhook at this path");
    };
    if route_name.contains('/') {
        return ok_response(StatusCode::NOT_FOUND, "no webhook at this path");
    }
    let job = match jobs.iter().find(|j| j.route == route_name) {
        Some(j) => j,
        None => return ok_response(StatusCode::NOT_FOUND, "no webhook at this path"),
    };

    // ── Safety stack (§3.4.1): method/Content-Type → rate limit →
    // concurrency → (auth → idempotency → body below). Cheap rejections
    // come first. ─────────
    // Content-Type: JSON parses as payload; text/form bodies pass through
    // verbatim as string payloads (§3.4.1); multipart is rejected (we do
    // not decode it and should not read large uploads).
    if !acceptable_content_type(req.headers().get("Content-Type").and_then(|v| v.to_str().ok())) {
        return ok_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json, text/* or application/x-www-form-urlencoded",
        );
    }
    if !guard.check_rate(&job.route, &remote_ip) {
        return ok_response(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
    }
    let _inflight = match guard.acquire(&job.route, Arc::clone(guard)) {
        Some(g) => g,
        None => {
            tracing::warn!(route = %job.route, "webhook: route at concurrency cap");
            return ok_response(StatusCode::SERVICE_UNAVAILABLE, "route busy");
        }
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

    // Event-type header (§3.4.1 fallback chain head; payload fallbacks are
    // consulted after the body is parsed).
    let event_header = req
        .headers()
        .get("X-GitHub-Event")
        .or_else(|| req.headers().get("X-GitLab-Event"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // V2 replay protection: optional timestamped signature binding.
    let ts_header = req
        .headers()
        .get("X-MyClaw-Timestamp")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Delivery id for idempotency (provider fallback chain, §3.4.1).
    let delivery_id = req
        .headers()
        .get("X-GitHub-Delivery")
        .or_else(|| req.headers().get("svix-id"))
        .or_else(|| req.headers().get("X-Request-ID"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Pre-read size gate: Content-Length over the cap rejects without reading.
    if let Some(cl) = req
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if cl > WEBHOOK_BODY_LIMIT {
            return ok_response(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }
    }

    // Collect body bytes (256KB cap + 15s read timeout).
    let body_bytes = match collect_body_capped(req.into_body()).await {
        Ok(b) => b,
        Err(e) if e.to_string().contains("too large") => {
            return ok_response(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }
        Err(e) if e.to_string().contains("timeout") => {
            return ok_response(StatusCode::REQUEST_TIMEOUT, "body read timeout");
        }
        Err(e) => {
            tracing::warn!(err = %e, "webhook: failed to read body");
            return ok_response(StatusCode::BAD_REQUEST, "failed to read body");
        }
    };

    // Verify auth per-route (§3.4.1: secret is required on every custom
    // route — projection already rejected empty secrets, defense here).
    if job.secret.is_empty() {
        return ok_response(StatusCode::INTERNAL_SERVER_ERROR, "route misconfigured");
    }
    match job.auth {
        WebhookAuth::Hmac => {
            // V2 replay protection: when the client sends X-MyClaw-Timestamp,
            // the signature must cover "v2:{ts}:{body}" and the timestamp
            // must be within ±300s. Without the header, plain GitHub-style
            // body signature (V1) applies.
            if let Some(ts) = ts_header.as_deref() {
                let skew = ts
                    .parse::<i64>()
                    .ok()
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|dt| (chrono::Utc::now() - dt).num_seconds().abs())
                    .unwrap_or(i64::MAX);
                if skew > WEBHOOK_V2_MAX_SKEW_SECS {
                    tracing::warn!(route = %job.route, "webhook: V2 timestamp outside allowed skew");
                    return ok_response(StatusCode::UNAUTHORIZED, "stale timestamp");
                }
                let signed = format!("v2:{}:{}", ts, String::from_utf8_lossy(&body_bytes));
                let ok = sig_header
                    .as_deref()
                    .map(|sig| verify_hmac_signature(signed.as_bytes(), &job.secret, sig))
                    .unwrap_or(false);
                if !ok {
                    tracing::warn!(route = %job.route, "webhook: V2 HMAC verification failed");
                    return ok_response(StatusCode::UNAUTHORIZED, "invalid signature");
                }
            } else {
                match sig_header {
                    Some(ref sig) if !verify_hmac_signature(&body_bytes, &job.secret, sig) => {
                        tracing::warn!(route = %job.route, "webhook: HMAC verification failed");
                        return ok_response(StatusCode::UNAUTHORIZED, "invalid signature");
                    }
                    None => {
                        tracing::warn!(route = %job.route, "webhook: missing signature header");
                        return ok_response(StatusCode::UNAUTHORIZED, "missing signature");
                    }
                    _ => {}
                }
            }
        }
        WebhookAuth::Bearer => {
            let expected = format!("Bearer {}", job.secret);
            match auth_header {
                Some(ref h) if h.as_str() == expected => {}
                _ => {
                    tracing::warn!(route = %job.route, "webhook: Bearer auth failed");
                    return ok_response(StatusCode::UNAUTHORIZED, "invalid token");
                }
            }
        }
    }

    // Idempotency: duplicate delivery ids are acknowledged and dropped.
    if let Some(did) = delivery_id.as_deref() {
        if !guard.check_delivery(did) {
            tracing::info!(route = %job.route, delivery_id = %did, "webhook: duplicate delivery, ignored");
            return ok_response(StatusCode::OK, "duplicate");
        }
    }

    tracing::info!(route = %job.route, "webhook triggered");

    // Parse payload: JSON bodies become objects; anything else is passed
    // through as a plain string (§3.4.1 — no silent Null).
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(
            String::from_utf8_lossy(&body_bytes).to_string(),
        ),
    };

    // Event whitelist + condition filters (§3.4.1). Non-matching requests
    // are acknowledged 200 "ignored" and leave a Skipped history entry.
    let event_type = extract_event_type(event_header.as_deref(), &payload);
    let ignore_reason = if let Some(events) = job.events.as_ref() {
        let et = event_type.as_deref().unwrap_or("");
        if !events.iter().any(|e| e == et) {
            Some(format!("event '{}' not in whitelist", et))
        } else {
            None
        }
    } else {
        None
    }
    .or_else(|| {
        job.filters
            .as_ref()
            .filter(|fs| !fs.iter().all(|f| filter_matches(f, &payload)))
            .map(|_| "filters not satisfied".to_string())
    });
    if let Some(reason) = ignore_reason {
        tracing::info!(route = %job.route, reason = %reason, "webhook: ignored");
        if let Some(sched) = ctx.ctx.scheduler.as_ref() {
            sched.record_webhook_run(
                &job.id,
                RunRecord {
                    run_at: chrono::Utc::now().to_rfc3339(),
                    status: RunStatus::Skipped,
                    trigger: Some("webhook".to_string()),
                    error: Some(reason),
                    payload: Some(pretty_payload(&payload, 8192)),
                    ..Default::default()
                },
            );
        }
        return ok_response(StatusCode::OK, "ignored");
    }

    // Render template with payload, then append the full payload for context
    // (§3.4.1: full-payload default, 4000-char truncation, {{payload}} and
    // {{event_type}} reserved placeholders).
    let mut prompt = render_template(&job.prompt_template, &payload);
    if prompt.contains("{{payload}}") {
        prompt = prompt.replace("{{payload}}", &pretty_payload(&payload, 4000));
    } else if let Some(et) = event_type.as_deref() {
        prompt = prompt.replace("{{event_type}}", et);
    }
    if !job.payload_off && !job.prompt_template.contains("{{payload}}") {
        let appendix = pretty_payload(&payload, 4000);
        if !appendix.is_empty() {
            prompt.push_str("\n\n--- webhook payload ---\n");
            prompt.push_str(&appendix);
        }
    }

    let session_key = format!(
        "_job_{}",
        crate::ids::bare_dir_name(&job.id)
    );
    let started = std::time::Instant::now();
    let result = run_scheduled_task(&ctx, &session_key, &prompt).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    // History record (§3.4: trigger field + webhook audit fields).
    if let Some(sched) = ctx.ctx.scheduler.as_ref() {
        let mut record = RunRecord::now(match &result {
            Ok(_) => RunStatus::Ok,
            Err(_) => RunStatus::Error,
        });
        record.trigger = Some("webhook".to_string());
        record.duration_ms = duration_ms;
        record.payload = Some(pretty_payload(&payload, 8192));
        record.prompt_head = Some(prompt.chars().take(512).collect());
        if let Err(e) = &result {
            record = record.with_error(e.to_string());
        }
        sched.record_webhook_run(&job.id, record);
    }

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
    // JSON API: cheap Content-Type rejection before auth/body work.
    let ct_json = req
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false);
    if !ct_json {
        return ok_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Content-Type must be application/json");
    }

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

/// `POST /hooks/wake` — Wake endpoint (kept for URL contract; the old
/// heartbeat wakeup mechanism was removed with the heartbeat system).
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

    // TODO: enqueue a system event to wake the agent loop
    // For now, just acknowledge.
    ok_response(StatusCode::OK, "wake acknowledged")
}

// ── Auth helpers ───────────────────────────────────────────────────────────

/// Extract the event type: header fallback chain first (X-GitHub-Event →
/// X-GitLab-Event, captured before body consumption), then payload fields
/// (`event_type` → `type`) — hermes 5-level chain, §3.4.1.
fn extract_event_type(event_header: Option<&str>, payload: &serde_json::Value) -> Option<String> {
    if let Some(h) = event_header.filter(|h| !h.is_empty()) {
        return Some(h.to_string());
    }
    payload
        .get("event_type")
        .or_else(|| payload.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Evaluate a single webhook filter condition against the payload.
/// AND semantics across the filter list; `not` negates the match.
fn filter_matches(
    f: &crate::agents::scheduling::scheduler::WebhookFilter,
    payload: &serde_json::Value,
) -> bool {
    let mut cur = payload;
    for seg in f.field.split('.') {
        cur = match cur.get(seg) {
            Some(v) => v,
            None => return f.not, // missing field: only a `not` filter passes
        };
    }
    // Strings compare directly; numbers/bools against their string form.
    let actual = match cur {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => return f.not,
    };
    let matched = if let Some(eq) = f.equals.as_deref() {
        actual == eq
    } else if let Some(re) = f.matches.as_deref() {
        regex::Regex::new(re).map(|r| r.is_match(&actual)).unwrap_or(false)
    } else {
        true
    };
    matched != f.not
}

/// Pretty-print a payload for prompt context, truncated to `max_chars`.
fn pretty_payload(payload: &serde_json::Value, max_chars: usize) -> String {
    let s = serde_json::to_string_pretty(payload).unwrap_or_default();
    if s.chars().count() <= max_chars {
        s
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}\n…[truncated]", truncated)
    }
}

/// Content-Type gate (§3.4.1 first stack step): application/json parses as
/// the payload; text/* and form-urlencoded bodies pass through verbatim as
/// string payloads; anything else (missing, multipart, …) is rejected cheap
/// before any body read.
fn acceptable_content_type(ct: Option<&str>) -> bool {
    let Some(ct) = ct else { return false };
    let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    mime == "application/json" || mime.starts_with("text/") || mime == "application/x-www-form-urlencoded"
}

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

/// Capped body read for custom webhook routes (§3.4.1): 256KB hard limit,
/// 15s read timeout. `Err(anyhow!("too large"))` maps to 413 upstream.
async fn collect_body_capped<B>(body: B) -> anyhow::Result<Bytes>
where
    B: hyper::body::Body,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let fut = async {
        use http_body_util::BodyExt;
        let mut body = std::pin::pin!(body);
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        while let Some(frame) = body.as_mut().frame().await {
            let frame = frame?;
            if let Ok(mut data) = frame.into_data() {
                use bytes::Buf;
                while data.has_remaining() {
                    let chunk = data.chunk();
                    if buf.len() + chunk.len() > WEBHOOK_BODY_LIMIT {
                        anyhow::bail!("too large");
                    }
                    let n = chunk.len();
                    buf.extend_from_slice(chunk);
                    data.advance(n);
                }
            }
        }
        Ok(Bytes::from(buf))
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(WEBHOOK_BODY_TIMEOUT_SECS),
        fut,
    )
    .await
    {
        Ok(res) => res,
        Err(_) => anyhow::bail!("body read timeout"),
    }
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

    fn test_scheduler(dir: &std::path::Path) -> SharedScheduler {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        Scheduler::new(
            dir.join("jobs"),
            "test",
            "UTC".to_string(),
            None,
            tx,
            dir.join("last_channel"),
            dir.join("last_recipient"),
        )
    }

    fn test_entry(schedule: &str) -> JobEntry {
        JobEntry {
            id: String::new(),
            schedule: Some(schedule.to_string()),
            webhook: None,
            prompt: "p".to_string(),
            target: "last".to_string(),
            name: Some("test-job".to_string()),
            tz: None,
            active_hours: None,
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
            context_policy: crate::config::scheduler::ContextPolicy::Inject,
        }
    }

    #[test]
    fn trigger_now_forces_next_run_due() {
        let tmp = tempfile::tempdir().unwrap();
        let sched = test_scheduler(tmp.path());
        let id = sched.add_job(test_entry("0 0 10 * * 5")).unwrap();
        let before = sched
            .jobs()
            .iter()
            .find(|j| j.id == id)
            .unwrap()
            .next_run_at
            .clone();
        assert!(before.is_some());

        sched
            .update_job(&id, JobUpdate { trigger_now: true, ..Default::default() })
            .unwrap();

        let after = sched.jobs().iter().find(|j| j.id == id).unwrap().next_run_at.clone();
        let after = chrono::DateTime::parse_from_rfc3339(&after.unwrap()).unwrap();
        assert!(after <= chrono::Utc::now() + chrono::Duration::seconds(2));
    }

    #[test]
    fn trigger_now_rejected_for_paused_job() {
        let tmp = tempfile::tempdir().unwrap();
        let sched = test_scheduler(tmp.path());
        let id = sched.add_job(test_entry("0 0 10 * * 5")).unwrap();
        sched.set_enabled(&id, false).unwrap();
        assert!(sched
            .update_job(&id, JobUpdate { trigger_now: true, ..Default::default() })
            .is_err());
    }

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
    fn silent_marker_ok() {
        assert!(is_silent_ok("cron_ok", "cron"));
        assert!(is_silent_ok("Cron_OK", "cron"));
        assert!(is_silent_ok(" cron_ok ", "cron"));
        assert!(!is_silent_ok("I found something", "cron"));
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

    // ── P1-B2 directory-based job storage ─────────────────────────────────

    #[test]
    fn jobs_store_directory_load_wins_over_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let jobs_root = dir.path().join("jobs");

        // Directory store: one job at {id}/meta.json
        let id_a = "test/job/019fe342aaaa";
        let meta_dir = jobs_root.join(crate::ids::dir_name(id_a));
        std::fs::create_dir_all(&meta_dir).unwrap();
        let entry_a = serde_json::to_string(&test_entry("0 9 * * *")).unwrap();
        let mut parsed: serde_json::Value = serde_json::from_str(&entry_a).unwrap();
        parsed["id"] = serde_json::json!(id_a);
        std::fs::write(meta_dir.join("meta.json"), parsed.to_string()).unwrap();

        // Legacy single file also present with a DIFFERENT job.
        let id_legacy = "test/job/019fe342bbbb";
        let mut legacy_entry = serde_json::to_value(test_entry("0 8 * * *")).unwrap();
        legacy_entry["id"] = serde_json::json!(id_legacy);
        std::fs::create_dir_all(&jobs_root).unwrap();
        std::fs::write(
            jobs_root.join("jobs.json"),
            serde_json::to_string(&JobsFile {
                jobs: vec![serde_json::from_value(legacy_entry).unwrap()],
            })
            .unwrap(),
        )
        .unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let sched = Scheduler::new(
            jobs_root,
            "test",
            "UTC".to_string(),
            None,
            tx,
            dir.path().join("lc"),
            dir.path().join("lr"),
        );
        let jobs = sched.jobs();
        // Directory store wins entirely; legacy job is ignored.
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id_a);
    }

    #[test]
    fn jobs_store_legacy_file_fallback_when_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        let jobs_root = dir.path().join("jobs");
        std::fs::create_dir_all(&jobs_root).unwrap();

        let id_legacy = "test/job/019fe342cccc";
        let mut legacy_entry = serde_json::to_value(test_entry("0 8 * * *")).unwrap();
        legacy_entry["id"] = serde_json::json!(id_legacy);
        std::fs::write(
            jobs_root.join("jobs.json"),
            serde_json::to_string(&JobsFile {
                jobs: vec![serde_json::from_value(legacy_entry).unwrap()],
            })
            .unwrap(),
        )
        .unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let sched = Scheduler::new(
            jobs_root,
            "test",
            "UTC".to_string(),
            None,
            tx,
            dir.path().join("lc"),
            dir.path().join("lr"),
        );
        let jobs = sched.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id_legacy);
    }

    #[test]
    fn jobs_store_writes_per_job_meta_and_prunes_delete() {
        let dir = tempfile::tempdir().unwrap();
        let jobs_root = dir.path().join("jobs");
        let sched = test_scheduler(dir.path());

        // Add a job → meta.json appears under {id}/meta.json.
        let entry = test_entry("0 9 * * *");
        let id = sched.add_job(entry.clone()).unwrap();
        let meta = jobs_root
            .join(crate::ids::dir_name(&id))
            .join("meta.json");
        assert!(meta.is_file());
        // meta.json round-trips the full JobEntry.
        let on_disk: JobEntry =
            serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
        assert_eq!(on_disk.id, id);
        assert_eq!(on_disk.schedule.as_deref(), Some("0 9 * * *"));
        // The legacy single file must NOT be (re)created.
        assert!(!jobs_root.join("jobs.json").exists());

        // Remove → meta.json gone.
        assert!(sched.remove_job(&id).unwrap());
        assert!(!meta.exists());
    }

    // ── Orthogonal trigger model (§3.4) tests ─────────────────────────

    fn wh_entry(name: Option<&str>, secret: &str, schedule: Option<&str>) -> JobEntry {
        JobEntry {
            webhook: Some(WebhookDef {
                auth: "hmac".to_string(),
                secret: secret.to_string(),
                events: None,
                filters: None,
                payload_off: false,
            }),
            name: name.map(|s| s.to_string()),
            schedule: schedule.map(|s| s.to_string()),
            ..test_entry("0 0 9 * * *")
        }
    }

    #[test]
    fn webhook_projection_derives_route_from_name() {
        let tmp = tempfile::tempdir().unwrap();
        let sched = test_scheduler(tmp.path());
        sched
            .add_job(wh_entry(Some("gh-issues"), "s3cret", None))
            .unwrap();
        let jobs = sched.webhook_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].route, "gh-issues");
        assert_eq!(jobs[0].secret, "s3cret");
    }

    #[test]
    fn add_job_rejects_nameless_entry() {
        // §3.4: name is required at the write boundary; the load path
        // backfills legacy files, so the store invariant always holds.
        let tmp = tempfile::tempdir().unwrap();
        let sched = test_scheduler(tmp.path());
        let err = sched.add_job(wh_entry(None, "s", None));
        assert!(err.is_err());
    }

    #[test]
    fn webhook_projection_rejects_bad_slug_secret_and_builtin_names() {
        let tmp = tempfile::tempdir().unwrap();
        let sched = test_scheduler(tmp.path());
        // Empty secret → rejected at load.
        sched
            .add_job(wh_entry(Some("no-secret"), "", None))
            .unwrap();
        // Non-slug name → rejected.
        sched
            .add_job(wh_entry(Some("Bad Slug"), "s", None))
            .unwrap();
        // Built-in route collision → rejected.
        sched.add_job(wh_entry(Some("agent"), "s", None)).unwrap();
        assert!(sched.webhook_jobs().is_empty());
    }

    #[test]
    fn webhook_projection_duplicate_routes_keep_first() {
        let tmp = tempfile::tempdir().unwrap();
        let sched = test_scheduler(tmp.path());
        sched
            .add_job(wh_entry(Some("dup"), "s1", None))
            .unwrap();
        sched
            .add_job(wh_entry(Some("dup"), "s2", None))
            .unwrap();
        let jobs = sched.webhook_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].secret, "s1");
    }

    #[test]
    fn cron_timer_skips_scheduleless_jobs() {
        // compute_next_run_full(None schedule) never yields a due time.
        assert!(compute_next_run_full(None, None, None, "UTC").is_none());
        // And with a schedule it still works.
        assert!(compute_next_run_full(None, Some("0 0 9 * * *"), None, "UTC").is_some());
    }

    // ── Event extraction / filters / payload appendix ─────────────────

    #[test]
    fn extract_event_type_header_chain() {
        let payload = serde_json::json!({"type": "payload-type"});
        assert_eq!(
            extract_event_type(Some("push"), &payload),
            Some("push".to_string())
        );
        assert_eq!(
            extract_event_type(None, &payload),
            Some("payload-type".to_string())
        );
        let payload2 = serde_json::json!({"event_type": "et"});
        assert_eq!(
            extract_event_type(None, &payload2),
            Some("et".to_string())
        );
        assert_eq!(extract_event_type(None, &serde_json::json!({})), None);
    }

    #[test]
    fn filter_matches_equals_matches_not() {
        let payload = serde_json::json!({
            "action": "opened",
            "issue": {"state": "open", "number": 7}
        });
        let f = |field: &str, equals: Option<&str>, matches_: Option<&str>, not: bool| WebhookFilter {
            field: field.to_string(),
            equals: equals.map(|s| s.to_string()),
            matches: matches_.map(|s| s.to_string()),
            not,
        };
        assert!(filter_matches(&f("action", Some("opened"), None, false), &payload));
        assert!(!filter_matches(&f("action", Some("closed"), None, false), &payload));
        assert!(filter_matches(&f("action", Some("closed"), None, true), &payload));
        assert!(filter_matches(&f("issue.state", Some("open"), None, false), &payload));
        assert!(filter_matches(&f("issue.number", Some("7"), None, false), &payload));
        assert!(filter_matches(&f("issue.title", Some("x"), None, true), &payload)); // missing + not
        assert!(filter_matches(&f("action", None, Some("open.*"), false), &payload));
    }

    #[test]
    fn pretty_payload_truncates_to_limit() {
        let big = serde_json::json!({"data": "x".repeat(10_000)});
        let out = pretty_payload(&big, 4000);
        assert!(out.chars().count() < 4100);
        assert!(out.ends_with("…[truncated]"));
        assert_eq!(
            pretty_payload(&serde_json::json!({"a": 1}), 4000),
            "{\n  \"a\": 1\n}"
        );
    }

    // ── Webhook guard (rate / concurrency / idempotency) ──────────────

    #[test]
    fn guard_rate_limit_window() {
        let g = WebhookGuard::default();
        for _ in 0..120 {
            assert!(g.check_rate("r", "1.2.3.4"));
        }
        assert!(!g.check_rate("r", "1.2.3.4")); // over cap
        assert!(g.check_rate("r", "5.6.7.8")); // other IP fine
        assert!(g.check_rate("other", "1.2.3.4")); // other route fine
    }

    #[test]
    fn guard_inflight_cap_and_release() {
        let g = std::sync::Arc::new(WebhookGuard::default());
        let a = g.acquire("r", std::sync::Arc::clone(&g)).unwrap();
        assert!(g.acquire("r", std::sync::Arc::clone(&g)).is_some());
        drop(a);
        // Slot released → acquire succeeds again.
        assert!(g.acquire("r", std::sync::Arc::clone(&g)).is_some());
    }

    #[test]
    fn guard_inflight_enforces_cap_of_8() {
        let g = std::sync::Arc::new(WebhookGuard::default());
        let mut held = Vec::new();
        for _ in 0..8 {
            held.push(g.acquire("r", std::sync::Arc::clone(&g)).unwrap());
        }
        assert!(g.acquire("r", std::sync::Arc::clone(&g)).is_none());
    }

    #[test]
    fn guard_delivery_idempotency() {
        let g = WebhookGuard::default();
        assert!(g.check_delivery("d-1"));
        assert!(!g.check_delivery("d-1")); // duplicate
        assert!(g.check_delivery("d-2"));
    }

    // ── Template rendering tests (migrated from webhook_loader.rs) ────

    #[test]
    fn render_template_simple() {
        let template = "Hello {{name}}!";
        let payload = serde_json::json!({"name": "world"});
        assert_eq!(render_template(template, &payload), "Hello world!");
    }

    #[test]
    fn render_template_nested() {
        let template = "Issue: {{issue.title}} by {{issue.user.login}}";
        let payload = serde_json::json!({
            "issue": {
                "title": "Fix bug",
                "user": {"login": "alice"}
            }
        });
        assert_eq!(
            render_template(template, &payload),
            "Issue: Fix bug by alice"
        );
    }

    #[test]
    fn render_template_array_index() {
        let template = "First commit: {{commits[0].message}}";
        let payload = serde_json::json!({
            "commits": [{"message": "fix"}, {"message": "feat"}]
        });
        assert_eq!(render_template(template, &payload), "First commit: fix");
    }

    #[test]
    fn render_template_missing_field() {
        let template = "Hello {{name}}!";
        let payload = serde_json::json!({});
        assert_eq!(render_template(template, &payload), "Hello !");
    }

    #[test]
    fn render_template_multiple_same_field() {
        let template = "{{x}} and {{x}}";
        let payload = serde_json::json!({"x": "foo"});
        assert_eq!(render_template(template, &payload), "foo and foo");
    }

    #[test]
    fn render_template_no_placeholders() {
        let template = "No placeholders here.";
        let payload = serde_json::json!({});
        assert_eq!(render_template(template, &payload), "No placeholders here.");
    }

    #[test]
    fn render_template_number_and_bool() {
        let template = "Count: {{count}}, Active: {{active}}";
        let payload = serde_json::json!({"count": 42, "active": true});
        assert_eq!(
            render_template(template, &payload),
            "Count: 42, Active: true"
        );
    }

    #[test]
    fn render_template_unclosed_braces_ignored() {
        let template = "Hello {{name} not closed";
        let payload = serde_json::json!({"name": "world"});
        assert_eq!(
            render_template(template, &payload),
            "Hello {{name} not closed"
        );
    }

    #[test]
    fn route_slug_accepts_lowercase_slugs() {
        assert!(is_route_slug("github-issues"));
        assert!(is_route_slug("r2d2"));
        assert!(is_route_slug("a"));
    }

    #[test]
    fn route_slug_rejects_bad_names() {
        assert!(!is_route_slug("")); // empty
        assert!(!is_route_slug("GitHub")); // uppercase
        assert!(!is_route_slug("under_score")); // underscore
        assert!(!is_route_slug("中文")); // unicode
        assert!(!is_route_slug("has/slash"));
        assert!(!is_route_slug(&"x".repeat(65))); // too long
    }
}
