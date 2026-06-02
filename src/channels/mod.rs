//! channels — Message channel adapters (Telegram, WeChat, QQ Bot, Client).

pub mod message;
pub mod security;
pub mod turn_stream;
pub mod telegram;
#[cfg(feature = "qqbot")]
pub mod qqbot;
#[cfg(feature = "wechat")]
pub mod wechat;
#[cfg(feature = "client")]
pub mod client;

pub use message::{
    CallbackAction, Channel, ChannelCapabilities, ChannelMessage, DedupState, InlineButton,
    LenUnit, MINIMAL_CAPABILITIES, MediaSource, MessageId, MessagePayload, ProcessingStatus,
    SendMessage, SendResult, SendTarget,
};
pub use security::{
    AllowList, AuthDecision, ChannelSecurityPolicy, GroupAuthMode, MessageScope,
    warn_if_locked_down,
};
pub use turn_stream::{StreamDelivery, TurnStream};
pub use telegram::TelegramChannel;
#[cfg(feature = "qqbot")]
pub use qqbot::QQBotChannel;
#[cfg(feature = "wechat")]
pub use wechat::WechatChannel;
#[cfg(feature = "client")]
pub use client::ClientChannel;
