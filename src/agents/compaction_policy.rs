use crate::config::agent::ContextConfig;
use crate::providers::ChatMessage;
use super::agent_impl::types::{TokenTracker, estimate_tokens, estimate_message_tokens};
use super::scheduling::work_unit;

pub(crate) struct CompactionPolicy {
    tracker: TokenTracker,
    pub(crate) compact_threshold: f64,
    pub(crate) retain_work_units: usize,
}

impl CompactionPolicy {
    pub(crate) fn from_context_config(cfg: &ContextConfig) -> Self {
        Self {
            tracker: TokenTracker::default(),
            compact_threshold: cfg.compact_threshold,
            retain_work_units: cfg.retain_work_units,
        }
    }

    pub(crate) fn init_from_stored(&mut self, total: u64) {
        self.tracker.update_from_usage(total, 0, 0);
    }

    pub(crate) fn init_from_history(&mut self, system_prompt: &str, history: &[ChatMessage]) {
        if !system_prompt.is_empty() {
            self.tracker.record_pending(estimate_tokens(system_prompt) + 4);
        }
        for msg in history {
            self.tracker.record_pending(estimate_message_tokens(msg));
        }
    }

    pub(crate) fn update_usage(&mut self, input: u64, output: u64, cached: u64) {
        self.tracker.update_from_usage(input, output, cached);
    }

    pub(crate) fn record_pending(&mut self, tokens: u64) {
        self.tracker.record_pending(tokens);
    }

    pub(crate) fn should_compact(&self, context_window: u64) -> bool {
        let threshold = (context_window as f64 * self.compact_threshold) as u64;
        self.tracker.total_tokens() >= threshold
    }

    /// Returns the boundary index for compaction, or None if nothing to compact.
    ///
    /// budget = context_window * threshold - system_prompt_tokens - tool_spec_tokens
    pub(crate) fn compaction_boundary(
        &self,
        history: &[ChatMessage],
        context_window: u64,
        system_prompt_tokens: u64,
        tool_spec_tokens: u64,
    ) -> Option<usize> {
        let budget = ((context_window as f64 * self.compact_threshold) as u64)
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(tool_spec_tokens);
        if budget == 0 {
            return None;
        }
        work_unit::find_compaction_boundary_for_budget(history, budget, self.retain_work_units.max(1))
    }

    pub(crate) fn adjust_for_compaction(&mut self, removed: u64, added: u64) {
        self.tracker.adjust_for_compaction(removed, added);
    }

    pub(crate) fn token_total(&self) -> u64 {
        self.tracker.total_tokens()
    }

    pub(crate) fn last_usage(&self) -> (u64, u64, u64) {
        (self.tracker.last_input(), self.tracker.last_cached(), self.tracker.last_output())
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.tracker.is_fresh()
    }
}
