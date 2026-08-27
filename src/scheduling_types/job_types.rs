//! job_types — cron job 契约类型与纯校验函数（L1 基础层）。
//!
//! #151 Phase 8+：cronjob 工具（L3）不得引用 `scheduling_runtime`（L4）。
//! 工具实际用到的数据契约（JobEntry/JobUpdate/WebhookDef/WebhookFilter/
//! JobRemovalAudit）与纯函数（校验/注入扫描/活跃时段判断/时区解析/路由
//! slug 校验）从 scheduler.rs 下沉至此；`scheduling_runtime::scheduler`
//! re-export 保持 L4/L5 既有路径不变。`SchedulerApi` 门面 trait 一并放在
//! 本模块——它的方法签名引用上述 L1 类型，放 api（L0）会反向引用 L1，
//! 与契约同层是合法且最小的落点。

use chrono::{Timelike, Utc};
use serde::{Deserialize, Serialize};

use super::cron_types::{DeliveryConfig, DeliveryMode, FailureAlertConfig, RetryConfig, RunRecord, ScheduleSpec};

// ── JobEntry ────────────────────────────────────────────────────────────────

/// A single cron job stored in `{jobs_root}/{id}/meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEntry {
    /// Unique ID — FQID `<ns>/job/<uuidv7>`; legacy 12-hex ids remain readable.
    pub id: String,
    /// Timer trigger channel (§3.4 orthogonal model): optional polymorphic
    /// object `{kind: cron|every|at, …}`. Legacy plain-string cron
    /// expressions and the legacy `schedule_kind` sibling field are folded
    /// into the canonical object at load. None = no timer channel
    /// (webhook-only or archived jobs).
    #[serde(default)]
    pub schedule: Option<ScheduleSpec>,
    /// Prompt to send to the agent when triggered.
    pub prompt: String,
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
    /// Delivery configuration — the single source of truth for where this
    /// job's output goes (#78). Defaults to `DeliveryMode::Last` when
    /// absent, matching the old implicit `target: "last"` default. Legacy
    /// `target` (+ pre-mode `delivery`) meta.json files are folded into
    /// this shape at load — see `fold_one_target_delivery`.
    #[serde(default)]
    pub delivery: DeliveryConfig,
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
    // ── New fields ──────────────────────────────────────────────────────────
    /// Per-job retry policy for transient errors.
    #[serde(default)]
    pub retry: Option<RetryConfig>,
    /// Per-job failure alert configuration.
    #[serde(default)]
    pub failure_alert: Option<FailureAlertConfig>,
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


fn default_context_policy() -> crate::config::scheduler::ContextPolicy {
    crate::config::scheduler::ContextPolicy::Inject
}

fn default_true() -> bool {
    true
}

/// Update fields for a cron job (all optional, only set fields are updated).
#[derive(Debug, Clone, Default)]
pub struct JobUpdate {
    pub name: Option<String>,
    /// New schedule spec; None here means "unchanged" — pair with
    /// `schedule_changed` (Some(None) clears the timer channel).
    pub schedule: Option<Option<ScheduleSpec>>,
    pub schedule_changed: bool,
    /// Webhook channel; pair with `webhook_changed` (Some(None) removes it).
    pub webhook: Option<Option<WebhookDef>>,
    pub webhook_changed: bool,
    pub prompt: Option<String>,
    pub tz: Option<String>,
    pub active_hours: Option<String>,
    pub enabled: Option<bool>,
    pub delivery: Option<DeliveryConfig>,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Option<Vec<String>>,
    pub retry: Option<RetryConfig>,
    pub failure_alert: Option<FailureAlertConfig>,
    pub max_runs: Option<Option<u32>>,
    pub delete_after_run: Option<bool>,
    pub model: Option<Option<String>>,
    pub provider: Option<Option<String>>,
    /// Force the job due now: sets `next_run_at` to the current time so the
    /// next scheduler tick fires it. Requires the job to be enabled.
    pub trigger_now: bool,
}

/// The legacy single-file jobs structure (P1-B2: read fallback only).

/// Forensic summary of a job at the moment it was removed, captured while
/// its run log JSONL file (the durable evidence source, per §3.4) still
/// exists — the per-job directory is deleted right after (#77).
#[derive(Debug, Clone)]
pub struct JobRemovalAudit {
    pub id: String,
    pub name: String,
    pub run_log_count: usize,
    pub last_status: Option<&'static str>,
    pub last_output_preview: Option<String>,
}

impl JobRemovalAudit {
    pub(crate) fn capture(id: String, name: Option<String>, run_log: Vec<RunRecord>) -> Self {
        let last = run_log.first();
        Self {
            name: name.unwrap_or_else(|| id.clone()),
            id,
            run_log_count: run_log.len(),
            last_status: last.map(|r| r.status.as_str()),
            last_output_preview: last.map(|r| r.output_preview.clone()),
        }
    }

    /// Emit the INFO audit line. Called for both the explicit `remove`
    /// path and the silent `delete_after_run` auto-delete path — the
    /// journal record is what survives once the directory is gone.
    pub(crate) fn log(&self, action: &str) {
        tracing::info!(
            job_id = %self.id,
            job_name = %self.name,
            run_log_entries = self.run_log_count,
            last_status = self.last_status.unwrap_or("(no runs)"),
            last_output_preview = %self.last_output_preview.as_deref().unwrap_or(""),
            "{}",
            action,
        );
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

/// Route-segment validity: lowercase URL-safe slug `[a-z0-9-]`, 1-64 chars.
/// Names are user-facing, so validation is strict (no `_`, no unicode).
/// （自 scheduling_runtime/webhook.rs 下沉：cronjob 工具的 name/webhook 校验。）
pub fn is_route_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Parse the legacy `target` string grammar ("last" | "none" |
/// "channel[:account]") into a [`DeliveryConfig`]. Used to migrate old
/// meta.json files (`fold_one_target_delivery`), and as the grammar for
/// the ad-hoc `/hooks/agent` `target` request field, which is a one-off
/// per-request string and was never part of a job's persisted schema.
pub fn parse_target_string(target: &str) -> DeliveryConfig {
    match target {
        "none" => DeliveryConfig {
            mode: DeliveryMode::None,
            ..Default::default()
        },
        "last" | "" => DeliveryConfig::default(), // mode: Last
        name => {
            let (channel, account_id) = match name.split_once(':') {
                Some((ch, acc)) => (ch.to_string(), Some(acc.to_string())),
                None => (name.to_string(), None),
            };
            DeliveryConfig {
                mode: DeliveryMode::Fixed,
                channel: Some(channel),
                account_id,
                ..Default::default()
            }
        }
    }
}

/// Statically decompose a job's delivery config into the fields
/// `CronTrigger` needs. `Last` mode deliberately leaves channel/account as
/// `None` so `send_to_target_internal` resolves them lazily at delivery
/// time (the response goes to whoever most recently messaged in, which may
/// differ from trigger time for a long-running turn) - existing behavior,
/// unchanged by #78.
pub fn cron_delivery_fields(
    delivery: &DeliveryConfig,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
) {
    match delivery.mode {
        DeliveryMode::None => (None, None, None, None, true),
        DeliveryMode::Fixed => (
            delivery.channel.clone(),
            Some(
                delivery
                    .account_id
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            ),
            delivery.to.clone(),
            delivery.thread_id.clone(),
            false,
        ),
        DeliveryMode::Last => (None, None, delivery.to.clone(), delivery.thread_id.clone(), false),
    }
}

/// Check if current time is within active hours.
/// Format: "HH:MM-HH:MM" e.g. "08:00-24:00".
/// `tz_name` is the IANA timezone (e.g. "Asia/Shanghai").
/// Compute the next run time for a job from its schedule spec.
/// Orthogonal trigger model: None = never timer-due (webhook-only or
/// archived jobs); the HTTP server handles their other channel.

// ── SchedulerApi facade（#151 Phase 8+）──────────────────────────────────────
// cronjob 工具（L3）只经此方法面操作调度器；Scheduler 的 impl 放在
// scheduling_runtime/scheduler.rs（L4→L1 依赖方向合法），组合根继续传
// 具体 Arc<Scheduler>（泛型构造函数内部强转，daemon/builder.rs 零改动）。

pub trait SchedulerApi: Send + Sync {
    /// 新增任务，返回受理后的 job id。
    fn add_job(&self, entry: JobEntry) -> anyhow::Result<String>;
    /// 按 id 增量更新，返回是否存在。
    fn update_job(&self, id: &str, update: JobUpdate) -> anyhow::Result<bool>;
    /// 全量任务快照（列表与 not-found 提示同源）。
    fn jobs(&self) -> Vec<JobEntry>;
    /// 启停切换，返回是否存在。
    fn set_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<bool>;
    /// 删除任务，返回移除时的取证摘要（#77）。
    fn remove_job(&self, id: &str) -> anyhow::Result<Option<JobRemovalAudit>>;
    /// 读取持久化运行日志（JSONL）。
    fn read_run_log(&self, job_id: &str, limit: usize) -> Vec<RunRecord>;
}
