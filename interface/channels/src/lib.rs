//! channels — Interface Layer: Channel Adapters
//!
//! Implements the [`Channel`] trait for WeChat iLink Bot and Telegram Bot APIs.
//!
//! # Features
//!
//! - `wechat`    — WeChat iLink Bot channel (QR login, AES-ECB crypto, long-poll)
//! - `telegram` — Telegram Bot API channel (webhook/long-poll, streaming edits)

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ── Core message types ─────────────────────────────────────────────────────────

/// A message received from a channel.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    /// Unique message identifier.
    pub id: String,
    /// Platform-specific sender identifier (e.g. WeChat wxid, Telegram user id).
    pub sender: String,
    /// Platform identifier to reply to (chat id or user id).
    pub reply_target: String,
    /// Message text content.
    pub content: String,
    /// Channel name (e.g. "wechat", "telegram").
    pub channel: String,
    /// Unix timestamp (milliseconds).
    pub timestamp: u64,
    /// Thread/chat thread identifier (platform-specific).
    pub thread_ts: Option<String>,
    /// Scope for message interruption grouping.
    pub interruption_scope_id: Option<String>,
    /// Media attachments.
    pub attachments: Vec<MediaAttachment>,
}

/// A message to send through a channel.
#[derive(Debug, Clone)]
pub struct SendMessage {
    pub content: String,
    pub recipient: String,
    pub subject: Option<String>,
    pub thread_ts: Option<String>,
    pub cancellation_token: Option<CancellationToken>,
    pub attachments: Vec<MediaAttachment>,
}

impl SendMessage {
    pub fn new(content: impl Into<String>, recipient: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: None,
            thread_ts: None,
            cancellation_token: None,
            attachments: vec![],
        }
    }
}

/// A media attachment.
#[derive(Debug, Clone)]
pub struct MediaAttachment {
    pub file_name: String,
    pub data: Vec<u8>,
    pub mime_type: Option<String>,
}

// ── Channel trait ─────────────────────────────────────────────────────────────

/// Core channel trait — implemented by all messaging platform adapters.
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()>;

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()>;

    async fn health_check(&self) -> bool {
        true
    }

    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn supports_draft_updates(&self) -> bool {
        false
    }

    async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    async fn update_draft(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn finalize_draft(&self, _recipient: &str, _message_id: &str, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

// ── Feature-gated modules ──────────────────────────────────────────────────────

#[cfg(feature = "wechat")]
pub mod wechat;
#[cfg(feature = "telegram")]
pub mod telegram;
