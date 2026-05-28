//! Agent + per-subsystem configuration sections.

use serde::{Deserialize, Serialize};

// ── PermissionMode ────────────────────────────────────────────────────────────

/// Controls what actions the agent can take without human approval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// All tools allowed, no approval needed.
    Full,
    /// Default: safe tools auto-approved, dangerous tools need approval.
    #[default]
    Default,
    /// Only read-only tools allowed.
    ReadOnly,
}

// ── RunMode ───────────────────────────────────────────────────────────────────

/// Controls execution context: is there a human user present?
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Interactive session — a user or supervisor is present.
    #[default]
    Interactive,
    /// Autonomous background task (Cron, Webhook) — no active user.
    Background,
}

// ── ContextConfig — `[context_engine]` ────────────────────────────────────────

/// Context-window management configuration. Maps the `[context_engine]`
/// TOML section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Compact threshold: trigger compaction when token usage exceeds
    /// this fraction of context_window. Default: 0.7
    #[serde(default = "default_compact_threshold")]
    pub compact_threshold: f64,

    /// Number of recent complete work units to retain during compaction.
    #[serde(default = "default_retain_work_units")]
    pub retain_work_units: usize,
}

fn default_compact_threshold() -> f64 { 0.7 }
fn default_retain_work_units() -> usize { 2 }

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compact_threshold: default_compact_threshold(),
            retain_work_units: default_retain_work_units(),
        }
    }
}

// ── ToolExecutorConfig — `[tool_executor]` ────────────────────────────────────

/// Tool-executor configuration. Maps the `[tool_executor]` TOML section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutorConfig {
    /// Tool call timeout in seconds.
    #[serde(default = "default_tool_timeout")]
    pub timeout_secs: u64,
}

fn default_tool_timeout() -> u64 { 180 }

impl Default for ToolExecutorConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_tool_timeout(),
        }
    }
}

// ── AgentConfig — `[agent]` ───────────────────────────────────────────────────

/// Agent-wide configuration. Per the RFC v2 target shape, this section
/// only carries the permission mode (subsystem configs moved to their
/// own top-level sections).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Permission mode — controls tool approval requirements.
    #[serde(default)]
    pub permission_mode: PermissionMode,
}

// ── PromptConfig — `[prompt]` ─────────────────────────────────────────────────

/// System prompt builder configuration. Maps the `[prompt]` TOML section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptConfig {
    /// Maximum system prompt length in characters. 0 = unlimited.
    #[serde(default)]
    pub max_chars: usize,

    /// Use native tool calling (vs XML protocol).
    #[serde(default = "default_true")]
    pub native_tools: bool,

    /// IANA timezone name (e.g. "Asia/Shanghai").
    /// Takes precedence over `timezone_offset` when set.
    #[serde(default)]
    pub timezone: Option<String>,

    /// Timezone offset in hours (e.g. 8 for UTC+8).
    /// Legacy fallback — prefer `timezone` for DST-aware scheduling.
    #[serde(default = "default_timezone_offset")]
    pub timezone_offset: i32,
}

fn default_timezone_offset() -> i32 { 8 }

fn default_true() -> bool { true }

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            max_chars: 0,
            native_tools: true,
            timezone: None,
            timezone_offset: default_timezone_offset(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_config() {
        let config = AgentConfig::default();
        assert_eq!(config.permission_mode, PermissionMode::Default);
    }

    #[test]
    fn default_subsystem_configs() {
        let tex = ToolExecutorConfig::default();
        assert_eq!(tex.timeout_secs, 180);
        let ctx = ContextConfig::default();
        assert!((ctx.compact_threshold - 0.7).abs() < 1e-9);
        assert_eq!(ctx.retain_work_units, 2);
    }
}
