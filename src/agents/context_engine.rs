//! Pluggable context engine interface.
//!
//! Controls how conversation context is managed when approaching the model's
//! token limit. The built-in `DefaultContextEngine` is the default implementation.
//! Third-party engines can replace it via the config system.

use crate::providers::{ChatMessage, ChatUsage};

/// Token usage statistics tracked by a context engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenStats {
    pub last_prompt_tokens: u64,
    pub last_completion_tokens: u64,
    pub last_total_tokens: u64,
    pub threshold_tokens: u64,
    pub context_length: u64,
    pub compression_count: u64,
}

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub removed_messages: usize,
    pub summary_tokens: u64,
    pub removed_tokens: u64,
    pub version: u32,
}

/// Abstract base class for pluggable context engines.
///
/// The engine is responsible for:
/// - Deciding when compaction should fire (`should_compress`)
/// - Performing compaction (`compact`)
/// - Tracking token usage from API responses (`update_usage`)
pub trait ContextEngine: Send + Sync {
    /// Update tracked token usage from an API response.
    fn update_usage(&mut self, usage: &ChatUsage);

    /// Return true if compaction should fire now.
    fn should_compress(&self, total_tokens: u64, context_window: u64, compact_threshold: f64) -> bool;

    /// Compact history so that the compressible prefix fits within target_window.
    ///
    /// Returns the compaction result, or None if no compaction was needed.
    fn compact(
        &mut self,
        history: &mut Vec<ChatMessage>,
        system_prompt: &str,
        target_window: u64,
        compact_threshold: f64,
    ) -> anyhow::Result<Option<CompactionResult>>;

    /// Current token statistics.
    fn token_stats(&self) -> TokenStats;

    /// Adjust token tracker after compaction.
    fn adjust_for_compaction(&mut self, removed_tokens: u64, added_tokens: u64);

    /// Total estimated tokens.
    fn total_tokens(&self) -> u64;

    /// Whether the tracker has never been updated.
    fn is_fresh(&self) -> bool;
}

/// Simple token tracker used by context engines.
#[derive(Debug, Clone, Default)]
pub struct TokenTracker {
    last_input_tokens: u64,
    last_cached_tokens: u64,
    last_output_tokens: u64,
    pending_estimated_tokens: u64,
    compression_count: u64,
}

impl TokenTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update_from_usage(&mut self, input: u64, output: u64, cached: u64) {
        self.last_input_tokens = input;
        self.last_output_tokens = output;
        self.last_cached_tokens = cached;
        self.pending_estimated_tokens = 0;
    }

    pub fn record_pending(&mut self, tokens: u64) {
        self.pending_estimated_tokens += tokens;
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

    pub fn adjust_for_compaction(&mut self, removed: u64, added: u64) {
        let net = removed.saturating_sub(added);
        let from_pending = net.min(self.pending_estimated_tokens);
        self.pending_estimated_tokens -= from_pending;
        self.last_input_tokens = self.last_input_tokens.saturating_sub(net - from_pending);
        self.compression_count += 1;
    }

    pub fn input(&self) -> u64 { self.last_input_tokens }
    pub fn cached(&self) -> u64 { self.last_cached_tokens }
    pub fn output(&self) -> u64 { self.last_output_tokens }
    pub fn compression_count(&self) -> u64 { self.compression_count }
}
