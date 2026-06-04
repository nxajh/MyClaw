//! Token estimation + tracking utilities.
//!
//! Extracted from `agent_impl::types` so that the types Session and
//! ContextEngine depend on (TokenTracker, estimate_*, is_write_tool)
//! survive the H45 deletion of `agent_impl`.

use crate::providers::{ChatMessage, ContentPart};

/// Estimate token count from text length (~4 bytes per token).
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Estimate token count for a `ChatMessage`.
pub fn estimate_message_tokens(msg: &ChatMessage) -> u64 {
    let mut tokens = 4u64; // metadata overhead
    for part in &msg.parts {
        tokens += match part {
            ContentPart::Text { text } => estimate_tokens(text),
            ContentPart::ImageUrl { .. } => 800,
            ContentPart::ImageB64 { .. } => 800,
            // ImageRef is a disk-only placeholder; if encountered it represents
            // an (un-hydrated) image — charge the same flat cost as an image.
            ContentPart::ImageRef { .. } => 800,
            // Audio is adapted to text before a model sees it; if a raw audio
            // part is still present, charge a flat cost like an image.
            ContentPart::AudioB64 { .. } | ContentPart::AudioRef { .. } => 800,
            ContentPart::Thinking { thinking, .. } => estimate_tokens(thinking),
        };
    }
    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            tokens += estimate_tokens(&tc.id)
                + estimate_tokens(&tc.name)
                + estimate_tokens(&tc.arguments)
                + 8;
        }
    }
    if let Some(ref tcid) = msg.tool_call_id {
        tokens += estimate_tokens(tcid) + 4;
    }
    tokens
}

/// Estimate the total prompt tokens for a system prompt + full history — i.e.
/// the size of what an LLM request actually carries. Used both to seed/reconcile
/// the [`TokenTracker`] and as a tracker-independent pre-send compaction guard.
pub fn estimate_history_tokens(system_prompt: &str, history: &[ChatMessage]) -> u64 {
    let mut total = 0u64;
    if !system_prompt.is_empty() {
        total += estimate_tokens(system_prompt) + 4;
    }
    for msg in history {
        total += estimate_message_tokens(msg);
    }
    total
}

/// Token usage tracker — combines precise API-reported usage with
/// estimated pending tokens. Lives on `Session.token_tracker`.
#[derive(Debug, Clone, Default)]
pub struct TokenTracker {
    last_input_tokens: u64,
    last_cached_tokens: u64,
    last_output_tokens: u64,
    pending_estimated_tokens: u64,
}

impl TokenTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with precise usage from API response. Resets pending estimates.
    pub fn update_from_usage(&mut self, input_tokens: u64, output_tokens: u64, cached_tokens: u64) {
        self.last_input_tokens = input_tokens;
        self.last_output_tokens = output_tokens;
        self.last_cached_tokens = cached_tokens;
        self.pending_estimated_tokens = 0;
    }

    pub fn record_pending(&mut self, tokens: u64) {
        self.pending_estimated_tokens += tokens;
    }

    /// Seed the tracker by estimating tokens for the system prompt + every
    /// message in the history. Used at turn start when no prior API usage
    /// has been recorded yet (fresh session or post-restart).
    pub fn seed_from_history(&mut self, system_prompt: &str, history: &[ChatMessage]) {
        self.record_pending(estimate_history_tokens(system_prompt, history));
    }

    pub fn total_tokens(&self) -> u64 {
        self.last_input_tokens
            .saturating_add(self.last_cached_tokens)
            .saturating_add(self.last_output_tokens)
            .saturating_add(self.pending_estimated_tokens)
    }

    pub fn is_fresh(&self) -> bool {
        self.last_input_tokens == 0
            && self.last_cached_tokens == 0
            && self.pending_estimated_tokens == 0
    }

    pub fn last_input(&self) -> u64 { self.last_input_tokens }
    pub fn last_cached(&self) -> u64 { self.last_cached_tokens }
    pub fn last_output(&self) -> u64 { self.last_output_tokens }

    pub fn adjust_for_compaction(&mut self, removed_tokens: u64, added_tokens: u64) {
        let net_reduction = removed_tokens.saturating_sub(added_tokens);
        let from_pending = net_reduction.min(self.pending_estimated_tokens);
        self.pending_estimated_tokens -= from_pending;
        self.last_input_tokens = self
            .last_input_tokens
            .saturating_sub(net_reduction - from_pending);
    }
}

/// True for tools that can mutate system state and are therefore blocked
/// in `PermissionMode::ReadOnly`.
pub fn is_write_tool(name: &str) -> bool {
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

#[cfg(test)]
mod tracker_tests {
    use super::*;

    #[test]
    fn estimate_history_tokens_counts_system_and_messages() {
        let history = vec![
            ChatMessage::user_text("hello there"),
            ChatMessage::assistant_text("hi"),
        ];
        let est = estimate_history_tokens("you are a bot", &history);
        // Strictly greater than the system prompt alone + nonzero per message.
        assert!(est > estimate_tokens("you are a bot"));
    }
}
