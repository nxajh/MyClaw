//! channels — Message channel adapters (Telegram, WeChat).

pub mod telegram;
#[cfg(feature = "wechat")]
pub mod wechat;

pub use telegram::TelegramChannel;
#[cfg(feature = "wechat")]
pub use wechat::WechatChannel;

pub use crate::channels_message::{Channel, ChannelMessage, SendMessage};
