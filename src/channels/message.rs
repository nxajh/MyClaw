//! channels_message — Shared channel message types.

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

// ── MessagePayload (RFC §6.2-§6.4) ─────────────────────────────────────────────

/// Platform-specific message id (e.g. Telegram message_id, QQBot msg_id).
/// Returned by `send_payload` / `edit_message` and accepted by
/// `edit_message` / `delete_message` to identify a previously sent message.
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

/// Result of `send_payload`. `Some(id)` when the platform returns a
/// message identifier (Telegram, QQBot do); `None` for fire-and-forget
/// transports (current ClientChannel / Wechat).
pub type SendResult = Option<MessageId>;

/// Where to deliver a message. Replaces the routing fields of
/// `SendMessage` (recipient + thread_ts + cancellation_token).
#[derive(Debug, Clone)]
pub struct SendTarget {
    pub recipient: String,
    pub thread_id: Option<String>,
    pub cancellation_token: Option<CancellationToken>,
}

impl SendTarget {
    pub fn new(recipient: impl Into<String>) -> Self {
        Self {
            recipient: recipient.into(),
            thread_id: None,
            cancellation_token: None,
        }
    }

    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }
}

/// Source of media content for `MessagePayload::Media`.
#[derive(Debug, Clone)]
pub enum MediaSource {
    /// Remote URL (HTTP/HTTPS).
    Url(String),
    /// In-memory bytes (e.g. from Telegram file API decryption).
    Inline {
        data: Vec<u8>,
        mime_type: Option<String>,
        file_name: Option<String>,
    },
}

/// What to send. Replaces the content/attachments/image_urls/inline_buttons
/// salad of `SendMessage` with a closed enum.
///
/// Channels that don't support a variant downgrade via `to_fallback_text`
/// or return an error from `send_payload` (implementation choice).
#[derive(Debug, Clone)]
pub enum MessagePayload {
    /// Plain text.
    Text { text: String },
    /// Text + inline action buttons. Channels without button support
    /// downgrade by sending the text alone.
    Interactive {
        text: String,
        buttons: Vec<InlineButton>,
    },
    /// Media (image/file) with optional caption.
    Media {
        source: MediaSource,
        caption: Option<String>,
    },
}

impl MessagePayload {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Lossy downgrade to a plain-text representation. Used by the default
    /// `send_payload` impl to keep non-overriding channels functional.
    pub fn to_fallback_text(&self) -> String {
        match self {
            MessagePayload::Text { text } => text.clone(),
            MessagePayload::Interactive { text, buttons } => {
                let mut s = text.clone();
                if !buttons.is_empty() {
                    s.push_str("\n[");
                    for (i, b) in buttons.iter().enumerate() {
                        if i > 0 {
                            s.push_str(" | ");
                        }
                        s.push_str(&b.label);
                    }
                    s.push(']');
                }
                s
            }
            MessagePayload::Media { caption, source } => {
                let url_or_size = match source {
                    MediaSource::Url(u) => format!("<media: {}>", u),
                    MediaSource::Inline { data, file_name, .. } => match file_name {
                        Some(f) => format!("<media: {} ({} bytes)>", f, data.len()),
                        None => format!("<media: {} bytes>", data.len()),
                    },
                };
                match caption {
                    Some(c) => format!("{}\n{}", c, url_or_size),
                    None => url_or_size,
                }
            }
        }
    }
}

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

// ── Callback actions (RFC §11 Phase 5) ─────────────────────────────────────────

/// Structured button callback action.
///
/// Replaces the `__retry:{sk_prefix}` / `__abort:{sk_prefix}` string-prefix
/// convention with a closed enum. `serialize()` produces the wire format
/// embedded in `InlineButton.callback_data` (kept under Telegram's 64-byte
/// limit); `parse()` recognises inbound callback strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackAction {
    /// User asked to retry the last failed turn.
    Retry { session_key_prefix: String },
    /// User asked to abort the pending retry prompt.
    Abort { session_key_prefix: String },
    /// Future-extension hook for app-defined callbacks.
    Custom { tag: String, data: String },
}

impl CallbackAction {
    /// Wire format: `__<tag>:<payload>`. Stays compatible with the
    /// historical `__retry:` / `__abort:` prefixes so messages produced
    /// by older orchestrator builds still parse.
    pub fn serialize(&self) -> String {
        match self {
            Self::Retry { session_key_prefix } => format!("__retry:{}", session_key_prefix),
            Self::Abort { session_key_prefix } => format!("__abort:{}", session_key_prefix),
            Self::Custom { tag, data } => format!("__{}:{}", tag, data),
        }
    }

    /// Parse a callback_data / inbound text string into a `CallbackAction`.
    /// Returns `None` for non-callback text (regular user messages).
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("__")?;
        let (tag, data) = rest.split_once(':')?;
        match tag {
            "retry" => Some(Self::Retry {
                session_key_prefix: data.to_string(),
            }),
            "abort" => Some(Self::Abort {
                session_key_prefix: data.to_string(),
            }),
            other => Some(Self::Custom {
                tag: other.to_string(),
                data: data.to_string(),
            }),
        }
    }
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

    /// Structured send (RFC §6.2 / Phase 2). Default downgrades to
    /// `send()` using `payload.to_fallback_text()`; channels override to
    /// natively render `Interactive` (inline keyboards) and `Media`
    /// (image / file uploads). Returns the platform message id when one
    /// is produced.
    async fn send_payload(
        &self,
        target: &SendTarget,
        payload: &MessagePayload,
    ) -> anyhow::Result<SendResult> {
        let mut msg = SendMessage::new(payload.to_fallback_text(), &target.recipient);
        msg.thread_ts = target.thread_id.clone();
        msg.cancellation_token = target.cancellation_token.clone();
        if let MessagePayload::Interactive { buttons, .. } = payload {
            msg.inline_buttons = Some(buttons.clone());
        }
        self.send(&msg).await?;
        Ok(None)
    }

    /// Edit a previously sent message in-place. Default returns Err for
    /// channels that don't support edit (RFC §7.1; Phase 3 will add
    /// real impls on Telegram).
    async fn edit_message(
        &self,
        _target: &SendTarget,
        _message_id: &MessageId,
        _payload: &MessagePayload,
    ) -> anyhow::Result<()> {
        anyhow::bail!("edit_message not supported by {}", self.name())
    }

    /// Delete a previously sent message. Default returns Err.
    async fn delete_message(
        &self,
        _target: &SendTarget,
        _message_id: &MessageId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("delete_message not supported by {}", self.name())
    }

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
    /// `create_stream` mechanism. Default reads from `capabilities()`.
    fn supports_streaming(&self) -> bool {
        self.capabilities().supports_streaming
    }

    /// Create a per-turn streaming output handle.
    ///
    /// RFC §7.6 (Phase 1.5): replaces `push_event` + `cancel_signal`.
    /// `SessionContext::process_turn` installs the returned stream on
    /// `Session.turn_stream` before invoking `Agent::run`; the agent
    /// pushes via `session.turn_stream.as_mut()`. Non-streaming channels
    /// return `None`; the agent then falls through to the
    /// `send_payload` / `send` fallback at end of turn.
    fn create_stream(
        &self,
        _reply_target: &str,
    ) -> Option<Box<dyn crate::channels::TurnStream>> {
        None
    }

    /// Authorization policy snapshot for this channel (RFC §14).
    /// Default: open policy — used by Client (connection-level token authn).
    /// Hot-reload-capable channels read through their internal RwLock and
    /// return a cloned snapshot.
    fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
        crate::channels::ChannelSecurityPolicy::open()
    }

    /// Decide whether an inbound message is authorized. Channels call this
    /// in their listen/poll loop before forwarding to the orchestrator.
    /// Default implementation delegates to `security_policy()`.
    fn check_authorization(
        &self,
        sender: &str,
        scope: crate::channels::MessageScope<'_>,
    ) -> crate::channels::AuthDecision {
        crate::channels::security::evaluate(&self.security_policy(), sender, scope)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_action_roundtrip_retry_abort() {
        let r = CallbackAction::Retry { session_key_prefix: "abc123".into() };
        assert_eq!(r.serialize(), "__retry:abc123");
        assert_eq!(CallbackAction::parse("__retry:abc123"), Some(r));

        let a = CallbackAction::Abort { session_key_prefix: "xyz".into() };
        assert_eq!(a.serialize(), "__abort:xyz");
        assert_eq!(CallbackAction::parse("__abort:xyz"), Some(a));
    }

    #[test]
    fn callback_action_custom_passthrough() {
        let c = CallbackAction::Custom { tag: "vote".into(), data: "yes".into() };
        assert_eq!(c.serialize(), "__vote:yes");
        assert_eq!(CallbackAction::parse("__vote:yes"), Some(c));
    }

    #[test]
    fn callback_action_rejects_non_callback() {
        assert_eq!(CallbackAction::parse("hello world"), None);
        assert_eq!(CallbackAction::parse("__noseparator"), None);
        assert_eq!(CallbackAction::parse(""), None);
    }

    #[test]
    fn message_payload_fallback_text() {
        let t = MessagePayload::text("hi");
        assert_eq!(t.to_fallback_text(), "hi");

        let i = MessagePayload::Interactive {
            text: "Pick one".into(),
            buttons: vec![
                InlineButton { label: "A".into(), callback_data: "a".into() },
                InlineButton { label: "B".into(), callback_data: "b".into() },
            ],
        };
        assert_eq!(i.to_fallback_text(), "Pick one\n[A | B]");
    }
}