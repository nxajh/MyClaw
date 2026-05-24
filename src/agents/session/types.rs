//! Session types — SummaryMetadata, TokenTracker, and Session struct.

use std::sync::Arc;

use crate::channels::{Channel, ChannelMessage};
use crate::providers::capability_chat::ChatMessage;
use crate::providers::ContentPart;
use super::session_override::SessionOverride;
use super::recovery::BreakpointItem;
use super::backend::PersistHook;

/// Summary metadata stored in Session memory (no text parsing needed).
#[derive(Debug, Clone)]
pub struct SummaryMetadata {
    pub version: u32,
    pub token_estimate: u64,
    pub up_to_message: i64,
}

/// Token usage tracker — combines precise API-reported usage with estimated pending tokens.
#[derive(Debug, Clone, Default)]
pub struct TokenTracker {
    /// Last API response's input_tokens (new, non-cached).
    last_input_tokens: u64,
    /// Last API response's cached_input_tokens.
    last_cached_tokens: u64,
    /// Last API response's output_tokens.
    last_output_tokens: u64,
    /// Estimated tokens of items added to history after the last API response.
    pending_estimated_tokens: u64,
}

impl TokenTracker {
    /// Update with precise usage from API response. Resets pending estimates.
    /// `input_tokens` = new (non-cached) tokens, `cached_tokens` = cache-hit tokens.
    pub fn update_from_usage(&mut self, input_tokens: u64, output_tokens: u64, cached_tokens: u64) {
        self.last_input_tokens = input_tokens;
        self.last_output_tokens = output_tokens;
        self.last_cached_tokens = cached_tokens;
        self.pending_estimated_tokens = 0;
    }

    /// Record estimated tokens for a new item added to history.
    pub fn record_pending(&mut self, tokens: u64) {
        self.pending_estimated_tokens += tokens;
    }

    /// Total context tokens (input + cached + output now in history + pending).
    pub fn total_tokens(&self) -> u64 {
        self.last_input_tokens
            .saturating_add(self.last_cached_tokens)
            .saturating_add(self.last_output_tokens)
            .saturating_add(self.pending_estimated_tokens)
    }

    /// Returns true if the tracker has never been updated (fresh session or recovery).
    pub fn is_fresh(&self) -> bool {
        self.last_input_tokens == 0
            && self.last_cached_tokens == 0
            && self.pending_estimated_tokens == 0
    }

    /// Last input tokens (new, non-cached).
    pub fn last_input(&self) -> u64 { self.last_input_tokens }

    /// Last cached input tokens.
    pub fn last_cached(&self) -> u64 { self.last_cached_tokens }

    /// Last output tokens.
    pub fn last_output(&self) -> u64 { self.last_output_tokens }

    /// Adjust tracker after compaction: deduct removed tokens, add summary tokens.
    /// Preserves output_tokens and only touches input/pending estimates.
    pub fn adjust_for_compaction(&mut self, removed_tokens: u64, added_tokens: u64) {
        let net_reduction = removed_tokens.saturating_sub(added_tokens);
        // Deduct from pending first, then from input.
        let from_pending = net_reduction.min(self.pending_estimated_tokens);
        self.pending_estimated_tokens -= from_pending;
        self.last_input_tokens = self.last_input_tokens
            .saturating_sub(net_reduction - from_pending);
    }
}

/// Estimate token count from text length (~4 bytes per token).
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Estimate token count for a ChatMessage.
pub fn estimate_message_tokens(msg: &ChatMessage) -> u64 {
    let mut tokens = 4u64; // metadata overhead
    for part in &msg.parts {
        tokens += match part {
            ContentPart::Text { text } => estimate_tokens(text),
            ContentPart::ImageUrl { .. } => 800,
            ContentPart::ImageB64 { .. } => 800,
            ContentPart::Thinking { thinking, .. } => estimate_tokens(thinking),
        };
    }
    // Estimate tool_calls overhead (id + name + arguments).
    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            tokens += estimate_tokens(&tc.id) + estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments) + 8;
        }
    }
    // tool_call_id on tool result messages.
    if let Some(ref tcid) = msg.tool_call_id {
        tokens += estimate_tokens(tcid) + 4;
    }
    tokens
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
    /// Last total token count reported by the API (input + cached + output).
    /// Loaded from meta.json on session restore; None for brand-new sessions.
    pub last_total_tokens: Option<u64>,
    /// Per-session runtime overrides set by slash commands.
    pub session_override: SessionOverride,
    /// Set when the last persisted turn ended with a user message but no
    /// corresponding assistant response (e.g. daemon crash/SIGKILL). The
    /// orchestrator will prompt the user to retry or abort on the next
    /// interaction. Not persisted — rebuilt on every session load.
    pub incomplete_turn: bool,
    /// Tool calls that were pending when the session was interrupted
    /// (assistant emitted tool_calls but no tool results were persisted).
    /// Detected on session load; used by the orchestrator to inject a
    /// recovery prompt so the model can re-execute the missing tools.
    pub breakpoint_items: Vec<BreakpointItem>,
    /// Last incoming ChannelMessage. Carries sender, reply_target, attachments,
    /// images. Persisted so startup recovery can reconstruct the routing
    /// context and resume an interrupted turn. RFC v2 §三.A replaces the old
    /// `last_reply_target: Option<String>` field with this richer message.
    pub last_message: Option<ChannelMessage>,
    /// Token usage tracker combining API-reported usage with pending estimates.
    pub token_tracker: TokenTracker,
    /// Transient persist hook. Set by the orchestrator, not persisted.
    pub persist: Option<Arc<dyn PersistHook>>,
    /// Transient channel reference for push_event/cancel_signal. Not persisted.
    pub channel: Option<Arc<dyn Channel>>,
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
            last_total_tokens: None,
            session_override: SessionOverride::default(),
            incomplete_turn: false,
            breakpoint_items: Vec::new(),
            last_message: None,
            token_tracker: TokenTracker::default(),
            persist: None,
            channel: None,
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

    /// Append an assistant text message to history.
    /// Skips empty messages to avoid API format errors on reload.
    pub fn add_assistant(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history.push(ChatMessage::assistant_text(text));
        self.message_ids.push(0);
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
    pub(crate) fn drop_pre_boundary(&mut self, boundary: usize, version: u32) {
        self.history.drain(..boundary);
        self.message_ids.drain(..boundary);
        self.compact_version = version;
    }

    /// Persist session state to disk via the transient persist hook.
    pub fn save_to_disk(&self) {
        if let Some(ref persist) = self.persist {
            if let Some(ref msg) = self.last_message {
                persist.save_reply_target(&self.id, msg.reply_target.as_str());
            }
            persist.save_token_count(&self.id, self.last_total_tokens.unwrap_or(0));
        }
    }

    // ── Deprecated aliases ──────────────────

    /// Append a user message to history.
    #[deprecated(note = "use add_user instead")]
    pub fn add_user_text(&mut self, text: String) {
        self.add_user(text);
    }

    /// Append an assistant text message to history.
    #[deprecated(note = "use add_assistant instead")]
    pub fn add_assistant_text(&mut self, text: String) {
        self.add_assistant(text);
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("owner", &self.owner)
            .field("agent_name", &self.agent_name)
            .field("parent_session_id", &self.parent_session_id)
            .field("history_len", &self.history.len())
            .field("compact_version", &self.compact_version)
            .field("last_total_tokens", &self.last_total_tokens)
            .field("incomplete_turn", &self.incomplete_turn)
            .field("has_last_message", &self.last_message.is_some())
            .field("token_tracker", &self.token_tracker)
            .field("persist", &self.persist.is_some())
            .field("channel", &self.channel.is_some())
            .finish()
    }
}
