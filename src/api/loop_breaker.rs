//! Loop-breaker configuration — L0 contract layer.
//!
//! `LoopBreakerConfig` is the `[loop_breaker]` TOML section shared between
//! `config` (L1), `cli`, `daemon` and `agents` (L4). It lives in `api` so the
//! config layer does not depend on agents. The runtime state machine
//! (`LoopBreaker` / `LoopBreakerCounter`) stays in `agents::loop_breaker`.

use serde::{Deserialize, Serialize};

/// Configuration for loop breaking. Also serves as the `[loop_breaker]`
/// TOML section — all fields have defaults so the section is optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopBreakerConfig {
    /// Hard cap on total tool calls. 0 = unlimited (but still checks patterns).
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: usize,
    /// Sliding window size for pattern detection.
    #[serde(default = "default_window_size")]
    pub window_size: usize,
    /// Exact repeat threshold: same tool + same args N times → break.
    #[serde(default = "default_exact_repeat_threshold")]
    pub exact_repeat_threshold: usize,

    /// Ping-pong threshold: alternating rounds before breaking.
    #[serde(default = "default_ping_pong_rounds")]
    pub ping_pong_rounds: usize,
    /// No-progress threshold: same tool + same result hash N consecutive times → break.
    #[serde(default = "default_no_progress_threshold")]
    pub no_progress_threshold: usize,
    /// Tools that are inherently exploratory (e.g. "shell") and need a higher threshold
    /// before NoProgress is triggered. These tools naturally produce similar results
    /// (empty grep, exit code 0) across different args without actually looping.
    #[serde(default = "default_relaxed_tools")]
    pub relaxed_tools: Vec<String>,
    /// Rapid-repeat window (seconds). ExactRepeat only triggers when the
    /// threshold is reached *within* this time span. Calls spread over a
    /// longer period (e.g. polling with `sleep 120` between checks) are
    /// legitimate and will not trigger.
    #[serde(default = "default_rapid_repeat_window")]
    pub rapid_repeat_window_secs: u64,
}

fn default_max_tool_calls() -> usize {
    100
}
fn default_window_size() -> usize {
    20
}
fn default_exact_repeat_threshold() -> usize {
    3
}
fn default_ping_pong_rounds() -> usize {
    6
}
fn default_no_progress_threshold() -> usize {
    5
}
fn default_relaxed_tools() -> Vec<String> {
    vec![
        "shell".to_string(),
        "task_update".to_string(),
        "task_delete".to_string(),
    ]
}
fn default_rapid_repeat_window() -> u64 {
    60
}

impl Default for LoopBreakerConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: default_max_tool_calls(),
            window_size: default_window_size(),
            exact_repeat_threshold: default_exact_repeat_threshold(),
            ping_pong_rounds: default_ping_pong_rounds(),
            no_progress_threshold: default_no_progress_threshold(),
            relaxed_tools: default_relaxed_tools(),
            rapid_repeat_window_secs: default_rapid_repeat_window(),
        }
    }
}
