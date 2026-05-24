//! ContextEngine — unified façade over CompactionPolicy + CompactionExecutor.
//!
//! RFC v2 C21: merges the two previously-separate structs into a single
//! entry point. Internal details are preserved; this is a structural
//! refactoring that simplifies the Agent's field count.

use std::sync::Arc;

use crate::config::agent::ContextConfig;
use crate::providers::{ChatMessage, ProviderRegistry};
use crate::providers::capability_chat::ToolSpec;
use crate::agents::resource_provider::ResourceProvider;
use crate::agents::tool_registry::ToolRegistry;
use super::compaction_policy::CompactionPolicy;
use super::compaction_executor::{CompactionExecutor, CompactionResult};

/// Unified context management: token tracking + compaction policy + execution.
///
/// Replaces the separate `CompactionPolicy` + `CompactionExecutor` fields
/// that were held by `AgentLoop`.
pub(crate) struct ContextEngine {
    policy: CompactionPolicy,
    executor: CompactionExecutor,
}

impl ContextEngine {
    pub(crate) fn new(
        config: &ContextConfig,
        registry: Arc<dyn ProviderRegistry>,
        resources: Arc<ResourceProvider>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            policy: CompactionPolicy::from_context_config(config),
            executor: CompactionExecutor::new(registry, resources, tools),
        }
    }

    // ── Policy delegates ──────────────────────────────────────────────────

    pub(crate) fn init_from_stored(&mut self, total: u64) {
        self.policy.init_from_stored(total);
    }

    pub(crate) fn init_from_history(&mut self, system_prompt: &str, history: &[ChatMessage]) {
        self.policy.init_from_history(system_prompt, history);
    }

    pub(crate) fn update_usage(&mut self, input: u64, output: u64, cached: u64) {
        self.policy.update_usage(input, output, cached);
    }

    pub(crate) fn record_pending(&mut self, tokens: u64) {
        self.policy.record_pending(tokens);
    }

    pub(crate) fn should_compact(&self, context_window: u64) -> bool {
        self.policy.should_compact(context_window)
    }

    pub(crate) fn compaction_boundary(
        &self,
        history: &[ChatMessage],
        context_window: u64,
        system_prompt_tokens: u64,
        tool_spec_tokens: u64,
    ) -> Option<usize> {
        self.policy.compaction_boundary(history, context_window, system_prompt_tokens, tool_spec_tokens)
    }

    pub(crate) fn adjust_for_compaction(&mut self, removed: u64, added: u64) {
        self.policy.adjust_for_compaction(removed, added);
    }

    pub(crate) fn token_total(&self) -> u64 {
        self.policy.token_total()
    }

    pub(crate) fn last_usage(&self) -> (u64, u64, u64) {
        self.policy.last_usage()
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.policy.is_fresh()
    }

    // ── Executor delegates ────────────────────────────────────────────────

    pub(crate) async fn execute_compaction(
        &self,
        history: &[ChatMessage],
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        boundary: usize,
        model_id: &str,
    ) -> anyhow::Result<CompactionResult> {
        self.executor.execute(history, system_prompt, tool_specs, boundary, model_id).await
    }
}
