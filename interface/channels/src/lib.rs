//! Channel Adapters — WeChat, Telegram, Discord, Slack
//!
//! Features (compile-time optional):
//! - `wechat`   - WeChat channel adapter
//! - `telegram` - Telegram channel adapter
//! - `discord`  - Discord channel adapter
//! - `slack`    - Slack channel adapter

/// Channel trait — implemented by all channel adapters.
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
}

#[cfg(feature = "wechat")]
pub mod wechat;
#[cfg(feature = "telegram")]
pub mod telegram;
#[cfg(feature = "discord")]
pub mod discord;
#[cfg(feature = "slack")]
pub mod slack;