//! `ContextEngine` — facade combining `CompactionPolicy` (the budget /
//! boundary decision logic) with `CompactionExecutor` (the LLM-driven
//! summarizer). RFC v2 §三.A: collapsing the two into a single touch
//! point so `Agent.run` interacts with one type, not two.
//!
//! Internal types (`CompactionPolicy`, `CompactionExecutor`,
//! `CompactionResult`) keep their existing implementations unchanged —
//! this struct is a thin coordinator that delegates to them. C18's
//! `Agent.run` rewrite will call through this facade instead of holding
//! two separate fields.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::providers::capability_chat::{ChatMessage, ToolSpec};
use crate::providers::ProviderRegistry;

use super::compaction_executor::{CompactionExecutor, CompactionResult};
use super::compaction_policy::CompactionPolicy;
use super::resource_provider::ResourceProvider;
use super::session::Session;
use super::tool_registry::ToolRegistry;
use crate::config::agent::ContextConfig;

/// Unified context-management facade.
///
/// Pairs a `CompactionPolicy` (token tracking + boundary search) with a
/// `CompactionExecutor` (summarizer + memory-tool sandbox). All methods
/// the rest of the agent needs go through here.
pub(crate) struct ContextEngine {
    policy: CompactionPolicy,
    executor: CompactionExecutor,
}

impl ContextEngine {
    /// Build from the same inputs the old AgentLoop used to construct
    /// `CompactionPolicy` + `CompactionExecutor` separately.
    pub(crate) fn new(
        context: &ContextConfig,
        registry: Arc<dyn ProviderRegistry>,
        resources: Arc<ResourceProvider>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            policy: CompactionPolicy::from_context_config(context),
            executor: CompactionExecutor::new(registry, resources, tools),
        }
    }

    // ── token tracking (delegates to policy) ────────────────────────────

    /// Total tokens currently held in context (input + cached + output + pending).
    pub(crate) fn token_total(&self) -> u64 {
        self.policy.token_total()
    }

    /// Last (input, cached, output) tuple reported by the LLM API.
    pub(crate) fn last_usage(&self) -> (u64, u64, u64) {
        self.policy.last_usage()
    }

    /// Update tracker from API response.
    pub(crate) fn update_usage(&mut self, input: u64, output: u64, cached: u64) {
        self.policy.update_usage(input, output, cached);
    }

    /// Record an estimated token addition (e.g. tool result just appended).
    pub(crate) fn record_pending(&mut self, tokens: u64) {
        self.policy.record_pending(tokens);
    }

    /// Snap to a stored total (post-restore from `last_total_tokens`).
    pub(crate) fn init_from_stored(&mut self, total: u64) {
        self.policy.init_from_stored(total);
    }

    /// Seed from a fresh history (no prior API call has happened).
    pub(crate) fn init_from_history(&mut self, system_prompt: &str, history: &[ChatMessage]) {
        self.policy.init_from_history(system_prompt, history);
    }

    /// Adjust tracker after compaction (deduct removed, add summary tokens).
    pub(crate) fn adjust_for_compaction(&mut self, removed: u64, added: u64) {
        self.policy.adjust_for_compaction(removed, added);
    }

    /// True if the tracker has never been touched.
    pub(crate) fn is_fresh(&self) -> bool {
        self.policy.is_fresh()
    }

    // ── policy queries ─────────────────────────────────────────────────

    /// Has token usage crossed the compaction threshold for `context_window`?
    pub(crate) fn should_compact(&self, context_window: u64) -> bool {
        self.policy.should_compact(context_window)
    }

    /// Find the boundary index for compaction, given the context budget.
    pub(crate) fn compaction_boundary(
        &self,
        history: &[ChatMessage],
        context_window: u64,
        system_prompt_tokens: u64,
        tool_spec_tokens: u64,
    ) -> Option<usize> {
        self.policy
            .compaction_boundary(history, context_window, system_prompt_tokens, tool_spec_tokens)
    }

    /// Public read of the configured compaction threshold (0.0–1.0).
    pub(crate) fn compact_threshold(&self) -> f64 {
        self.policy.compact_threshold
    }

    // ── execution (delegates to executor) ──────────────────────────────

    /// Run the summarizer for the given history slice and return the
    /// summary + bookkeeping. Caller (Agent.run) applies the result to
    /// the live `Session` and updates the tracker via
    /// `adjust_for_compaction`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_compaction(
        &self,
        history: &[ChatMessage],
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        boundary: usize,
        model_id: &str,
        session: &Session,
    ) -> anyhow::Result<CompactionResult> {
        self.executor
            .execute(history, system_prompt, tool_specs, boundary, model_id, session)
            .await
    }
}

// Avoid an "unused import" lint when the facade is included from the
// agents module without yet being wired into AgentLoop (transitional).
#[allow(dead_code)]
type _RwLockMarker<T> = RwLock<T>;
