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
use tokio::sync::mpsc;
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

// ── ChannelInboundMessage + Channel trait (moved from channels/message.rs, #151 Phase 3b) ──

/// A runtime message received from a channel.
#[derive(Debug, Clone)]
pub struct ChannelInboundMessage {
    pub id: String,
    pub sender: MessageSender,
    pub receiver: MessageReceiver,
    pub content: ChannelMessageContent,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
    /// 方案 C (RFC §3.3, race fix 2026-08-10): wake-time silence intent for
    /// synthesized delegation notices. `Some(true)` = intermediate notice
    /// (pending tasks remained when the terminal event was collected →
    /// silenced turn → the model output is delivered as an ordinary
    /// intermediate message and the turn does NOT end), `Some(false)` = final
    /// notice (pending empty → loud summary). `None` for real user messages /
    /// scheduled turns → `process_turn` falls back to the live suspension
    /// snapshot at turn start. Runtime-only; never persisted (see
    /// `to_persisted`).
    pub silenced_override: Option<bool>,
    /// RFC channel-role-split §1.1: turn-scoped "is there a human user
    /// present?" marker. `Interactive` (default) for user messages, daemon
    /// recovery synthetic messages and delegation-wake notices; `Background`
    /// for cron/webhook synthesized turns (scheduled.rs). Drives
    /// `Session::turn_headless` + `prompt_config.run_mode` inside
    /// `process_turn` — NOT a delivery handle (that's the channel registry).
    /// Runtime-only; never persisted.
    pub run_mode: crate::api::run_mode::RunMode,
}

impl Default for ChannelInboundMessage {
    fn default() -> Self {
        Self {
            id: String::new(),
            sender: MessageSender::new(String::new()),
            receiver: MessageReceiver::new(String::new()),
            content: ChannelMessageContent::text(String::new()),
            timestamp: 0,
            interruption_scope_id: None,
            silenced_override: None,
            run_mode: crate::api::run_mode::RunMode::default(),
        }
    }
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

impl PersistedChannelMessage {
    /// Reverse of `to_persisted`: rebuild a runtime message for inbound-spool
    /// replay (RFC inbound-spool §6.4). File bodies are gone (never spooled);
    /// `silenced_override` is `None` — replayed messages are ordinary user
    /// messages, never synthesized delegation notices.
    pub fn into_runtime(&self) -> ChannelInboundMessage {
        ChannelInboundMessage {
            id: self.id.clone(),
            sender: MessageSender::new(self.sender_id.clone()),
            receiver: self.receiver.clone(),
            content: ChannelMessageContent::text(self.text.clone()),
            timestamp: self.timestamp,
            interruption_scope_id: self.interruption_scope_id.clone(),
            silenced_override: None,
            run_mode: crate::api::run_mode::RunMode::default(),
        }
    }
}

/// Marker trait for channel adapters.
#[async_trait]
pub trait Channel: crate::api::message::OutboundChannel + Send + Sync {
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

    /// Notify the channel about tool call lifecycle events.
    /// Default implementation does nothing — channels can override to show
    /// per-tool progress (e.g. WeChat reply progress).
    async fn on_tool_event(&self, _recipient: &str, _event: ToolEvent) {}

    /// Declarative capabilities (RFC §6.1). Default points at
    /// `MINIMAL_CAPABILITIES`; each channel overrides to publish its own
    /// `&'static ChannelCapabilities`.
    fn capabilities(&self) -> &ChannelCapabilities {
        &MINIMAL_CAPABILITIES
    }

    /// Whether auto-TTS is enabled for this channel instance (per-account
    /// `tts` config flag, default off). `SessionContext::process_turn`
    /// consults this together with the global `[agent] auto_tts` master
    /// switch before synthesizing a reply to voice.
    fn tts_enabled(&self) -> bool {
        false
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
    fn create_stream(&self, _reply_target: &str) -> Option<Box<dyn crate::api::turn_stream::TurnStream>> {
        None
    }

    /// 单 preview (2026-08-12): like `create_stream`, but the returned
    /// stream may TAKE OVER an existing preview message (cross-turn fold for
    /// async-delegation continuation — the whole suspension flow is one
    /// evolving message). `fold` carries the platform message id + last
    /// body; channels without fold support ignore it. Default: plain
    /// `create_stream`.
    fn create_stream_folding(
        &self,
        reply_target: &str,
        _fold: Option<crate::api::turn_stream::FoldCandidate>,
    ) -> Option<Box<dyn crate::api::turn_stream::TurnStream>> {
        let _ = _fold;
        self.create_stream(reply_target)
    }

    /// Authorization policy snapshot for this channel (RFC §14).
    /// Default: open policy — used by Client (connection-level token authn).
    /// Hot-reload-capable channels read through their internal RwLock and
    /// return a cloned snapshot.
    fn security_policy(&self) -> crate::api::security::ChannelSecurityPolicy {
        crate::api::security::ChannelSecurityPolicy::open()
    }

    /// Decide whether an inbound message is authorized. Channels call this
    /// in their listen/poll loop before forwarding to the orchestrator.
    /// Default implementation delegates to `security_policy()`.
    fn check_authorization(
        &self,
        sender: &str,
        scope: crate::api::security::MessageScope<'_>,
    ) -> crate::api::security::AuthDecision {
        crate::api::security::evaluate(&self.security_policy(), sender, scope)
    }

    /// Group statistics for the `/groups` slash command.
    /// Only group-capable channels (e.g. QQBot) override this.
    /// Default: empty vec.
    fn group_stats(&self) -> Vec<GroupStat> {
        vec![]
    }
}

// ── CallbackAction (moved from channels/message.rs, #151 Phase 3b) ──

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

// ── Channel capabilities (moved from channels/message.rs, #151 Phase 3c; RFC §6.1) ──

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
            supports_streaming: true,
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

// ── Callback / stats types (moved from channels/message.rs, #151 Phase 3c) ──

/// RFC §11 Phase 5 callback action.
pub enum ProcessingStatus {
    /// LLM call started — the bot is "thinking".
    Thinking,
    /// Response sent successfully (status cleanup already handled in send()).
    Done,
    /// An error occurred during processing.
    Error,
}

/// Tool lifecycle event for channels that show per-tool progress.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    /// A tool call is about to start.
    Start {
        tool_name: String,
        tool_call_id: String,
    },
    /// A tool call finished (success or failure).
    End {
        tool_name: String,
        tool_call_id: String,
        success: bool,
    },
}

/// Per-group statistics surfaced by the `/groups` command.
#[derive(Debug, Clone)]
pub struct GroupStat {
    pub group_id: String,
    pub name: Option<String>,
    pub buffered_messages: usize,
    pub history_limit: usize,
}
