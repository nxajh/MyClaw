//! channels — Interface Layer: Channel Adapters
//!
//! Implements the [`Channel`] trait for WeChat iLink Bot and Telegram Bot APIs.
//!
//! # Features
//!
//! - `wechat`    — WeChat iLink Bot channel (QR login, AES-ECB crypto, long-poll)
//! - `telegram` — Telegram Bot API channel (webhook/long-poll, streaming edits)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
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
    /// Image URLs to include as attachments (optional).
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

    /// Returns true if the message content exceeds the per-character chunk
    /// limit. Used by channels that send in chunks.
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

// ── Channel trait ─────────────────────────────────────────────────────────────

/// Core channel trait — implemented by all messaging platform adapters.
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()>;

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>;

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

// ── Shared utilities ────────────────────────────────────────────────────────────

/// Split a long message into chunks of at most `limit` characters each.
/// Tries to break at whitespace when possible.
pub fn split_message_chunk(message: &str, limit: usize) -> Vec<String> {
    if message.chars().count() <= limit {
        return vec![message.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = message;

    while !remaining.is_empty() {
        if remaining.chars().count() <= limit {
            chunks.push(remaining.to_string());
            break;
        }

        // Find the byte offset for the Nth character boundary.
        let hard_split = remaining
            .char_indices()
            .nth(limit)
            .map_or(remaining.len(), |(idx, _)| idx);

        // Try to split at a natural break point near hard_split.
        let split_at = {
            let search_area = &remaining[..hard_split];
            if let Some(newline_pos) = search_area.rfind('\n') {
                if search_area[..newline_pos].chars().count() >= limit / 2 {
                    Some(newline_pos + 1)
                } else {
                    search_area.rfind(' ').map(|p| p + 1)
                }
            } else {
                search_area.rfind(' ').map(|p| p + 1)
            }
        };

        let end = split_at.unwrap_or(hard_split);
        chunks.push(remaining[..end].to_string());
        remaining = &remaining[end..];
    }

    chunks
}

/// A rate limiter backed by a token bucket — accumulates tokens at
/// ` refill_per_sec` tokens/second up to `max_tokens`.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    tokens: f64,
    max_tokens: f64,
    refill_per_sec: f64,
    last_refill: std::time::Instant,
}

impl RateLimiter {
    pub fn new(max_tokens: u64, refill_per_sec: f64) -> Self {
        Self {
            tokens: max_tokens as f64,
            max_tokens: max_tokens as f64,
            refill_per_sec,
            last_refill: std::time::Instant::now(),
        }
    }

    /// Try to acquire `cost` tokens. Returns the duration to wait if none available.
    pub fn try_acquire(&mut self, cost: u64) -> Option<Duration> {
        self.refill();
        if (self.tokens as u64) < cost {
            let needed = cost.saturating_sub(self.tokens as u64);
            Some(Duration::from_secs_f64(needed as f64 / self.refill_per_sec))
        } else {
            self.tokens -= cost as f64;
            None
        }
    }

    /// Check if `cost` tokens are available (non-blocking).
    pub fn available(&mut self, cost: u64) -> bool {
        self.refill();
        (self.tokens as u64) >= cost
    }

    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.max_tokens);
        self.last_refill = std::time::Instant::now();
    }
}

// ── Dedup state ───────────────────────────────────────────────────────────────

/// Tracks recently-seen message IDs to suppress duplicates from reconnects.
/// Uses parking_lot::Mutex for interior mutability — safe in async contexts.
/// Wrapped in Arc so it can be cheaply cloned into spawned listener tasks.
#[derive(Debug, Clone)]
pub struct DedupState {
    seen: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
}

impl DedupState {
    pub fn new() -> Self {
        Self {
            seen: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Check if a message ID is already seen; record it if not.
    /// Returns true if this is a NEW message (not a duplicate).
    pub fn check_and_record(&self, id: &str) -> bool {
        self.seen.lock().insert(id.to_string())
    }
}

impl Default for DedupState {
    fn default() -> Self {
        Self::new()
    }
}