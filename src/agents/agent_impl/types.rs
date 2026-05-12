use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::providers::{ChatMessage, ChatUsage, ContentPart, StopReason, ToolCall};
use super::super::TurnEvent;

// ── StreamMode ──────────────────────────────────────────────────────────────

/// Determines how the LLM stream is consumed inside `chat_loop`.
///
/// - `Collect`: silently collect into a `CollectedResponse` (existing `run()` behavior).
/// - `Streamed`: forward events via mpsc + support cancellation (for `run_streamed()`).
#[derive(Clone)]
pub(crate) enum StreamMode {
    Collect,
    Streamed {
        event_tx: mpsc::Sender<TurnEvent>,
        cancel: CancellationToken,
    },
}

/// Response collected from a chat stream.
pub(crate) struct CollectedResponse {
    pub(crate) text: String,
    pub(crate) reasoning_content: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    pub(crate) stop_reason: StopReason,
    pub(crate) usage: Option<ChatUsage>,
}

/// Estimate token count from text length (~4 bytes per token).
pub(crate) fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Estimate token count for a ChatMessage.
pub(crate) fn estimate_message_tokens(msg: &ChatMessage) -> u64 {
    let mut tokens = 4u64; // metadata overhead
    for part in &msg.parts {
        tokens += match part {
            ContentPart::Text { text } => estimate_tokens(text),
            ContentPart::ImageUrl { .. } => 800,
            ContentPart::ImageB64 { .. } => 800,
            ContentPart::Thinking { thinking } => estimate_tokens(thinking),
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

/// Token usage tracker — combines precise API-reported usage with estimated pending tokens.
#[derive(Debug, Clone, Default)]
pub(crate) struct TokenTracker {
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

/// Returns true for tools that can mutate system state and are therefore
/// blocked in `AutonomyLevel::ReadOnly` mode.
pub(crate) fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "shell"
            | "file_write"
            | "file_edit"
            | "file_delete"
            | "agent_delegate"
            | "agent_kill"
            | "http"
    )
}

// ── Extension trait for ChatMessage ──────────────────────────────────────────

/// Extension methods for ChatMessage.
#[allow(dead_code)]
pub(super) trait ChatMessageExt {
    fn with_name(self, name: String) -> ChatMessage;
}

impl ChatMessageExt for ChatMessage {
    fn with_name(self, name: String) -> ChatMessage {
        ChatMessage {
            role: self.role,
            parts: self.parts,
            name: Some(name),
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }
    }
}
