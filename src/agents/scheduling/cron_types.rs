//! Cron types — data models for the cron system.
//!
//! Separated from scheduler.rs for clarity. All types are serializable
//! and stored in jobs.json.

use serde::{Deserialize, Serialize};

// ── Delivery ────────────────────────────────────────────────────────────────

/// Per-job delivery configuration.
/// When set, overrides the `target` field with more granular control.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryConfig {
    /// Target channel name (e.g. "telegram", "discord").
    pub channel: String,
    /// Target account ID (for multi-instance channels).
    #[serde(default)]
    pub account_id: Option<String>,
    /// Target user/group ID (channel-specific format).
    #[serde(default)]
    pub to: Option<String>,
    /// Thread/topic ID for threaded channels (Discord, Telegram topics).
    #[serde(default)]
    pub thread_id: Option<String>,
}

// ── Retry ──────────────────────────────────────────────────────────────────

/// Error categories that can trigger retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryableError {
    RateLimit,
    Timeout,
    ServerError,
    Network,
    Overloaded,
}

/// Per-job retry policy for transient errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Max retries before marking as permanently failed (default: 3).
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Backoff delays in milliseconds for each retry (default: [30000, 60000, 300000]).
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: Vec<u64>,
}

fn default_max_attempts() -> u32 {
    3
}
fn default_backoff_ms() -> Vec<u64> {
    vec![30_000, 60_000, 300_000]
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff_ms: default_backoff_ms(),
        }
    }
}

// ── Failure Alert ──────────────────────────────────────────────────────────

/// Configuration for alerting on consecutive failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureAlertConfig {
    /// Alert after N consecutive failures (default: 3).
    #[serde(default = "default_after")]
    pub after: u32,
    /// Minimum seconds between repeated alerts (default: 3600).
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Whether to include skipped runs in the failure count (default: false).
    #[serde(default)]
    pub include_skipped: bool,
}

fn default_after() -> u32 {
    3
}
fn default_cooldown_secs() -> u64 {
    3600
}

impl Default for FailureAlertConfig {
    fn default() -> Self {
        Self {
            after: default_after(),
            cooldown_secs: default_cooldown_secs(),
            include_skipped: false,
        }
    }
}

// ── Run Record ──────────────────────────────────────────────────────────────

/// Status of a single cron job execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Ok,
    Error,
    Timeout,
    Skipped,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Skipped => "skipped",
        }
    }
}

/// Record of a single cron job execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunRecord {
    /// ISO 8601 timestamp of execution.
    pub run_at: String,
    /// Execution status.
    pub status: RunStatus,
    /// Trigger source: "cron" (timer) or "webhook" (HTTP POST). Absent on
    /// legacy records — treated as cron.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// Execution duration in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
    /// First 200 chars of output (for quick preview).
    #[serde(default)]
    pub output_preview: String,
    /// Error message if status != Ok.
    #[serde(default)]
    pub error: Option<String>,
    /// Input tokens consumed.
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens produced.
    #[serde(default)]
    pub output_tokens: u64,
    /// Webhook audit: received payload, truncated to 8KB (webhook runs only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// Webhook audit: first 512 chars of the rendered prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_head: Option<String>,
}

impl RunRecord {
    /// Create a RunRecord with the current timestamp.
    pub fn now(status: RunStatus) -> Self {
        Self {
            run_at: chrono::Utc::now().to_rfc3339(),
            status,
            ..Default::default()
        }
    }

    pub fn with_duration(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }

    pub fn with_error(mut self, err: String) -> Self {
        self.error = Some(err);
        self
    }

    pub fn with_output_preview(mut self, output: &str) -> Self {
        self.output_preview = output.chars().take(200).collect();
        self
    }

    /// Tag the run's trigger source: "cron" or "webhook".
    pub fn with_trigger(mut self, trigger: &str) -> Self {
        self.trigger = Some(trigger.to_string());
        self
    }
}

// ── Schedule Kind ───────────────────────────────────────────────────────────

/// Scheduling type for a cron job.
/// When present, overrides the `schedule` string field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleKind {
    /// Standard cron expression.
    Cron { expr: String },
    /// Fixed interval (e.g. every 30 minutes).
    Every { interval_ms: u64 },
    /// One-shot: run once at a specific time, then auto-disable.
    At { at: String },
}
