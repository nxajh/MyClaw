//! RunMode — interactive vs background turn mode (L0 contract).

use serde::{Deserialize, Serialize};

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
