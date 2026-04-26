//! Slack channel adapter (stub)

use super::Channel;

/// Slack channel stub.
pub struct SlackAdapter;

impl Channel for SlackAdapter {
    fn name(&self) -> &str {
        "slack"
    }
}