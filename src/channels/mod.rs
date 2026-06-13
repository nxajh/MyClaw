//! channels — Message channel adapters (Telegram, WeChat, QQ Bot, Client).

#[cfg(feature = "client")]
pub mod client;
pub mod message;
#[cfg(feature = "qqbot")]
pub mod qqbot;
pub mod security;
pub mod telegram;
pub mod turn_stream;
#[cfg(feature = "wechat")]
pub mod wechat;

#[cfg(feature = "client")]
pub use client::ClientChannel;
pub use message::{
    CallbackAction, Channel, ChannelCapabilities, ChannelFile, ChannelFileBody, ChannelFileMeta,
    ChannelInboundMessage, ChannelMessageContent, ChannelOutboundMessage, DedupState, InlineButton,
    LenUnit, LocalFileBody, MINIMAL_CAPABILITIES, MessageId, MessageReceiver, MessageSender,
    OutboundSendResult, PersistedChannelMessage, ProcessingStatus, SendOptions,
};
#[cfg(feature = "qqbot")]
pub use qqbot::QQBotChannel;
pub use security::{
    AllowList, AuthDecision, ChannelSecurityPolicy, GroupAuthMode, MessageScope,
    warn_if_locked_down,
};
pub use telegram::TelegramChannel;
pub use turn_stream::{StreamDelivery, TurnStream};
#[cfg(feature = "wechat")]
pub use wechat::WechatChannel;
