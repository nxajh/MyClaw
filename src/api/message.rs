//! api_message — Shared channel message types (L0 contract layer).
//!
//! These types are the shared contracts between channels, agents, and tools.
//! They are defined here to break the `agents→channels` and `tools→agents`
//! dependency violations.

use serde::{Deserialize, Serialize};

/// Identity of the party that sent a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSender {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl MessageSender {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
        }
    }
}

/// Where to deliver a channel message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReceiver {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
}

impl MessageReceiver {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            thread_id: None,
            reply_to_message_id: None,
        }
    }

    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    pub fn with_reply_to(mut self, message_id: impl Into<String>) -> Self {
        self.reply_to_message_id = Some(message_id.into());
        self
    }
}

/// Serializable inbound context kept on sessions. File bodies are runtime-only
/// and must not be persisted here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedChannelMessage {
    pub id: String,
    pub sender_id: String,
    pub receiver: MessageReceiver,
    pub text: String,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
}

use tokio_util::sync::CancellationToken;
use async_trait::async_trait;
use std::pin::Pin;
use tokio::io::AsyncRead;
use std::path::PathBuf;
use std::sync::Arc;

/// Optional parameters for outbound sends via `Channel::send_message`.
///
/// Reserved extension point — `cancellation_token` is declared but not yet
/// wired through any adapter send path. Kept so future send options
/// (cancellation, priority, TTL) can be added without an API break.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    pub cancellation_token: Option<CancellationToken>,
}

// ── File types ──────────────────────────────────────────────────────────────

/// Metadata for a file attached to a channel message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelFileMeta {
    pub file_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Original public URL if this file was fetched from (or is available at)
    /// a URL. Channels that support URL-based upload (e.g. QQ Bot) can pass
    /// this directly to the platform API instead of downloading + base64.
    /// Not persisted — runtime-only.
    #[serde(skip)]
    pub source_url: Option<String>,
}

/// A repeat-openable file body provider passed across the channel/session boundary.
#[async_trait]
pub trait ChannelFileBody: Send + Sync {
    async fn open(&self) -> anyhow::Result<Pin<Box<dyn AsyncRead + Send>>>;

    /// Best-effort path for providers that read files directly from disk.
    fn path_hint(&self) -> Option<&str> {
        None
    }
}

/// A local file body. The path is intentionally private: channel adapters consume
/// the body through `open()` and do not depend on session filesystem layout.
#[derive(Debug, Clone)]
pub struct LocalFileBody {
    path: PathBuf,
}

impl LocalFileBody {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl ChannelFileBody for LocalFileBody {
    async fn open(&self) -> anyhow::Result<Pin<Box<dyn AsyncRead + Send>>> {
        let file = tokio::fs::File::open(&self.path).await?;
        Ok(Box::pin(file))
    }

    fn path_hint(&self) -> Option<&str> {
        self.path.to_str()
    }
}

/// A complete file attachment for the channel message model: metadata + body.
#[derive(Clone)]
pub struct ChannelFile {
    pub meta: ChannelFileMeta,
    pub body: Arc<dyn ChannelFileBody>,
}

impl std::fmt::Debug for ChannelFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelFile")
            .field("meta", &self.meta)
            .field("body", &"<ChannelFileBody>")
            .finish()
    }
}

// ── ChannelMessageContent ───────────────────────────────────────────────────

/// The content of an outbound or inbound channel message.
#[derive(Debug, Clone)]
pub struct ChannelMessageContent {
    pub text: String,
    pub files: Vec<ChannelFile>,
    pub buttons: Vec<InlineButton>,
}

impl ChannelMessageContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            files: vec![],
            buttons: vec![],
        }
    }
}

/// A message to be sent out through a channel.
#[derive(Debug, Clone)]
pub struct ChannelOutboundMessage {
    pub receiver: MessageReceiver,
    pub content: ChannelMessageContent,
    pub options: SendOptions,
}

impl ChannelOutboundMessage {
    pub fn text(receiver: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            receiver: MessageReceiver::new(receiver),
            content: ChannelMessageContent::text(text),
            options: SendOptions::default(),
        }
    }
}

/// Result of sending one logical outbound message. Multi-file messages may map
/// to several platform messages, so ids are returned in send order.
#[derive(Debug, Clone, Default)]
pub struct OutboundSendResult {
    pub message_ids: Vec<MessageId>,
}

impl OutboundSendResult {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn single(id: MessageId) -> Self {
        Self {
            message_ids: vec![id],
        }
    }
}

/// Platform-specific message id (e.g. Telegram message_id, QQBot msg_id).
/// Returned by `send_message` to identify a previously sent message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId(pub String);

impl MessageId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ── Core message types ─────────────────────────────────────────────────────────

/// An inline button for interactive messages.
/// Used by channels that support inline keyboards (e.g. Telegram).
/// Channels that don't support buttons ignore the `inline_buttons` field.
#[derive(Debug, Clone)]
pub struct InlineButton {
    /// Button label displayed to the user.
    pub label: String,
    /// Callback data sent back when the button is clicked.
    /// For Telegram: max 64 bytes.
    pub callback_data: String,
}

#[async_trait]
pub trait OutboundChannel: Send + Sync {
    async fn send_outbound_message(
        &self,
        msg: &ChannelOutboundMessage,
    ) -> anyhow::Result<OutboundSendResult>;
    fn supports_file_send(&self) -> bool;
}

