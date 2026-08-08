//! Scheduler configuration — Heartbeat, Cron, Webhook.
//!
//! Job definitions come from files (`cron/*.md`, `webhooks/*.md`),
//! not from TOML config. This module only holds global settings.

use serde::{Deserialize, Serialize};

fn default_every() -> String {
    "30m".to_string()
}
fn default_target() -> String {
    "last".to_string()
}
fn default_webhook_port() -> u16 {
    18789
}

/// Heartbeat configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    /// Enable periodic heartbeat checks.
    #[serde(default)]
    pub enabled: bool,
    /// Interval string: "5m", "30m", "1h". "0" disables.
    #[serde(default = "default_every")]
    pub every: String,
    /// Where to send heartbeat output: "last" | "none" | channel name.
    #[serde(default = "default_target")]
    pub target: String,
    /// Active hours, e.g. "08:00-24:00". None = always active.
    #[serde(default)]
    pub active_hours: Option<String>,
    /// Custom heartbeat prompt. None = default prompt.
    #[serde(default)]
    pub prompt: Option<String>,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            every: default_every(),
            target: default_target(),
            active_hours: None,
            prompt: None,
        }
    }
}

/// Context policy for scheduled turns — determines whether the turn
/// is injected into the user's active session or run in isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextPolicy {
    /// Inject into the user's active session via routing key.
    /// The turn's result appears in the user's conversation history.
    Inject,
    /// Run in an isolated session. The result is sent to the target
    /// channel but NOT written to the user's session history.
    Isolated,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self::Isolated
    }
}

/// A single cron job (loaded from `cron/*.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Cron expression (5-field: min hour day month weekday).
    /// e.g. "0 9 * * *" = every day at 9:00.
    pub schedule: String,
    /// Prompt to send to the agent when triggered.
    pub prompt: String,
    /// Where to send output: "last" | "none" | channel name.
    #[serde(default = "default_target")]
    pub target: String,
    /// Active hours restriction, e.g. "08:00-24:00". None = always active.
    #[serde(default)]
    pub active_hours: Option<String>,
    /// Context policy: inject into user session or run isolated.
    /// Defaults to Inject for cron jobs.
    #[serde(default = "default_cron_context_policy")]
    pub context_policy: ContextPolicy,
}

fn default_cron_context_policy() -> ContextPolicy {
    ContextPolicy::Inject
}

/// Cron scheduler configuration (global settings only).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronConfig {
    /// Enable cron scheduler.
    #[serde(default)]
    pub enabled: bool,
}

/// Webhook server configuration (global settings only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Enable webhook HTTP server.
    #[serde(default)]
    pub enabled: bool,
    /// Port to listen on.
    #[serde(default = "default_webhook_port")]
    pub port: u16,
    /// Default secret for built-in endpoints (`/hooks/agent`, `/hooks/wake`).
    /// Individual webhook files can override with their own secret.
    #[serde(default)]
    pub secret: Option<String>,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_webhook_port(),
            secret: None,
        }
    }
}

/// Unified scheduler configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
}
