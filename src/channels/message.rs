//! channels_message — Shared channel message types.

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agents::TurnEvent;

// ── Channel capabilities (RFC §6.1) ────────────────────────────────────────────

/// Unit used by the platform to measure message length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LenUnit {
    /// Unicode code points (Rust `chars().count()`).
    Codepoints,
    /// UTF-16 code units (Telegram's measure — emoji counts as 2).
    Utf16Units,
    /// Raw UTF-8 bytes.
    Bytes,
}

/// Declarative capabilities of a Channel implementation.
///
/// Per RFC §6.1: const-fn constructors let each channel publish a
/// `&'static ChannelCapabilities`. `MINIMAL_CAPABILITIES` is the default
/// for implementations that don't override `Channel::capabilities()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelCapabilities {
    pub supports_streaming: bool,
    pub supports_edit: bool,
    pub supports_delete: bool,
    pub supports_inline_buttons: bool,
    pub supports_media: bool,
    pub supports_threads: bool,
    /// Maximum length of a single message; messages longer than this must be split.
    pub message_chunk_limit: usize,
    pub message_len_unit: LenUnit,
}

impl ChannelCapabilities {
    /// Conservative defaults — no streaming/edit/delete/media/buttons; treat
    /// length as codepoints with a generous 64 KiB cap so non-overriding
    /// channels don't trip the splitter unnecessarily.
    pub const fn minimal() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: false,
            supports_delete: false,
            supports_inline_buttons: false,
            supports_media: false,
            supports_threads: false,
            message_chunk_limit: 65_536,
            message_len_unit: LenUnit::Codepoints,
        }
    }

    /// ClientChannel (WebSocket WebUI/TUI): full streaming, plenty of slack.
    pub const fn client() -> Self {
        Self {
            supports_streaming: true,
            supports_edit: false,
            supports_delete: false,
            supports_inline_buttons: true,
            supports_media: true,
            supports_threads: false,
            message_chunk_limit: 65_536,
            message_len_unit: LenUnit::Codepoints,
        }
    }

    /// Telegram bot: 4096 UTF-16 code units, edit/delete/buttons/media.
    pub const fn telegram() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: true,
            supports_delete: true,
            supports_inline_buttons: true,
            supports_media: true,
            supports_threads: true,
            message_chunk_limit: 4096,
            message_len_unit: LenUnit::Utf16Units,
        }
    }

    /// QQBot: 2000 codepoints, inline buttons via Keyboard.
    pub const fn qqbot() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: false,
            supports_delete: false,
            supports_inline_buttons: true,
            supports_media: false,
            supports_threads: false,
            message_chunk_limit: 2000,
            message_len_unit: LenUnit::Codepoints,
        }
    }

    /// Wechat: 2048 codepoints, minimal feature set.
    pub const fn wechat() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: false,
            supports_delete: false,
            supports_inline_buttons: false,
            supports_media: false,
            supports_threads: false,
            message_chunk_limit: 2048,
            message_len_unit: LenUnit::Codepoints,
        }
    }
}

/// Static default returned by `Channel::capabilities()` when an
/// implementation doesn't override it. Zero-cost reference.
pub static MINIMAL_CAPABILITIES: ChannelCapabilities = ChannelCapabilities::minimal();

// ── Core message types ─────────────────────────────────────────────────────────

/// A message received from a channel.
///
/// Serializable so it can be persisted on `Session.last_message`—the
/// orchestrator needs the original incoming message context (sender,
/// reply_target, attached images) for ask_user, push_event, and recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
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
    /// Optional inline buttons (Telegram inline_keyboard, etc.)
    /// Channels that don't support buttons silently ignore this field.
    pub inline_buttons: Option<Vec<InlineButton>>,
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
            inline_buttons: None,
        }
    }
    pub fn is_verbose(&self, chunk_limit: usize) -> bool {
        self.content.chars().count() > chunk_limit
    }
}

/// A media attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Declarative capabilities (RFC §6.1). Default points at
    /// `MINIMAL_CAPABILITIES`; each channel overrides to publish its own
    /// `&'static ChannelCapabilities`.
    fn capabilities(&self) -> &ChannelCapabilities {
        &MINIMAL_CAPABILITIES
    }

    /// Measure text length in the unit declared by `capabilities()`.
    /// Used for chunking and "is this within platform limits" checks.
    fn message_len(&self, text: &str) -> usize {
        match self.capabilities().message_len_unit {
            LenUnit::Codepoints => text.chars().count(),
            LenUnit::Utf16Units => text.encode_utf16().count(),
            LenUnit::Bytes => text.len(),
        }
    }

    /// Whether this channel supports streaming turn events via the
    /// `push_event` mechanism. Default reads from `capabilities()`.
    fn supports_streaming(&self) -> bool {
        self.capabilities().supports_streaming
    }

    /// Forward a per-turn event to the channel addressed by `reply_target`.
    ///
    /// RFC v2 §三.B: replaces the prepare/take_stream_context two-call dance.
    /// `Agent.run()` calls this whenever it has a `TurnEvent` to surface
    /// (text chunk, tool call, thinking delta). Non-streaming channels keep
    /// the default no-op; `ClientChannel` overrides to push into the
    /// per-reply_target stream.
    async fn push_event(&self, _reply_target: &str, _event: TurnEvent) {}

    /// Get a cancellation token for the current turn at `reply_target`, if any.
    ///
    /// RFC v2 §三.B: `Agent.run()` polls this once per turn to decide whether
    /// it should abort the LLM call on user request. Non-streaming channels
    /// keep the default `None`; `ClientChannel` overrides to look up the
    /// token registered via `cancel_signal_register`.
    fn cancel_signal(&self, _reply_target: &str) -> Option<CancellationToken> {
        None
    }
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

/// Split a message into chunks of at most `limit` units, where the unit
/// (codepoints / UTF-16 code units / bytes) is platform-specific.
///
/// Splitting priority:
/// 1. Double newline (paragraph boundary)
/// 2. Single newline (line boundary)
/// 3. Space (word boundary)
/// 4. Hard cut at limit (last resort)
///
/// Preserves code blocks by extending past the limit to the closing fence
/// when possible.
pub fn split_message_chunk(message: &str, limit: usize, unit: LenUnit) -> Vec<String> {
    if measure(message, unit) <= limit {
        return vec![message.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = message;

    while !remaining.is_empty() {
        if measure(remaining, unit) <= limit {
            chunks.push(remaining.to_string());
            break;
        }

        let split_char_pos = find_split_point(remaining, limit, unit);
        let byte_pos = remaining
            .char_indices()
            .nth(split_char_pos)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let (chunk, rest) = remaining.split_at(byte_pos);
        chunks.push(chunk.trim_end().to_string());
        remaining = rest.trim_start_matches([' ', '\t']);
    }

    chunks
}

/// Backwards-compatible wrapper: split by codepoints.
/// New code should pass an explicit `LenUnit`.
pub fn split_message_chunk_chars(message: &str, limit: usize) -> Vec<String> {
    split_message_chunk(message, limit, LenUnit::Codepoints)
}

fn measure(s: &str, unit: LenUnit) -> usize {
    match unit {
        LenUnit::Codepoints => s.chars().count(),
        LenUnit::Utf16Units => s.encode_utf16().count(),
        LenUnit::Bytes => s.len(),
    }
}

/// Per-codepoint cost in the requested unit. Used to convert a unit-budget
/// into a codepoint-index when scanning.
fn char_cost(c: char, unit: LenUnit) -> usize {
    match unit {
        LenUnit::Codepoints => 1,
        LenUnit::Utf16Units => c.len_utf16(),
        LenUnit::Bytes => c.len_utf8(),
    }
}

/// Find the best position to split text, preferring natural boundaries.
/// Returns a codepoint index.
fn find_split_point(text: &str, limit: usize, unit: LenUnit) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

    // Convert unit-limit into a codepoint cap: largest k such that the cost
    // of chars[..k] is <= limit.
    let cap = {
        let mut acc = 0usize;
        let mut k = 0;
        for &c in &chars {
            let cost = char_cost(c, unit);
            if acc + cost > limit {
                break;
            }
            acc += cost;
            k += 1;
        }
        k.min(len)
    };

    // Detect whether the cap lands inside a code block; if so, try to extend
    // past the closing fence (over the unit budget by design).
    let mut in_code_block = false;
    let mut i = 0;
    while i < cap {
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            in_code_block = !in_code_block;
            i += 3;
            continue;
        }
        i += 1;
    }
    if in_code_block {
        let mut j = cap;
        while j + 2 < len {
            if chars[j] == '`' && chars[j + 1] == '`' && chars[j + 2] == '`' {
                return (j + 3).min(len);
            }
            j += 1;
        }
    }

    if let Some(pos) = find_last_pattern(&chars[..cap], &['\n', '\n']) {
        return pos + 2;
    }
    if let Some(pos) = find_last_char(&chars[..cap], '\n') {
        return pos + 1;
    }
    if let Some(pos) = find_last_char(&chars[..cap], ' ') {
        return pos + 1;
    }
    cap.max(1)
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