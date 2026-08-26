//! channels — Message channel adapters (Telegram, WeChat, QQ Bot, Client).

pub mod message;
#[cfg(feature = "qqbot")]
pub mod qqbot;
pub mod security;
pub mod telegram;
#[cfg(feature = "wechat")]
pub mod wechat;

pub use message::{
    CallbackAction, Channel, ChannelCapabilities, ChannelFile, ChannelFileBody, ChannelFileMeta,
    ChannelInboundMessage, ChannelMessageContent, ChannelOutboundMessage, DedupState, GroupStat,
    InlineButton, LenUnit, LocalFileBody, MINIMAL_CAPABILITIES, MessageId, MessageReceiver,
    MessageSender, OutboundSendResult, PersistedChannelMessage, ProcessingStatus, SendOptions,
    ToolEvent,
};
#[cfg(feature = "qqbot")]
pub use qqbot::QQBotChannel;
pub use security::{
    AllowList, AuthDecision, ChannelSecurityPolicy, GroupAuthMode, MessageScope,
    warn_if_locked_down,
};
pub use telegram::TelegramChannel;
pub use crate::api::turn_stream::{FoldCandidate, StreamDelivery, TurnStream};
#[cfg(feature = "wechat")]
pub use wechat::WechatChannel;
