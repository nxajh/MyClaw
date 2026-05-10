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
    /// Base64-encoded image data (used when the source URL is not directly
    /// accessible by the LLM provider, e.g. Telegram file API).
    pub image_base64: Option<Vec<String>>,
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

/// Processing status notification from Orchestrator to Channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingStatus {
    /// LLM call started — the bot is "thinking".
    Thinking,
    /// Response sent successfully (status cleanup already handled in send()).
    Done,
    /// An error occurred during processing.
    Error,
}

/// Marker trait for channel adapters.
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>;
    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>;
    async fn health_check(&self) -> bool;

    /// Notify the channel about processing status changes.
    /// Default implementation does nothing — channels can override to show
    /// status indicators (e.g. reactions).
    async fn on_status(&self, _recipient: &str, _status: ProcessingStatus) {}
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

/// Split a message into chunks of at most `limit` chars, respecting markdown structure.
///
/// Splitting priority:
/// 1. Double newline (paragraph boundary)
/// 2. Single newline (line boundary)
/// 3. Space (word boundary)
/// 4. Hard cut at limit (last resort)
///
/// Preserves code blocks and tables by treating them as atomic units when possible.
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

        // Try to find a good split point within the limit
        let split_pos = find_split_point(remaining, limit);
        // find_split_point returns a char index; convert to byte index for split_at
        let byte_pos = remaining
            .char_indices()
            .nth(split_pos)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let (chunk, rest) = remaining.split_at(byte_pos);
        chunks.push(chunk.trim_end().to_string());
        remaining = rest.trim_start();
    }

    chunks
}

/// Find the best position to split text, preferring natural boundaries.
fn find_split_point(text: &str, limit: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let limit = limit.min(len);

    // Track if we're inside a code block
    let mut in_code_block = false;

    // First pass: find code block boundaries to avoid splitting inside them
    let mut i = 0;
    while i < limit {
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            in_code_block = !in_code_block;
            i += 3;
            continue;
        }
        i += 1;
    }

    // If we're inside a code block at the limit, try to extend to the closing fence
    if in_code_block {
        // Look for closing fence after limit
        let mut j = limit;
        while j + 2 < len {
            if chars[j] == '`' && chars[j + 1] == '`' && chars[j + 2] == '`' {
                // Found closing fence, split after it
                return (j + 3).min(len);
            }
            j += 1;
        }
    }

    // Try splitting at double newline (paragraph boundary) - highest priority
    if let Some(pos) = find_last_pattern(&chars[..limit], &['\n', '\n']) {
        return pos + 2; // Include both newlines in the first chunk
    }

    // Try splitting at single newline
    if let Some(pos) = find_last_char(&chars[..limit], '\n') {
        return pos + 1; // Include newline in the first chunk
    }

    // Try splitting at space (word boundary)
    if let Some(pos) = find_last_char(&chars[..limit], ' ') {
        return pos + 1;
    }

    // Last resort: hard cut at limit
    limit
}

/// Find the last occurrence of a character pattern in the slice.
fn find_last_pattern(chars: &[char], pattern: &[char]) -> Option<usize> {
    if pattern.is_empty() || chars.len() < pattern.len() {
        return None;
    }

    (0..=chars.len() - pattern.len())
        .rev()
        .find(|&i| &chars[i..i + pattern.len()] == pattern)
}

/// Find the last occurrence of a character in the slice.
fn find_last_char(chars: &[char], target: char) -> Option<usize> {
    (0..chars.len()).rev().find(|&i| chars[i] == target)
}