//! Session types — SummaryMetadata and Session struct.

use std::sync::Arc;

use crate::agents::tokens::TokenTracker;
use crate::channels::{Channel, ChannelMessage};
use crate::providers::capability_chat::ChatMessage;
use super::backend::PersistHook;
use super::session_override::SessionOverride;

/// Summary metadata stored in Session memory (no text parsing needed).
#[derive(Debug, Clone)]
pub struct SummaryMetadata {
    pub version: u32,
    pub token_estimate: u64,
    pub up_to_message: i64,
}

/// Per-session conversation state.
///
/// Per RFC v2 §三.A the Session holds:
/// - The incremental conversation history and its persistence-bookkeeping.
/// - The last incoming ChannelMessage so retry / recovery / ask_user replies
///   land back on the same channel + reply_target.
/// - Optional `parent_session_id` for sub-sessions spawned by `agent_delegate`.
/// - `agent_name` identifying which agent in `workspace/agents/` owns this
///   session (defaults to "main").
/// - Token usage tracker — moved from CompactionPolicy so the session owns
///   its own context budget (C18 will rewire CompactionPolicy/ContextEngine
///   to read through `&Session.token_tracker`).
/// - Transient persist / channel handles — `Option<Arc<dyn …>>` so they
///   survive `Clone` cheaply, default to `None` for tests and ephemeral
///   sessions.
#[derive(Clone)]
pub struct Session {
    /// Session ID (e.g. "k3jr9px2").
    pub id: String,
    /// Owner routing key (e.g. "telegram:default:12345").
    /// RFC v2 renames "owner" semantically to `routing_key`; the field stays
    /// named `owner` for source-diff churn reasons.
    pub owner: String,
    /// Agent name that owns this session. References `workspace/agents/{name}/AGENT.md`.
    /// Defaults to "main"; sub-sessions inherit their delegating agent's name.
    pub agent_name: String,
    /// Parent session ID for sub-sessions spawned by `agent_delegate`.
    /// `None` for top-level user sessions.
    pub parent_session_id: Option<String>,
    /// Current conversation history (in-memory).
    pub history: Vec<ChatMessage>,
    /// Parallel to `history`: database message IDs, 0 for summary or unpersisted messages.
    pub message_ids: Vec<i64>,
    /// Monotonic compaction version counter.
    pub compact_version: u32,
    /// In-memory summary metadata (restored from backend on load).
    pub summary_metadata: Option<SummaryMetadata>,
    /// Per-session runtime overrides set by slash commands.
    pub session_override: SessionOverride,
    /// Set when the last persisted turn ended with a user message but no
    /// corresponding assistant response (e.g. daemon crash/SIGKILL). The
    /// orchestrator will prompt the user to retry or abort on the next
    /// interaction. Not persisted — rebuilt on every session load.
    pub incomplete_turn: bool,
    /// Last incoming ChannelMessage. Carries sender, reply_target, attachments,
    /// images. Persisted so startup recovery can reconstruct the routing
    /// context and resume an interrupted turn. RFC v2 §三.A replaces the old
    /// `last_reply_target: Option<String>` field with this richer message.
    pub last_message: Option<ChannelMessage>,
    /// Token usage tracker. Owned by the session so `Agent.run` /
    /// `ContextEngine` can read budgets without needing a parallel struct.
    /// Seeded by `SessionManager` from `backend.load_token_count` on
    /// session reload; updated from API `Usage` events thereafter.
    pub token_tracker: TokenTracker,
    /// Transient persistence hook installed by the Orchestrator at session
    /// load time. `None` for tests and the in-memory CLI mode. C18's
    /// `Agent.run` reaches through this to persist per-turn state.
    pub persist: Option<Arc<dyn PersistHook>>,
    /// Transient channel handle installed by the Orchestrator. `None` for
    /// sub-sessions that piggyback on the parent, and for tests.
    /// `Agent.run` uses `channel.push_event(reply_target, …)` to stream
    /// turn events to the originating UI.
    pub channel: Option<Arc<dyn Channel>>,
}

// `Arc<dyn PersistHook>` and `Arc<dyn Channel>` don't carry `Debug`, so a
// derived Debug fails. Hand-roll one that elides the transient handles.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("owner", &self.owner)
            .field("agent_name", &self.agent_name)
            .field("parent_session_id", &self.parent_session_id)
            .field("history_len", &self.history.len())
            .field("compact_version", &self.compact_version)
            .field("incomplete_turn", &self.incomplete_turn)
            .field("has_last_message", &self.last_message.is_some())
            .field("has_persist", &self.persist.is_some())
            .field("has_channel", &self.channel.is_some())
            .finish()
    }
}

impl Session {
    pub fn new(id: String) -> Self {
        Self {
            owner: String::new(),
            id,
            agent_name: "main".to_string(),
            parent_session_id: None,
            history: Vec::new(),
            message_ids: Vec::new(),
            compact_version: 0,
            summary_metadata: None,
            session_override: SessionOverride::default(),
            incomplete_turn: false,
            last_message: None,
            token_tracker: TokenTracker::new(),
            persist: None,
            channel: None,
        }
    }

    /// Install a persistence hook on this session. Returns `self` for chaining.
    pub fn with_persist(mut self, persist: Arc<dyn PersistHook>) -> Self {
        self.persist = Some(persist);
        self
    }

    /// Install a channel handle on this session. Returns `self` for chaining.
    pub fn with_channel(mut self, channel: Arc<dyn Channel>) -> Self {
        self.channel = Some(channel);
        self
    }

    /// Persist the session's per-turn metadata to disk via the installed
    /// `PersistHook`. Currently flushes `last_message.reply_target`, the
    /// session override JSON, and the most recent total token count. Per-
    /// message persistence is handled inline by `Agent.run`; this is the
    /// "after-turn settle" call for snapshot fields.
    ///
    /// No-op when no persist hook is installed.
    pub fn save_to_disk(&self) {
        let hook = match self.persist.as_ref() {
            Some(h) => h,
            None => return,
        };
        if let Some(ref msg) = self.last_message {
            hook.save_last_message(&self.id, msg);
        }
        if let Ok(s) = serde_json::to_string(&self.session_override) {
            hook.save_session_override(&self.id, &s);
        }
        let total = self.token_tracker.total_tokens();
        if total > 0 {
            hook.save_token_count(&self.id, total);
        }
    }

    /// Return the routing target for the next outbound message, derived from
    /// the most recently stored ChannelMessage.
    pub fn reply_target(&self) -> Option<&str> {
        self.last_message.as_ref().map(|m| m.reply_target.as_str())
    }

    /// Store the incoming message context.
    pub fn record_inbound(&mut self, msg: ChannelMessage) {
        self.last_message = Some(msg);
    }

    /// Append a user message to history.
    pub fn add_user(&mut self, text: String) {
        self.history.push(ChatMessage::user_text(text));
        self.message_ids.push(0);
    }

    /// Deprecated alias for [`Session::add_user`]. Kept so 40+ existing call
    /// sites compile during the C18 migration window; new code should call
    /// `add_user`.
    #[deprecated(note = "use Session::add_user")]
    pub fn add_user_text(&mut self, text: String) {
        self.add_user(text);
    }

    /// Append an assistant text message to history.
    /// Skips empty messages to avoid API format errors on reload.
    pub fn add_assistant(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history.push(ChatMessage::assistant_text(text));
        self.message_ids.push(0);
    }

    /// Deprecated alias for [`Session::add_assistant`]. Kept so existing
    /// call sites compile during the C18 migration window.
    #[deprecated(note = "use Session::add_assistant")]
    pub fn add_assistant_text(&mut self, text: String) {
        self.add_assistant(text);
    }

    /// Append an assistant message with tool_calls to history.
    /// Skips only when text is empty AND there are no tool_calls.
    pub fn add_assistant_with_tools(
        &mut self,
        text: String,
        tool_calls: Vec<crate::providers::ToolCall>,
        thinking: Option<String>,
        thinking_signature: Option<String>,
    ) {
        if text.trim().is_empty() && tool_calls.is_empty() {
            return;
        }
        let mut msg = ChatMessage::assistant_text(&text);
        msg.tool_calls = Some(tool_calls);
        if let Some(thinking) = thinking {
            use crate::providers::ContentPart;
            msg.parts.insert(0, ContentPart::Thinking { thinking, signature: thinking_signature });
        }
        self.history.push(msg);
        self.message_ids.push(0);
    }

    /// Append a tool result message to history.
    pub fn add_tool_result(&mut self, tool_call_id: String, content: String, is_error: bool) {
        let mut msg = ChatMessage::text("tool", &content);
        msg.tool_call_id = Some(tool_call_id);
        msg.is_error = Some(is_error);
        self.history.push(msg);
        self.message_ids.push(0);
    }

    /// Add a system message to history.
    pub fn add_system_text(&mut self, text: String) {
        self.history.push(ChatMessage::system_text(text));
        self.message_ids.push(0);
    }

    /// Remove the last assistant message (used when a loop break occurs).
    pub fn pop_last_assistant(&mut self) {
        if let Some(msg) = self.history.last() {
            if msg.role == "assistant" {
                self.history.pop();
                self.message_ids.pop();
            }
        }
    }

    /// Roll back history to the given length.
    /// Removes all messages added after position `len` (both in-memory history and message_ids).
    /// Used when a turn fails completely (e.g. empty LLM response) to undo all
    /// messages added during that turn (user + assistant/tool_calls/tool_results).
    pub fn rollback_to(&mut self, len: usize) {
        self.history.truncate(len);
        self.message_ids.truncate(len);
    }

    /// Replace history[compact_start..compact_end] with a single summary message.
    /// Updates compact_version and summary_metadata atomically.
    pub(crate) fn apply_compaction(
        &mut self,
        compact_start: usize,
        compact_end: usize,
        summary_msg: ChatMessage,
        version: u32,
        up_to_message: i64,
        summary_tokens: u64,
    ) {
        self.history.drain(compact_start..compact_end);
        self.history.insert(compact_start, summary_msg);
        self.message_ids.drain(compact_start..compact_end);
        self.message_ids.insert(compact_start, 0);
        self.compact_version = version;
        self.summary_metadata = Some(SummaryMetadata {
            version,
            token_estimate: summary_tokens,
            up_to_message,
        });
    }

    /// Drop history[..boundary] with no summary (fallback when summarizer fails).
    /// Bumps compact_version so the backend can record the event.
    #[allow(dead_code)] // restored summarizer-failure fallback path
    pub(crate) fn drop_pre_boundary(&mut self, boundary: usize, version: u32) {
        self.history.drain(..boundary);
        self.message_ids.drain(..boundary);
        self.compact_version = version;
    }
}
