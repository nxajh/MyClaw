//! Discord channel adapter (stub)

use super::Channel;

/// Discord channel stub.
pub struct DiscordAdapter;

impl Channel for DiscordAdapter {
    fn name(&self) -> &str {
        "discord"
    }
}