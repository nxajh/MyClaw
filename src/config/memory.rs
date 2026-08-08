//! Memory configuration (`[memory]` — idle-time distillation settings).

use serde::{Deserialize, Serialize};

fn default_distill_enabled() -> bool {
    true
}
fn default_distill_idle_secs() -> u64 {
    1800
}
fn default_distill_interval_secs() -> u64 {
    900
}

/// Memory system configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Enable idle-time agent-layer memory distillation.
    #[serde(default = "default_distill_enabled")]
    pub distill_enabled: bool,
    /// Idle threshold in seconds: no inbound messages for this long before a
    /// distillation pass may run.
    #[serde(default = "default_distill_idle_secs")]
    pub distill_idle_secs: u64,
    /// How often the scheduler checks for pending distillation (seconds).
    #[serde(default = "default_distill_interval_secs")]
    pub distill_interval_secs: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            distill_enabled: default_distill_enabled(),
            distill_idle_secs: default_distill_idle_secs(),
            distill_interval_secs: default_distill_interval_secs(),
        }
    }
}
