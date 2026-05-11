//! channels — Message channel adapters (Telegram, WeChat, QQ Bot, Client).

pub mod message;
pub mod telegram;
#[cfg(feature = "qqbot")]
pub mod qqbot;
#[cfg(feature = "wechat")]
pub mod wechat;
#[cfg(feature = "client")]
pub mod client;

pub use message::{Channel, ChannelMessage, SendMessage, InlineButton, DedupState, ProcessingStatus};
pub use telegram::TelegramChannel;
#[cfg(feature = "qqbot")]
pub use qqbot::QQBotChannel;
#[cfg(feature = "wechat")]
pub use wechat::WechatChannel;
#[cfg(feature = "client")]
pub use client::ClientChannel;
