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
    /// Target user/group ID (channel-specific format).
    #[serde(default)]
    pub to: Option<String>,
    /// Thread/topic ID for threaded channels (Discord, Telegram topics).
    #[serde(default)]
    pub thread_id: Option<String>,
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
