//! Agent configuration — permission mode, loop breaker, prompt settings.

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

// ── ContextConfig ─────────────────────────────────────────────────────────────

/// Context window management configuration.
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

// ── AgentConfig ───────────────────────────────────────────────────────────────

/// Agent behavior configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Hard cap on tool calls per turn. 0 = unlimited.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: usize,

    /// Maximum conversation history messages to keep. 0 = unlimited.
    #[serde(default = "default_max_history")]
    pub max_history: usize,

    /// Permission mode — controls tool approval requirements.
    #[serde(default)]
    pub permission_mode: PermissionMode,

    /// Tool call timeout in seconds.
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_secs: u64,

    /// Timeout in seconds waiting for the first streaming chunk from the API.
    /// Covers slow startup on large contexts; after the first chunk arrives,
    /// TCP keepalive handles dead-connection detection at the OS level.
    #[serde(default = "default_stream_first_chunk_timeout")]
    pub stream_first_chunk_timeout_secs: u64,

    /// Loop breaker: max consecutive identical tool calls before breaking.
    #[serde(default = "default_loop_breaker_threshold")]
    pub loop_breaker_threshold: u32,

    /// System prompt configuration.
    #[serde(default)]
    pub prompt: PromptConfig,

    /// Context window management settings.
    #[serde(default)]
    pub context: ContextConfig,
    /// Scheduler settings (heartbeat, cron, webhook).
    #[serde(default)]
    pub scheduler: crate::config::scheduler::SchedulerConfig,
}

fn default_max_tool_calls() -> usize { 100 }
fn default_max_history() -> usize { 200 }
fn default_tool_timeout() -> u64 { 180 }
fn default_stream_first_chunk_timeout() -> u64 { 600 }
fn default_loop_breaker_threshold() -> u32 { 3 }

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: default_max_tool_calls(),
            max_history: default_max_history(),
            permission_mode: PermissionMode::Default,
            tool_timeout_secs: default_tool_timeout(),
            stream_first_chunk_timeout_secs: default_stream_first_chunk_timeout(),
            loop_breaker_threshold: default_loop_breaker_threshold(),
            prompt: PromptConfig::default(),
            context: ContextConfig::default(),
            scheduler: crate::config::scheduler::SchedulerConfig::default(),
        }
    }
}

// ── PromptConfig ──────────────────────────────────────────────────────────────

/// System prompt builder configuration.
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
        assert_eq!(config.max_tool_calls, 100);
        assert_eq!(config.max_history, 200);
        assert_eq!(config.permission_mode, PermissionMode::Default);
        assert_eq!(config.tool_timeout_secs, 180);
        assert!(config.prompt.native_tools);
    }

    #[test]
    fn deserialize_agent_config() {
        let toml_str = r#"
max_tool_calls = 50
permission_mode = "full"
tool_timeout_secs = 300

[prompt]
max_chars = 10000
"#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_tool_calls, 50);
        assert_eq!(config.permission_mode, PermissionMode::Full);
        assert_eq!(config.prompt.max_chars, 10000);
    }
}
