//! channels — Message channel adapters (Telegram, WeChat).

pub mod message;
pub mod telegram;
#[cfg(feature = "wechat")]
pub mod wechat;

pub use message::{Channel, ChannelMessage, SendMessage, DedupState};
pub use telegram::TelegramChannel;
#[cfg(feature = "wechat")]
pub use wechat::WechatChannel;
