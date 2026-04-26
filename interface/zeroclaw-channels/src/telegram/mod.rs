//! Telegram channel adapter (stub)

use super::Channel;

/// Telegram channel stub.
pub struct TelegramAdapter;

impl Channel for TelegramAdapter {
    fn name(&self) -> &str {
        "telegram"
    }
}