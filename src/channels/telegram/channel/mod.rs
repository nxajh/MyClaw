//! TelegramChannel — the main bot adapter for the Telegram Bot API.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::channels::shared::{InboundDebouncer, TypingKeepAlive};
use crate::DedupState;

mod api;
mod channel;
mod session;

#[cfg(test)]
pub(crate) mod tests;

// ── TelegramChannel ────────────────────────────────────────────────────────────

/// Reaction tracker: reply_target → Vec<(chat_id, message_id)>.
type ReactionTracker = Arc<Mutex<std::collections::HashMap<String, Vec<(i64, i64)>>>>;

#[derive(Clone)]
pub struct TelegramChannel {
    bot_token: String,
    /// Normalized DM whitelist. Plain `Vec<String>` (not Arc<RwLock>)
    /// because MyClaw applies config changes via `myclaw reload` → SIGUSR1
    /// → hot_switch full-process restart, not in-process mutation. New
    /// process = fresh struct = no writer ever exists in this process.
    allowed_users: Vec<String>,
    /// Phase 4: allowed group chat IDs (RFC §14.5). `None` = reject all
    /// groups (Phase 4 default); `Some(vec ["*"])` = allow all groups.
    allowed_groups: Option<Vec<String>>,
    mention_only: bool,
    api_base: String,
    dedup: DedupState,
    /// Username of this bot (fetched lazily). Wrapped in Arc for Clone.
    bot_username: Arc<Mutex<Option<String>>>,
    /// Workspace directory for saving attachments.
    workspace_dir: Option<std::path::PathBuf>,
    /// Active typing keep-alive tasks, keyed by recipient (chat_id).
    typing: TypingKeepAlive,
    /// Whether to send acknowledgement reactions on received messages.
    ack_reactions: bool,
    /// Track ack reactions: reply_target → (chat_id, message_id) for removal after reply.
    pending_acks: ReactionTracker,
    /// Status reactions: reply_target → Vec<(chat_id, msg_id)>.
    status_reactions: ReactionTracker,
    /// Debounce window in milliseconds (0 = disabled) + merge buffer
    /// ("sender|reply_target" key).
    debouncer: InboundDebouncer,
    /// Stall watchdog timeout in seconds (0 = disabled).
    stall_timeout_secs: u64,
    /// Track when typing started for each recipient: reply_target → Instant.
    typing_started_at: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    /// Stall watchdog messages to delete when real reply arrives: reply_target → [(chat_id, msg_id)].
    stall_messages: ReactionTracker,
    /// Streaming preview mode for this channel.
    streaming_mode: crate::config::channel::StreamingMode,
    /// Per-account auto-TTS switch (default off).
    tts: bool,
    /// Targets with active streams; stall watchdog skips these to avoid
    /// redundant "still thinking" messages alongside the live preview.
    pub(crate) streaming_targets: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Directory for persisting state (e.g. Telegram update offset).
    base_dir: std::path::PathBuf,
    /// Shared HTTP client with connection pool.
    http: reqwest::Client,
    /// Lightweight ring buffer of recent sent messages (max 100 entries) for
    /// debugging and potential reply-chain context.
    message_cache: Arc<Mutex<VecDeque<(i64, String)>>>,
}
