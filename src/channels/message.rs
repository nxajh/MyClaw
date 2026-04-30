//! channels_message — Shared channel message types.

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ── Core message types ─────────────────────────────────────────────────────────

/// A message received from a channel.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub channel: String,
    pub timestamp: u64,
    pub thread_ts: Option<String>,
    pub interruption_scope_id: Option<String>,
    pub attachments: Vec<MediaAttachment>,
    /// URLs of images attached to this message (e.g. from Telegram photo messages).
    pub image_urls: Option<Vec<String>>,
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
    pub image_urls: Option<Vec<String>>,
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
            image_urls: None,
        }
    }
    pub fn is_verbose(&self, chunk_limit: usize) -> bool {
        self.content.chars().count() > chunk_limit
    }
}

/// A media attachment.
#[derive(Debug, Clone)]
pub struct MediaAttachment {
    pub file_name: String,
    pub data: Vec<u8>,
    pub mime_type: Option<String>,
}

/// Marker trait for channel adapters.
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>;
    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>;
    async fn health_check(&self) -> bool;
    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()>;
    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()>;
}

/// Dedup state for a channel adapter (in-memory).
#[derive(Clone)]
pub struct DedupState {
    seen: Arc<Mutex<std::collections::HashSet<String>>>,
    #[allow(dead_code)]
    window_secs: u64,
}

impl Default for DedupState {
    fn default() -> Self {
        Self {
            seen: Arc::new(Mutex::new(std::collections::HashSet::new())),
            window_secs: 300,
        }
    }
}

impl DedupState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if an update ID has been seen, and record it if not.
    /// Returns true if the ID was already seen (should skip), false if new.
    pub fn check_and_record(&self, id: &str) -> bool {
        let mut seen = self.seen.lock().unwrap();
        !seen.insert(id.to_string())
    }
}

/// Split a message into chunks of at most `limit` chars.
pub fn split_message_chunk(message: &str, limit: usize) -> Vec<String> {
    if message.chars().count() <= limit {
        return vec![message.to_string()];
    }
    message
        .chars()
        .collect::<Vec<char>>()
        .chunks(limit)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}