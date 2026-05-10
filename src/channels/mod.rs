//! channels — Message channel adapters (Telegram, WeChat, QQ Bot).

pub mod message;
pub mod telegram;
#[cfg(feature = "qqbot")]
pub mod qqbot;
#[cfg(feature = "wechat")]
pub mod wechat;

pub use message::{Channel, ChannelMessage, SendMessage, DedupState, ProcessingStatus};
pub use telegram::TelegramChannel;
#[cfg(feature = "qqbot")]
pub use qqbot::QQBotChannel;
#[cfg(feature = "wechat")]
pub use wechat::WechatChannel;
