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
            supports_file_send: false,
            supports_file_receive: true,
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
            supports_file_send: true,
            supports_file_receive: true,
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
            supports_file_send: false,
            supports_file_receive: false,
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
/// 1. Double newline (paragraph boundary)
/// 2. Single newline (line boundary)
/// 3. Space (word boundary)
/// 4. Hard cut at limit (last resort)
///
/// `limit` is a hard cap: every returned chunk measures `<= limit` units.
/// When a split lands inside a ``` code block, the chunk is given a closing
/// fence and the remainder a reopening fence, so each chunk is independently
/// valid markdown without overflowing the limit. (The fence overhead is
/// budgeted into the cut, so the closed/reopened chunks still fit.)
pub fn split_message_chunk(message: &str, limit: usize, unit: LenUnit) -> Vec<String> {
    if measure(message, unit) <= limit {
        return vec![message.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = message;
    // Set when the previous chunk ended inside an open ``` fence: reopen it.
    let mut carry_open = false;
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
        let block_prefix: String = if carry_open {
            "```\n".to_string()
        } else {
            table_carry.clone().unwrap_or_default()
        };
        let inline_reopen: String = inline_carry.iter().map(|m| m.token()).collect();
        let prefix = format!("{block_prefix}{inline_reopen}");
        let content_limit = limit
            .saturating_sub(measure(&prefix, unit))
            .saturating_sub(inline_reserve);

        if measure(remaining, unit) <= content_limit {
            let mut chunk = String::with_capacity(prefix.len() + remaining.len());
            chunk.push_str(&prefix);
            chunk.push_str(remaining.trim_end());
            chunks.push(chunk);
            break;
        }

        let split = find_split_point(remaining, content_limit, unit, carry_open, &inline_carry);
        let byte_pos = remaining
            .char_indices()
            .nth(split.index)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let (head, rest) = remaining.split_at(byte_pos);

        let mut chunk = String::with_capacity(prefix.len() + head.len() + 8);
        chunk.push_str(&prefix);
        chunk.push_str(head.trim_end());

        carry_open = split.in_code_block;
        if carry_open {
            // Close the fence here; the next chunk reopens it.
            chunk.push_str("\n```");
            table_carry = None;
            inline_carry = Vec::new();
        } else {
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
        remaining = if carry_open {
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

/// Inline markers (`**`, `~~`, single `` ` ``) left open at codepoint index
/// `upto`, in the order opened. `start` is the marker state at the beginning of
/// `chars` (markers carried over from a previous chunk). Markers inside fenced
/// code blocks are ignored; inside an inline-code span only a closing `` ` `` is
/// recognized.
fn open_inline_markers(chars: &[char], upto: usize, start: &[Marker]) -> Vec<Marker> {
    let len = chars.len();
    let upto = upto.min(len);
    let mut stack: Vec<Marker> = start.to_vec();
    let mut in_fenced = false;
    let mut i = 0;
    while i < upto {
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            in_fenced = !in_fenced;
            i += 3;
            continue;
        }
        if in_fenced {
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
    /// Whether the cut lands inside an open ``` fence — so the chunk needs a
    /// closing fence and the remainder a reopening one.
    in_code_block: bool,
    /// Inline markers open at the cut (non-code only), in open order, so the
    /// caller can close them on this chunk and reopen them on the next.
    markers: Vec<Marker>,
}

/// Find the best position to split text, preferring natural boundaries while
/// never exceeding `limit` units. `start_in_block` is the fence state at the
/// start of `text` (true when continuing a code block from a prior chunk).
///
/// When the chosen cut lands inside a code block, room for a closing "\n```"
/// fence is reserved so the caller can append it and still fit within `limit`.
fn find_split_point(
    text: &str,
    limit: usize,
    unit: LenUnit,
    start_in_block: bool,
    start_markers: &[Marker],
) -> SplitPoint {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

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

    // Whether codepoint index `pos` lies inside an open ``` fence.
    let in_block_at = |pos: usize| -> bool {
        let mut open = start_in_block;
        let mut i = 0;
        while i < pos {
            if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
                open = !open;
                i += 3;
                continue;
            }
            i += 1;
        }
        open
    };

    // Codepoint index of the first backtick of the fence currently open at
    // `pos`, or None if `pos` is not inside a block.
    let open_fence_start = |pos: usize| -> Option<usize> {
        let mut open = start_in_block;
        let mut start = if start_in_block { Some(0usize) } else { None };
        let mut i = 0;
        while i < pos {
            if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
                open = !open;
                start = if open { Some(i) } else { None };
                i += 3;
                continue;
            }
            i += 1;
        }
        if open { start } else { None }
    };

    // Best natural boundary within [0, cap]; hard cut at cap as last resort.
    let boundary_within = |cap: usize| -> usize {
        if let Some(pos) = find_last_pattern(&chars[..cap], &['\n', '\n']) {
            pos + 2
        } else if let Some(pos) = find_last_char(&chars[..cap], '\n') {
            pos + 1
        } else if let Some(pos) = find_last_char(&chars[..cap], ' ') {
            pos + 1
        } else {
            cap.max(1)
        }
    };

    let mut index = boundary_within(cap_for(limit));
    let mut in_code_block = in_block_at(index);

    if in_code_block {
        // The chunk will get a closing "\n```" appended — make room for it so
        // chunk content + fence still fits within `limit`.
        let fence_cost = measure("\n```", unit);
        let chunk_cost: usize = chars[..index].iter().map(|&c| char_cost(c, unit)).sum();
        if chunk_cost + fence_cost > limit {
            index = boundary_within(cap_for(limit.saturating_sub(fence_cost)).max(1));
            in_code_block = in_block_at(index);
        }
    }

    if in_code_block {
        // Avoid emitting a degenerate empty code block: if the fence opened at
        // the tail of this chunk with no real body before the cut, split
        // *before* the fence so the whole block moves to the next chunk.
        if let Some(f) = open_fence_start(index) {
            if f > 0 {
                // Body begins after the opening fence's own line (```lang\n).
                let mut b = (f + 3).min(index);
                while b < index && chars[b] != '\n' {
                    b += 1;
                }
                if b < index {
                    b += 1; // skip the newline after the opening fence
                }
                if chars[b..index].iter().all(|c| c.is_whitespace()) {
                    index = f;
                    in_code_block = false;
                }
            }
        }
    }

    let mut markers = Vec::new();
    if !in_code_block {
        // Don't cut inside a [text](url) link — move the cut before it.
        if let Some(a) = link_start_before(&chars, index) {
            if a > 0 {
                index = a;
            }
        }
        markers = open_inline_markers(&chars, index, start_markers);
    }

    SplitPoint {
        index,
        in_code_block,
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

    fn fences_balanced(chunk: &str) -> bool {
        chunk.matches("```").count() % 2 == 0
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
    }

    #[test]
    fn split_unterminated_code_block_within_limit() {
        // An unterminated (or very long) code block must never push a chunk past
        // the hard limit.
        let limit = 80;
        let huge = "a".repeat(5000);
        let msg = format!("head\n```\n{huge}"); // never closes
        let chunks = split_message_chunk(&msg, limit, LenUnit::Codepoints);
        assert_within(&chunks, limit, LenUnit::Codepoints);
        assert!(max_units(&chunks, LenUnit::Codepoints) <= limit);
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
