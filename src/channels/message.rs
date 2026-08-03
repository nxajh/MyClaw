//! channels_message — Shared channel message types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::io::AsyncRead;
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
    pub supports_file_send: bool,
    pub supports_file_receive: bool,
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
            supports_file_send: false,
            supports_file_receive: false,
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
            supports_file_send: true,
            supports_file_receive: true,
            supports_threads: false,
            message_chunk_limit: 65_536,
            message_len_unit: LenUnit::Codepoints,
        }
    }

    /// Telegram bot: 32 768 codepoints (rich messages), edit/delete/buttons/media.
    pub const fn telegram() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: true,
            supports_delete: true,
            supports_inline_buttons: true,
            supports_file_send: true,
            supports_file_receive: true,
            supports_threads: true,
            message_chunk_limit: 32768,
            message_len_unit: LenUnit::Codepoints,
        }
    }

    /// QQBot: 2000 codepoints, inline buttons via Keyboard.
    pub const fn qqbot() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: false,
            supports_delete: false,
            supports_inline_buttons: true,
            supports_file_send: true,
            supports_file_receive: true,
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
            supports_file_send: true,
            supports_file_receive: true,
            supports_threads: false,
            message_chunk_limit: 2048,
            message_len_unit: LenUnit::Codepoints,
        }
    }
}

/// Static default returned by `Channel::capabilities()` when an
/// implementation doesn't override it. Zero-cost reference.
pub static MINIMAL_CAPABILITIES: ChannelCapabilities = ChannelCapabilities::minimal();

// ── Outbound-message RFC phase-1 types ──────────────────────────────────────

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

/// A runtime message received from a channel.
#[derive(Debug, Clone)]
pub struct ChannelInboundMessage {
    pub id: String,
    pub sender: MessageSender,
    pub receiver: MessageReceiver,
    pub content: ChannelMessageContent,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
}

impl ChannelInboundMessage {
    /// Convert to the serializable form for session persistence.
    /// File bodies (runtime-only) are dropped; only text and routing survive.
    pub fn to_persisted(&self) -> PersistedChannelMessage {
        PersistedChannelMessage {
            id: self.id.clone(),
            sender_id: self.sender.id.clone(),
            receiver: self.receiver.clone(),
            text: self.content.text.clone(),
            timestamp: self.timestamp,
            interruption_scope_id: self.interruption_scope_id.clone(),
        }
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

    /// Send a fully-typed outbound message.
    async fn send_message(
        &self,
        msg: &ChannelOutboundMessage,
    ) -> anyhow::Result<OutboundSendResult> {
        if !msg.content.files.is_empty() {
            anyhow::bail!("send_message with files not supported by {}", self.name());
        }
        anyhow::bail!("send_message not implemented by {}", self.name())
    }

    /// Edit a previously sent message's content.
    /// Default: not supported — channels that support edit override this.
    async fn edit_message(
        &self,
        _receiver: &MessageReceiver,
        _message_id: &MessageId,
        _content: ChannelMessageContent,
    ) -> anyhow::Result<()> {
        anyhow::bail!("edit_message not supported by {}", self.name())
    }

    /// Delete a previously sent message.
    /// Default: not supported — channels that support delete override this.
    async fn delete_message(
        &self,
        _receiver: &MessageReceiver,
        _message_id: &MessageId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("delete_message not supported by {}", self.name())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelInboundMessage>>;
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
    /// `send_message` fallback at end of turn.
    fn create_stream(&self, _reply_target: &str) -> Option<Box<dyn crate::channels::TurnStream>> {
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
///
/// Uses a bounded FIFO ring: when `seen` reaches `capacity`, the oldest
/// half is evicted. This caps memory at O(capacity) regardless of how
/// long the daemon runs. Telegram update_ids are monotonic so eviction
/// never causes false negatives in practice — by the time an id is
/// evicted, Telegram's never going to redeliver an update from millions
/// of messages ago.
#[derive(Clone)]
pub struct DedupState {
    inner: Arc<Mutex<DedupInner>>,
    capacity: usize,
}

struct DedupInner {
    /// Set membership for O(1) lookup.
    seen: std::collections::HashSet<String>,
    /// Insertion order for FIFO eviction.
    order: std::collections::VecDeque<String>,
}

/// Cap memory at ~50K entries. At 32 bytes/entry average that's ~1.5 MB
/// per channel — well below noise floor. High-volume Telegram bots
/// processing 100 msg/sec keep ~8 minutes of history; recovery /
/// replay windows are far shorter than that.
const DEFAULT_DEDUP_CAPACITY: usize = 50_000;

impl Default for DedupState {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_DEDUP_CAPACITY)
    }
}

impl DedupState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DedupInner {
                seen: std::collections::HashSet::with_capacity(capacity),
                order: std::collections::VecDeque::with_capacity(capacity),
            })),
            capacity,
        }
    }

    /// Check if an update ID has been seen, and record it if not.
    /// Returns true if the ID was already seen (should skip), false if new.
    pub fn check_and_record(&self, id: &str) -> bool {
        // Poison-recover: the critical section never panics, and a poisoned
        // dedup cache must not cascade-crash the inbound path of a daemon.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.seen.contains(id) {
            return true;
        }
        let owned = id.to_string();
        inner.seen.insert(owned.clone());
        inner.order.push_back(owned);
        // Evict the oldest half when we hit capacity. Half-and-clear is
        // amortized O(1) per insert and avoids per-insert eviction churn.
        if inner.order.len() > self.capacity {
            let drop_n = self.capacity / 2;
            for _ in 0..drop_n {
                if let Some(old) = inner.order.pop_front() {
                    inner.seen.remove(&old);
                }
            }
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap().seen.len()
    }
}

/// Split a message into chunks of at most `limit` units, where the unit
/// (codepoints / UTF-16 code units / bytes) is platform-specific.
///
/// Splitting priority:
/// 1. Outside fenced code regions when possible (whole fence blocks stay atomic)
/// 2. Double newline (paragraph boundary)
/// 3. Single newline (line boundary)
/// 4. Space (word boundary)
/// 5. Hard cut at limit (last resort)
///
/// `limit` is a hard cap: every returned chunk measures `<= limit` units.
///
/// Fences are line-level (CommonMark-ish): only a line starting with 0–3 spaces
/// and ≥3 `` ` `` or `~` opens/closes a block. Mid-line triple backticks do not
/// toggle. When a split must land inside an oversized fence body, the chunk gets
/// a matching close and the next chunk reopens with the same marker/info so each
/// piece is independently valid markdown. Unclosed fences on the final chunk are
/// force-closed.
pub fn split_message_chunk(message: &str, limit: usize, unit: LenUnit) -> Vec<String> {
    if measure(message, unit) <= limit {
        // Still force-close a dangling fence so a single short chunk cannot leave
        // the channel renderer swallowing later UI (QQ unclosed-fence behavior).
        if let Some(fc) = trailing_open_carry(message, None) {
            let mut s = message.trim_end().to_string();
            s.push_str(&fc.close_suffix());
            if measure(&s, unit) <= limit {
                return vec![s];
            }
            // Close suffix would overflow — fall through to the splitter so the
            // last body piece and the synthetic close stay within `limit`.
        } else {
            return vec![message.to_string()];
        }
    }

    let mut chunks = Vec::new();
    let mut remaining = message;
    // Set when the previous chunk ended inside an open fence body: reopen it.
    let mut fence_carry: Option<FenceCarry> = None;
    // Set when the previous chunk ended inside a table body: repeat the
    // "header\ndelimiter\n" so the continuation still renders as a table.
    let mut table_carry: Option<String> = None;
    // Inline markers (`**`/`~~`/`` ` ``) open across the previous cut, reopened
    // at the start of this chunk.
    let mut inline_carry: Vec<Marker> = Vec::new();
    // Worst-case room for inline closers appended to a chunk ("**" + "~~" + "`").
    let inline_reserve = measure("**~~`", unit);

    while !remaining.is_empty() {
        // Front matter repeated on this chunk (reopened fence or table header,
        // then any reopened inline markers), reserved out of the budget so the
        // emitted chunk still fits `limit`.
        let block_prefix: String = if let Some(ref fc) = fence_carry {
            fc.open_line()
        } else {
            table_carry.clone().unwrap_or_default()
        };
        let inline_reopen: String = inline_carry.iter().map(|m| m.token()).collect();
        let prefix = format!("{block_prefix}{inline_reopen}");
        // When continuing a fence, inline closers are not appended on this path.
        let reserve = if fence_carry.is_some() {
            0
        } else {
            inline_reserve
        };
        let content_limit = limit
            .saturating_sub(measure(&prefix, unit))
            .saturating_sub(reserve);

        if measure(remaining, unit) <= content_limit {
            let mut chunk = String::with_capacity(prefix.len() + remaining.len() + 8);
            chunk.push_str(&prefix);
            chunk.push_str(remaining.trim_end());
            if let Some(fc) = trailing_open_carry(remaining, fence_carry.as_ref()) {
                let closed = format!("{}{}", chunk, fc.close_suffix());
                if measure(&closed, unit) <= limit {
                    chunks.push(closed);
                    break;
                }
                // Close would overflow: reserve close cost and cut body so the
                // synthetic close still fits (do not leave an open final chunk).
                let close_cost = measure(&fc.close_suffix(), unit);
                let body_limit = limit
                    .saturating_sub(measure(&prefix, unit))
                    .saturating_sub(close_cost)
                    .max(1);
                let split = find_split_point(
                    remaining,
                    body_limit,
                    unit,
                    fence_carry.as_ref(),
                    &inline_carry,
                );
                let byte_pos = remaining
                    .char_indices()
                    .nth(split.index)
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len());
                // Ensure we make progress and leave room for close.
                let byte_pos = if byte_pos == 0 || byte_pos >= remaining.len() {
                    remaining
                        .char_indices()
                        .nth(1.min(remaining.chars().count().saturating_sub(1)))
                        .map(|(i, _)| i)
                        .unwrap_or(remaining.len())
                } else {
                    byte_pos
                };
                let (head, _rest_unused) = remaining.split_at(byte_pos);
                let fc_use = split.fence_carry.unwrap_or(fc);
                // Prefer carry from split; budget body so prefix+body+close <= limit.
                let close_sfx = fc_use.close_suffix();
                let body_budget = limit
                    .saturating_sub(measure(&prefix, unit))
                    .saturating_sub(measure(&close_sfx, unit));
                let mut body = trim_to_unit_budget(head.trim_end(), body_budget, unit);
                // Guarantee progress: never emit a zero-length body while text remains
                // (would infinite-loop). If budget is 0, still take one char.
                if body.is_empty() && !remaining.is_empty() {
                    let one = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                    body = &remaining[..one];
                }
                // `body` is a byte-prefix of `remaining` (head is a prefix; body of head).
                let piece = format!("{prefix}{body}{close_sfx}");
                chunks.push(piece);
                fence_carry = Some(fc_use);
                table_carry = None;
                inline_carry = Vec::new();
                remaining = &remaining[body.len()..];
                continue;
            } else {
                chunks.push(chunk);
                break;
            }
        }

        let split = find_split_point(
            remaining,
            content_limit,
            unit,
            fence_carry.as_ref(),
            &inline_carry,
        );
        let byte_pos = remaining
            .char_indices()
            .nth(split.index)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let (head, rest) = remaining.split_at(byte_pos);

        let mut chunk = String::with_capacity(prefix.len() + head.len() + 8);
        chunk.push_str(&prefix);
        chunk.push_str(head.trim_end());

        if let Some(ref fc) = split.fence_carry {
            // Close the fence here; the next chunk reopens with the same info.
            chunk.push_str(&fc.close_suffix());
            fence_carry = Some(fc.clone());
            table_carry = None;
            inline_carry = Vec::new();
        } else {
            fence_carry = None;
            // Close any open inline markers (innermost first) so this chunk is
            // self-contained; reopen them on the next chunk.
            for m in split.markers.iter().rev() {
                chunk.push_str(m.token());
            }
            inline_carry = split.markers;
            // A table left open by this cut is repeated on the next chunk.
            table_carry = open_table_header(&chunk, rest);
        }
        chunks.push(chunk);

        // Leading whitespace is significant indentation inside a code block,
        // so only strip it between plain-text chunks.
        remaining = if fence_carry.is_some() {
            rest
        } else {
            rest.trim_start_matches([' ', '\t'])
        };
    }

    chunks
}

/// A markdown table row contains at least one `|`.
fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.contains('|')
}

/// A markdown table delimiter row, e.g. `| --- | :--: |` — only `|`, `-`, `:`,
/// and whitespace, with at least one `-`.
fn is_table_delimiter(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let mut has_dash = false;
    for c in t.chars() {
        match c {
            '-' => has_dash = true,
            '|' | ':' | ' ' | '\t' => {}
            _ => return false,
        }
    }
    has_dash
}

/// If `chunk` ends inside a table body that continues in `rest`, return the
/// "header\ndelimiter\n" to repeat on the next chunk so it still renders as a
/// table. Returns `None` when no table is open across the cut.
fn open_table_header(chunk: &str, rest: &str) -> Option<String> {
    // The continuation must itself begin with a table row.
    let next = rest.lines().find(|l| !l.trim().is_empty())?;
    if !is_table_row(next) {
        return None;
    }

    let mut header: Option<&str> = None;
    let mut delimiter: Option<&str> = None;
    let mut in_table = false;
    let mut prev: Option<&str> = None;
    for line in chunk.lines() {
        if in_table && !is_table_row(line) {
            in_table = false;
            header = None;
            delimiter = None;
        }
        if !in_table {
            if let Some(p) = prev {
                if is_table_delimiter(line) && is_table_row(p) {
                    in_table = true;
                    header = Some(p);
                    delimiter = Some(line);
                }
            }
        }
        prev = Some(line);
    }

    if in_table {
        Some(format!("{}\n{}\n", header?, delimiter?))
    } else {
        None
    }
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

/// Keep a prefix of `s` whose `measure` is ≤ `budget` (by `unit`).
fn trim_to_unit_budget(s: &str, budget: usize, unit: LenUnit) -> &str {
    if measure(s, unit) <= budget {
        return s;
    }
    let mut acc = 0usize;
    let mut end = 0usize;
    for (i, c) in s.char_indices() {
        let cost = char_cost(c, unit);
        if acc + cost > budget {
            break;
        }
        acc += cost;
        end = i + c.len_utf8();
    }
    &s[..end]
}

/// How far back to scan for a link enclosing the cut point.
const LINK_SCAN_WINDOW: usize = 4096;

/// Paired inline markdown markers that the splitter balances across chunks.
/// Single-character `*`/`_` italic is intentionally excluded: it is ambiguous
/// and replicating the renderer's heuristic risks mis-emphasizing plain text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    Bold,
    Strike,
    Code,
}

impl Marker {
    fn token(self) -> &'static str {
        match self {
            Marker::Bold => "**",
            Marker::Strike => "~~",
            Marker::Code => "`",
        }
    }
}

fn toggle_marker(stack: &mut Vec<Marker>, m: Marker) {
    if let Some(pos) = stack.iter().rposition(|&t| t == m) {
        stack.remove(pos);
    } else {
        stack.push(m);
    }
}

/// State needed to reopen a fenced code block on the next chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FenceCarry {
    /// `` ` `` or `~`.
    marker: char,
    /// Number of marker chars in the opening fence (≥ 3).
    len: usize,
    /// Info string after the opening fence markers (e.g. "rust"), no newline.
    info: String,
}

impl FenceCarry {
    fn open_line(&self) -> String {
        let mut s = String::with_capacity(self.len + self.info.len() + 2);
        for _ in 0..self.len {
            s.push(self.marker);
        }
        if !self.info.is_empty() {
            s.push_str(&self.info);
        }
        s.push('\n');
        s
    }

    fn close_suffix(&self) -> String {
        let mut s = String::with_capacity(self.len + 1);
        s.push('\n');
        for _ in 0..self.len {
            s.push(self.marker);
        }
        s
    }
}

/// A fenced region in codepoint indices: `[start, end)` covering open line
/// through close line (or through EOF if unclosed). `body_start` is the first
/// index after the opening fence line's trailing newline (or `start` if none).
#[derive(Clone, Debug)]
struct FenceRegion {
    start: usize,
    body_start: usize,
    end: usize,
    closed: bool,
    carry: FenceCarry,
}

/// Line-level fence line: optional ≤3 leading spaces, then ≥3 `` ` `` or `~`.
/// Closing fences may only use the fence char + optional trailing spaces/tabs.
#[derive(Clone, Copy, Debug)]
struct FenceLine {
    marker: char,
    len: usize,
    /// Codepoint offset of info start within the line content after markers;
    /// only meaningful for opening fences.
    info_start_in_line: usize,
    info_end_in_line: usize,
    is_closing_candidate: bool,
}

fn parse_fence_line(line: &str) -> Option<FenceLine> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() && i < 3 && chars[i] == ' ' {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    let marker = chars[i];
    if marker != '`' && marker != '~' {
        return None;
    }
    let start = i;
    while i < chars.len() && chars[i] == marker {
        i += 1;
    }
    let len = i - start;
    if len < 3 {
        return None;
    }
    // Closing fence: rest of line must be only spaces/tabs.
    let rest = &chars[i..];
    let is_closing_candidate = rest.iter().all(|c| *c == ' ' || *c == '\t');
    // Opening info: no backtick may appear in info for backtick fences (CM).
    if marker == '`' && rest.contains(&'`') && !is_closing_candidate {
        return None;
    }
    Some(FenceLine {
        marker,
        len,
        info_start_in_line: i,
        info_end_in_line: chars.len(),
        is_closing_candidate,
    })
}

/// Scan `text` for fenced regions using line-level rules.
/// If `start_carry` is set, the text begins already inside that fence body
/// (continuation chunk); the synthetic open is not present in `text`.
fn scan_fence_regions(text: &str, start_carry: Option<&FenceCarry>) -> Vec<FenceRegion> {
    let chars: Vec<char> = text.chars().collect();
    let mut regions = Vec::new();
    let mut i = 0usize;
    let mut open: Option<(usize, usize, FenceCarry)> = None; // start, body_start, carry

    if let Some(fc) = start_carry {
        // Continuation: body starts at 0; no open line in this slice.
        open = Some((0, 0, fc.clone()));
    }

    while i < chars.len() {
        let line_start = i;
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        let line_end = i; // exclusive, before '\n' if any
        let line: String = chars[line_start..line_end].iter().collect();
        let after_line = if i < chars.len() && chars[i] == '\n' {
            i + 1
        } else {
            i
        };

        if let Some((start, body_start, ref carry)) = open.clone() {
            if let Some(fl) = parse_fence_line(&line) {
                if fl.is_closing_candidate
                    && fl.marker == carry.marker
                    && fl.len >= carry.len
                {
                    regions.push(FenceRegion {
                        start,
                        body_start,
                        end: after_line,
                        closed: true,
                        carry: carry.clone(),
                    });
                    open = None;
                    i = after_line;
                    continue;
                }
            }
            i = after_line;
            continue;
        }

        // Not inside a fence: opening fence?
        if let Some(fl) = parse_fence_line(&line) {
            if !fl.is_closing_candidate || fl.len >= 3 {
                // Treat as open (CM: a fence line that could be close still opens
                // when not inside a block — empty open then immediate close is
                // handled by the next iteration if needed). Prefer opening when
                // the line has info or is a standard open.
                let info: String = if fl.is_closing_candidate {
                    String::new()
                } else {
                    line.chars()
                        .skip(fl.info_start_in_line)
                        .take(fl.info_end_in_line - fl.info_start_in_line)
                        .collect::<String>()
                        .trim_matches([' ', '\t'])
                        .to_string()
                };
                let carry = FenceCarry {
                    marker: fl.marker,
                    len: fl.len,
                    info,
                };
                open = Some((line_start, after_line, carry));
            }
        }
        i = after_line;
    }

    if let Some((start, body_start, carry)) = open {
        regions.push(FenceRegion {
            start,
            body_start,
            end: chars.len(),
            closed: false,
            carry,
        });
    }

    regions
}

/// If `text` ends still inside a fence (optionally continuing `start_carry`),
/// return the carry needed to close it.
fn trailing_open_carry(text: &str, start_carry: Option<&FenceCarry>) -> Option<FenceCarry> {
    let regions = scan_fence_regions(text, start_carry);
    regions.last().and_then(|r| {
        if !r.closed && r.end == text.chars().count() {
            Some(r.carry.clone())
        } else {
            None
        }
    })
}

fn region_containing(regions: &[FenceRegion], pos: usize) -> Option<&FenceRegion> {
    regions
        .iter()
        .find(|r| r.start <= pos && pos < r.end)
}

fn region_overlapping_cut(regions: &[FenceRegion], index: usize) -> Option<&FenceRegion> {
    // Cut at `index` is inside a region if the region covers that point.
    region_containing(regions, index)
}

/// Inline markers (`**`, `~~`, single `` ` ``) left open at codepoint index
/// `upto`, in the order opened. `start` is the marker state at the beginning of
/// `chars` (markers carried over from a previous chunk). Markers inside fenced
/// code regions (line-level) are ignored; inside an inline-code span only a
/// closing `` ` `` is recognized.
fn open_inline_markers(
    chars: &[char],
    upto: usize,
    start: &[Marker],
    regions: &[FenceRegion],
) -> Vec<Marker> {
    let len = chars.len();
    let upto = upto.min(len);
    let mut stack: Vec<Marker> = start.to_vec();
    let mut i = 0;
    while i < upto {
        if region_containing(regions, i).is_some() {
            i += 1;
            continue;
        }
        // Inside an inline-code span only a backtick (closing it) matters.
        if stack.last() == Some(&Marker::Code) {
            if chars[i] == '`' {
                stack.pop();
            }
            i += 1;
            continue;
        }
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            // Do not treat line-level `~~~` fences as strike — those are inside
            // regions and already skipped. Mid-line `~~` still toggles strike.
            toggle_marker(&mut stack, Marker::Strike);
            i += 2;
            continue;
        }
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            toggle_marker(&mut stack, Marker::Bold);
            i += 2;
            continue;
        }
        if chars[i] == '`' {
            stack.push(Marker::Code);
            i += 1;
            continue;
        }
        i += 1;
    }
    stack
}

/// Parse a `[text](url)` link starting at `start` (where `chars[start] == '['`),
/// returning the exclusive end index just past `)`. Link text and URL may not
/// span a newline.
fn parse_link_end(chars: &[char], start: usize) -> Option<usize> {
    let len = chars.len();
    let mut i = start + 1;
    while i < len && chars[i] != ']' {
        if chars[i] == '\n' {
            return None;
        }
        i += 1;
    }
    if i >= len || i + 1 >= len || chars[i + 1] != '(' {
        return None;
    }
    let mut j = i + 2;
    while j < len && chars[j] != ')' {
        if chars[j] == '\n' {
            return None;
        }
        j += 1;
    }
    if j >= len {
        return None;
    }
    Some(j + 1)
}

/// If codepoint `index` falls strictly inside a `[text](url)` link, return that
/// link's start so the caller can cut before it instead of breaking it.
fn link_start_before(chars: &[char], index: usize) -> Option<usize> {
    let lo = index.saturating_sub(LINK_SCAN_WINDOW);
    let mut p = index.min(chars.len());
    while p > lo {
        p -= 1;
        if chars[p] != '[' {
            continue;
        }
        if let Some(end) = parse_link_end(chars, p) {
            if p < index && index < end {
                return Some(p);
            }
        }
    }
    None
}

/// Result of choosing where to split a chunk.
struct SplitPoint {
    /// Codepoint index in `text` at which to cut.
    index: usize,
    /// When set, the cut is inside a fence body: close this chunk and reopen
    /// the next with this carry. `None` means the cut is outside any fence.
    fence_carry: Option<FenceCarry>,
    /// Inline markers open at the cut (non-code only), in open order, so the
    /// caller can close them on this chunk and reopen them on the next.
    markers: Vec<Marker>,
}

/// Find the best position to split text, preferring natural boundaries while
/// never exceeding `limit` units. `start_carry` is set when continuing a fence
/// body from a prior chunk.
///
/// Strategy (B+):
/// 1. Prefer cuts **outside** fenced regions so whole blocks stay atomic.
/// 2. Never cut through a fence line (open/close markers stay intact).
/// 3. Only cut inside a fence body when that single region exceeds the budget;
///    then reserve room for a synthetic close on this chunk.
fn find_split_point(
    text: &str,
    limit: usize,
    unit: LenUnit,
    start_carry: Option<&FenceCarry>,
    start_markers: &[Marker],
) -> SplitPoint {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let regions = scan_fence_regions(text, start_carry);

    // Largest codepoint count whose cumulative cost is <= budget.
    let cap_for = |budget: usize| -> usize {
        let mut acc = 0usize;
        let mut k = 0;
        for &c in &chars {
            let cost = char_cost(c, unit);
            if acc + cost > budget {
                break;
            }
            acc += cost;
            k += 1;
        }
        k.min(len)
    };

    let cost_of = |upto: usize| -> usize {
        chars[..upto.min(len)]
            .iter()
            .map(|&c| char_cost(c, unit))
            .sum()
    };

    // Best natural boundary within [0, cap]; hard cut at cap as last resort.
    // Also snap so we never land mid-line on a fence line (handled later).
    let boundary_within = |cap: usize| -> usize {
        let cap = cap.min(len);
        if cap == 0 {
            return 0;
        }
        if let Some(pos) = find_last_pattern(&chars[..cap], &['\n', '\n']) {
            pos + 2
        } else if let Some(pos) = find_last_char(&chars[..cap], '\n') {
            pos + 1
        } else if let Some(pos) = find_last_char(&chars[..cap], ' ') {
            pos + 1
        } else {
            cap.max(1).min(len)
        }
    };

    // If a candidate cut falls inside a fence region, try to move it to a safe
    // boundary. Prefer: end of a fully-included prior region, or just before
    // the region that would be split. Only keep an interior body cut when the
    // region itself cannot fit (oversized).
    let adjust_for_fences = |mut index: usize, budget: usize| -> (usize, Option<FenceCarry>) {
        if index == 0 {
            index = 1.min(len);
        }
        let Some(reg) = region_overlapping_cut(&regions, index) else {
            return (index, None);
        };

        // Cost from 0 to reg.start (prefix before this region).
        let prefix_cost = cost_of(reg.start);

        // If the entire region fits in budget when taken from the start of the
        // content slice, push the cut to reg.end (keep block atomic).
        // Note: budget is for this remaining slice from index 0.
        if cost_of(reg.end) <= budget {
            return (reg.end.max(1).min(len), None);
        }

        // Region does not fit entirely. Prefer cutting before it if there is
        // non-empty content before and that cut fits.
        if reg.start > 0 && prefix_cost <= budget {
            // Degenerate: opening fence at tail with no body in this chunk —
            // cut before the region so the whole block moves next.
            return (reg.start, None);
        }

        // Must cut inside the body (oversized single block, or continuation).
        // Never cut on the open line itself: body starts at body_start.
        let close_cost = measure(&reg.carry.close_suffix(), unit);
        let body_budget = budget.saturating_sub(close_cost);
        let mut body_cap = cap_for(body_budget);
        // Stay within this region; do not include the close line if present.
        let body_limit_idx = if reg.closed {
            // Close line is the last line of the region; body ends at that line's start.
            let mut line_start = reg.end;
            while line_start > reg.body_start && chars[line_start - 1] != '\n' {
                line_start -= 1;
            }
            let last_line: String = chars[line_start..reg.end.min(len)]
                .iter()
                .take_while(|&&c| c != '\n')
                .collect();
            if parse_fence_line(last_line.trim_end_matches('\n')).is_some_and(|fl| {
                fl.is_closing_candidate
                    && fl.marker == reg.carry.marker
                    && fl.len >= reg.carry.len
            }) {
                line_start
            } else {
                reg.end
            }
        } else {
            reg.end
        };

        if body_cap > body_limit_idx {
            body_cap = body_limit_idx;
        }
        if body_cap < reg.body_start {
            // Not even past the open line — cut before region if possible.
            if reg.start > 0 {
                return (reg.start, None);
            }
            // Continuation with tiny budget: take at least one char of body.
            body_cap = (reg.body_start + 1).min(body_limit_idx).max(1);
        }

        // Prefer line/paragraph boundaries inside the body only.
        let body_slice_cap = body_cap;
        let mut cut = if body_slice_cap > reg.body_start {
            let local = boundary_within(body_slice_cap);
            if local > reg.body_start {
                local
            } else {
                body_slice_cap.max(reg.body_start + 1.min(body_limit_idx.saturating_sub(reg.body_start)))
            }
        } else {
            (reg.body_start + 1).min(body_limit_idx).max(1)
        };
        cut = cut.min(body_limit_idx).max(1);

        // If cut still on open line (before body_start), snap to before region or body_start+1.
        if cut <= reg.start {
            return (reg.start.max(1), None);
        }
        if cut < reg.body_start {
            if reg.start > 0 {
                return (reg.start, None);
            }
            cut = reg.body_start.min(body_limit_idx).max(1);
        }

        // Empty body in this chunk (cut == body_start with no content): avoid
        // emitting open+immediate synthetic close — move cut before region.
        if cut <= reg.body_start && reg.start > 0 {
            return (reg.start, None);
        }

        (cut, Some(reg.carry.clone()))
    };

    let hard_cap = cap_for(limit).max(1).min(len);
    let mut index = boundary_within(hard_cap);
    if index == 0 {
        index = hard_cap;
    }
    let (mut index, mut fence_carry) = adjust_for_fences(index, limit);

    // Re-validate cost with synthetic close if needed.
    if let Some(ref fc) = fence_carry {
        let close_cost = measure(&fc.close_suffix(), unit);
        while index > 0 && cost_of(index) + close_cost > limit {
            index = boundary_within(index.saturating_sub(1)).max(1);
            // If we walked out of the fence body, re-adjust.
            let (i2, fc2) = adjust_for_fences(index, limit);
            index = i2;
            fence_carry = fc2;
            if fence_carry.is_none() {
                break;
            }
        }
        // Final clamp: never exceed limit with close suffix.
        if let Some(ref fc) = fence_carry {
            let close_cost = measure(&fc.close_suffix(), unit);
            while index > 1 && cost_of(index) + close_cost > limit {
                index -= 1;
            }
        }
    } else {
        while index > 1 && cost_of(index) > limit {
            index = boundary_within(index.saturating_sub(1)).max(1);
        }
    }

    // Guarantee progress.
    if index == 0 {
        index = 1.min(len);
    }
    if index > len {
        index = len;
    }

    let mut markers = Vec::new();
    if fence_carry.is_none() {
        // Don't cut inside a [text](url) link — move the cut before it.
        if let Some(a) = link_start_before(&chars, index) {
            if a > 0 {
                index = a;
            }
        }
        markers = open_inline_markers(&chars, index, start_markers, &regions);
    }

    SplitPoint {
        index,
        fence_carry,
        markers,
    }
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
        let r = CallbackAction::Retry {
            session_key_prefix: "abc123".into(),
        };
        assert_eq!(r.serialize(), "__retry:abc123");
        assert_eq!(CallbackAction::parse("__retry:abc123"), Some(r));

        let a = CallbackAction::Abort {
            session_key_prefix: "xyz".into(),
        };
        assert_eq!(a.serialize(), "__abort:xyz");
        assert_eq!(CallbackAction::parse("__abort:xyz"), Some(a));
    }

    #[test]
    fn callback_action_custom_passthrough() {
        let c = CallbackAction::Custom {
            tag: "vote".into(),
            data: "yes".into(),
        };
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
    fn dedup_state_basic_dedup() {
        let d = DedupState::new();
        assert!(!d.check_and_record("a")); // first sight → false
        assert!(d.check_and_record("a")); // duplicate → true
        assert!(!d.check_and_record("b"));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn dedup_state_bounded_eviction() {
        // capacity=10: insert 15 distinct ids. After 11th insert, evict
        // the oldest 5 (ids 0..5). Remaining 6 + 4 more inserts = 10.
        let d = DedupState::with_capacity(10);
        for i in 0..15 {
            assert!(!d.check_and_record(&format!("id-{i}")));
        }
        assert!(d.len() <= 10, "len {} must not exceed capacity 10", d.len());

        // id-0 was the oldest, should have been evicted — re-insert
        // returns false (i.e. treated as new, not duplicate).
        assert!(!d.check_and_record("id-0"), "id-0 should have been evicted");
    }

    #[test]
    fn dedup_state_recent_ids_still_dedup() {
        let d = DedupState::with_capacity(100);
        for i in 0..150 {
            assert!(!d.check_and_record(&format!("id-{i}")));
        }
        // Re-record the most recent id — must still be deduped
        assert!(d.check_and_record("id-149"));
    }

    fn max_units(chunks: &[String], unit: LenUnit) -> usize {
        chunks.iter().map(|c| measure(c, unit)).max().unwrap_or(0)
    }

    fn assert_within(chunks: &[String], limit: usize, unit: LenUnit) {
        for c in chunks {
            assert!(
                measure(c, unit) <= limit,
                "chunk exceeded hard limit {limit}: {} units in {c:?}",
                measure(c, unit)
            );
        }
    }

    /// Line-level fence balance: open/close rows of ≥3 `` ` `` (optional ≤3 spaces).
    fn fences_balanced(chunk: &str) -> bool {
        let mut open = 0i32;
        for line in chunk.lines() {
            if let Some(fl) = parse_fence_line(line) {
                if open > 0 && fl.is_closing_candidate && fl.marker == '`' && fl.len >= 3 {
                    open -= 1;
                } else if open == 0 {
                    open += 1;
                }
                // Inside an open body, non-closing fence lines (e.g. info) are content.
            }
        }
        open == 0
    }

    #[test]
    fn split_plain_text_never_exceeds_limit() {
        let msg = "lorem ipsum dolor sit amet ".repeat(50);
        for limit in [20usize, 50, 100, 137] {
            let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
            assert_within(&chunks, limit, LenUnit::Codepoints);
        }
    }

    #[test]
    fn split_code_block_within_limit_and_balanced() {
        // A code block that straddles the limit must be split — each chunk stays
        // within the hard limit AND keeps its fences balanced.
        let limit = 100;
        let code = "let x = 1;\n".repeat(60);
        let msg = format!("intro paragraph here\n```rust\n{code}```\nepilogue text");
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert!(chunks.len() > 1, "expected the code block to be split");
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for c in &chunks {
            assert!(fences_balanced(c), "unbalanced fences in chunk: {c:?}");
        }
        // Code content survives the split (fences/newlines aside).
        let rejoined: String = chunks.iter().map(|c| c.as_str()).collect();
        assert!(rejoined.contains("let x = 1;"));
        // Continuations should reopen with the same info string.
        assert!(
            chunks.iter().filter(|c| c.contains("```rust")).count() >= 1,
            "expected rust info on reopen: {chunks:?}"
        );
    }

    #[test]
    fn split_unterminated_code_block_within_limit() {
        // An unterminated (or very long) code block must never push a chunk past
        // the hard limit, and every chunk must be fence-balanced (force-close).
        let limit = 80;
        let huge = "a".repeat(5000);
        let msg = format!("head\n```\n{huge}"); // never closes
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        assert!(max_units(&chunks, LenUnit::Codepoints) <= limit);
        for c in &chunks {
            assert!(fences_balanced(c), "unbalanced fences in chunk: {c:?}");
        }
    }

    #[test]
    fn split_far_closing_fence_within_limit() {
        let limit = 100;
        let huge = "a".repeat(5000);
        let msg = format!("{}\n```\n{huge}\n```\ntail", "x".repeat(90));
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for c in &chunks {
            assert!(fences_balanced(c), "unbalanced fences in chunk: {c:?}");
        }
    }

    #[test]
    fn split_does_not_emit_empty_code_block() {
        // Prefix nearly fills the limit, then a code block starts. The splitter
        // must break BEFORE the fence rather than opening an empty block at the
        // tail of the first chunk.
        let limit = 100;
        let msg = format!("{}\n```\ncode body here\n```\nafter", "x".repeat(90));
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for c in &chunks {
            assert!(fences_balanced(c), "unbalanced fences: {c:?}");
            // No chunk should contain an empty fenced block.
            let squashed = c.replace(['\n', ' ', '\t'], "");
            assert!(
                !squashed.contains("``````"),
                "empty code block emitted in chunk: {c:?}"
            );
        }
    }

    #[test]
    fn split_keeps_two_fence_blocks_separate() {
        // Regression: cutting near the first block's closing fence must not
        // merge two independent fenced blocks into one open region (QQ swallow).
        let limit = 80;
        let b1 = "A".repeat(40);
        let b2 = "B".repeat(40);
        let msg = format!("pre\n```\n{b1}\n```\n\nmiddle prose\n\n```\n{b2}\n```\npost");
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for c in &chunks {
            assert!(fences_balanced(c), "unbalanced fences: {c:?}");
        }
        // Middle prose must appear outside a fence in some chunk (not only as
        // body between synthetic markers of a single merged block).
        let mut saw_middle_outside = false;
        for c in &chunks {
            if !c.contains("middle prose") {
                continue;
            }
            // Scan line-level: when we see "middle prose", fence depth should be 0.
            let mut open = false;
            for line in c.lines() {
                if let Some(fl) = parse_fence_line(line) {
                    if open && fl.is_closing_candidate {
                        open = false;
                    } else if !open {
                        open = true;
                    }
                    continue;
                }
                if line.contains("middle prose") && !open {
                    saw_middle_outside = true;
                }
            }
        }
        assert!(
            saw_middle_outside,
            "middle prose trapped inside a fence (merged blocks): {chunks:?}"
        );
    }

    #[test]
    fn split_midline_triple_backticks_do_not_toggle() {
        // Prose between blocks may contain ``` mid-line; must not re-pair fences.
        let limit = 60;
        let msg = "```\ncode1\n```\n说明：用```包裹代码\n```\ncode2\n```\n";
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for c in &chunks {
            assert!(fences_balanced(c), "unbalanced fences: {c:?}");
        }
        // code2 should remain fenced content, not swallowed as a third toggle.
        let joined = chunks.join("\n");
        assert!(joined.contains("code2"));
    }

    #[test]
    fn split_short_unclosed_fence_force_closes() {
        let msg = "```\nonly a little";
        let chunks = split_message_chunk(msg, 2000, LenUnit::Codepoints);
        assert_eq!(chunks.len(), 1);
        assert!(fences_balanced(&chunks[0]), "expected force-close: {:?}", chunks[0]);
        assert!(chunks[0].contains("only a little"));
    }

    #[test]
    fn scan_fence_regions_two_blocks() {
        let text = "```\na\n```\n\n```\nb\n```";
        let regs = scan_fence_regions(text, None);
        assert_eq!(regs.len(), 2, "expected two regions: {regs:?}");
        assert!(regs[0].closed && regs[1].closed);
        assert!(regs[0].end <= regs[1].start);
    }

    #[test]
    fn split_table_repeats_header_on_continuation() {
        // A table longer than the limit must be split so every continuation
        // chunk repeats the header + delimiter rows (so it still renders as a
        // table on channels like QQBot that support markdown tables).
        let limit = 120;
        let mut body = String::new();
        for n in 0..40 {
            body.push_str(&format!("| row {n} | value {n} |\n"));
        }
        let msg = format!("| Name | Value |\n| --- | --- |\n{body}");
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert!(chunks.len() > 1, "expected the table to be split");
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.contains("| Name | Value |") && c.lines().any(is_table_delimiter),
                "chunk {i} missing repeated table header/delimiter: {c:?}"
            );
        }
    }

    #[test]
    fn open_table_header_ignores_non_tables() {
        // Plain prose that merely exceeds the limit must not be mistaken for a
        // table.
        let limit = 50;
        let msg = "just a long paragraph of words ".repeat(10);
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for c in &chunks {
            assert!(
                !c.lines().any(is_table_delimiter),
                "spurious delimiter: {c:?}"
            );
        }
    }

    #[test]
    fn split_balances_bold_across_chunks() {
        // A single long bolded run (no paragraph breaks) forces a mid-span cut.
        // Each chunk must keep `**` balanced so QQBot/Wechat render it right.
        let limit = 60;
        let msg = format!("**{}**", "alpha ".repeat(50));
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert!(chunks.len() > 1, "expected the bold run to be split");
        assert_within(&chunks, limit, LenUnit::Codepoints);
        for c in &chunks {
            assert_eq!(
                c.matches("**").count() % 2,
                0,
                "unbalanced bold in chunk: {c:?}"
            );
        }
    }

    #[test]
    fn split_keeps_links_intact() {
        // A cut that would land inside a link must move before it so the link
        // survives whole in one chunk.
        let limit = 80;
        let link = "[click the documentation here](http://example.com/a/very/long/path)";
        let msg = format!(
            "{} {link} and then more trailing text here",
            "word ".repeat(12)
        );
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        assert!(
            chunks.iter().any(|c| c.contains(link)),
            "link was split across chunks: {chunks:?}"
        );
    }

    #[test]
    fn split_utf16_emoji_within_limit() {
        // Emoji cost 2 UTF-16 units each — the splitter must count units, not
        // codepoints, and never exceed the limit.
        let limit = 50;
        let msg = "😀".repeat(100);
        let chunks = split_message_chunk(&msg, limit, LenUnit::Utf16Units);
        assert_within(&chunks, limit, LenUnit::Utf16Units);
    }
}
