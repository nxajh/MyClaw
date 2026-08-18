//! `ContextEngine` — unified context-management facade.
//!
//! RFC v2 §三.A: collapses `CompactionPolicy` + `CompactionExecutor` into
//! a single type so `Agent.run` interacts with one touch point, not two.
//! The per-session `TokenTracker` lives on `Session.token_tracker` (not
//! here) — methods that need a token count take it as a parameter.
//!
//! Internals are private free functions inside this module; the public
//! surface is the `ContextEngine` impl block.

use std::sync::Arc;

use futures_util::StreamExt;

use crate::agents::resource_provider::ResourceProvider;
use crate::agents::scheduling::work_unit;
use crate::agents::session::Session;
use crate::agents::tokens::{
    estimate_history_tokens_with_media, estimate_message_tokens, estimate_tokens,
};
use crate::agents::tool_executor::MemoryToolExecutor;
use crate::agents::tool_registry::ToolRegistry;
use crate::config::agent::ContextConfig;
use crate::storage::SummaryRecord;
use crate::providers::capability_chat::{ChatProvider, ToolSpec};
use crate::providers::{
    BoxStream, ChatMessage, ChatRequest, ChatUsage, ContentPart, ProviderRegistry, StreamEvent,
    ThinkingConfig, ToolCall,
};

/// Result returned by `ContextEngine::execute_compaction`.
/// Caller is responsible for applying it to the live session (drain /
/// insert history, update metadata, adjust `Session.token_tracker`).
pub(crate) struct CompactionResult {
    pub compact_start: usize,
    pub compact_end: usize,
    pub summary: String,
    pub summary_tokens: u64,
    pub removed_tokens: u64,
    #[allow(dead_code)]
    pub compacted_count: usize,
}

const SUMMARY_OUTPUT_RESERVE: u64 = 20_000;
const COMPRESSION_SAFETY_MARGIN: u64 = 4_000;

/// Unified context-management facade. Holds the compaction threshold
/// / retain-units policy plus the summarizer plumbing (provider
/// registry, memory-tool executor) in one struct.
pub struct ContextEngine {
    compact_threshold: f64,
    retain_work_units: usize,
    registry: Arc<dyn ProviderRegistry>,
    resources: Arc<ResourceProvider>,
    memory_executor: MemoryToolExecutor,
    max_rounds: usize,
}

#[allow(dead_code)] // some accessors retained for /compact + future callers
impl ContextEngine {
    pub fn new(
        context: &ContextConfig,
        registry: Arc<dyn ProviderRegistry>,
        resources: Arc<ResourceProvider>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            compact_threshold: context.compact_threshold,
            retain_work_units: context.retain_work_units,
            registry,
            resources,
            memory_executor: MemoryToolExecutor::new(tools),
            max_rounds: 30,
        }
    }

    /// Public read of the configured compaction threshold (0.0–1.0).
    pub fn compact_threshold(&self) -> f64 {
        self.compact_threshold
    }

    /// Rebuild the LLM request message list after history mutation
    /// (compaction or force-drop fallback).
    ///
    /// Assembles: system prompt + cloned history + `sanitize_history` +
    /// `fold_absent_tool` for the on-demand send tools. This is the single
    /// post-mutation rebuild path; the initial `run()` assembly is kept
    /// inline because its ordering with `filter_modality_redundant_tools`
    /// (which needs the sanitized messages before `tool_specs` exist) is
    /// different.
    pub fn rebuild_messages(
        &self,
        system_prompt: &str,
        history: &[ChatMessage],
        tool_specs: &[ToolSpec],
    ) -> Vec<ChatMessage> {
        let mut messages: Vec<ChatMessage> = std::iter::once(ChatMessage::system_text(system_prompt))
            .chain(history.iter().cloned())
            .collect();
        crate::agents::session::sanitize_history(&mut messages);
        crate::agents::agent::fold_absent_tool(
            &mut messages,
            tool_specs,
            "send_message",
            "消息发送结果",
        );
        crate::agents::agent::fold_absent_tool(&mut messages, tool_specs, "send_media", "媒体发送结果");
        messages
    }

    // ── Compaction control flow ──────────────────────────────────────────────

    /// Compact `session.history` when it's over (or `force`d past) the model's
    /// context threshold, returning the rebuilt `messages` prefix on success.
    ///
    /// Called as a pre-send guard each loop iteration and as the context-overflow
    /// backstop. The threshold decision uses a direct history estimate
    /// (`estimate_history_tokens_with_media` under the model's MediaPolicy) rather
    /// than the token tracker, so a stale or under-counted tracker can't let an
    /// over-window request slip through. File parts are sized as markers when the
    /// model cannot inline them (e.g. glm-5.2 text-only), avoiding false overflow
    /// from base64 estimates. Returns `None` when no compaction was needed
    /// (`!force`) or none was possible (no boundary / summarizer error).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn maybe_compact(
        &self,
        session: &mut Session,
        system_prompt: &str,
        model_id: &str,
        provider: Arc<dyn ChatProvider>,
        tool_specs: &[ToolSpec],
        task_boards: Option<&Arc<crate::tools::TaskBoards>>,
        force: bool,
        override_retain: Option<usize>,
    ) -> Option<(Vec<ChatMessage>, u64, u64)> {
        let cfg = match self.registry.get_chat_model_config(model_id) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(model_id, error = %e, "maybe_compact: model config lookup failed, skipping compaction");
                return None;
            }
        };
        let window = match cfg.context_window {
            Some(w) => w,
            None => {
                tracing::warn!(model_id, "maybe_compact: context_window is None, skipping compaction");
                return None;
            }
        };
        // Match lower_media_for: text-only models pay marker cost, vision pays base64.
        let media_policy = self.registry.get_chat_media_policy(model_id);
        let estimate =
            estimate_history_tokens_with_media(system_prompt, &session.history, media_policy);
        tracing::debug!(
            model_id,
            estimate,
            window,
            threshold = (window as f64 * self.compact_threshold) as u64,
            force,
            history_len = session.history.len(),
            has_media_policy = media_policy.is_some(),
            "maybe_compact: compaction check"
        );
        if !force && !self.should_compact(estimate, window) {
            return None;
        }

        let sys_prompt_tokens = estimate_tokens(system_prompt);
        let tool_spec_tokens: u64 = tool_specs
            .iter()
            .map(|s| {
                estimate_tokens(&s.name)
                    + s.description.as_deref().map_or(0, estimate_tokens)
                    + estimate_tokens(&s.input_schema.to_string())
                    + 8
            })
            .sum();
        let compress_budget = window
            .saturating_sub(SUMMARY_OUTPUT_RESERVE)
            .saturating_sub(sys_prompt_tokens)
            .saturating_sub(tool_spec_tokens)
            .saturating_sub(COMPRESSION_SAFETY_MARGIN);
        let boundary = self.compaction_boundary_with_retain(
            &session.history,
            window,
            sys_prompt_tokens,
            tool_spec_tokens,
            override_retain,
        );

        // Snapshot history for the summarizer (which reads a slice); the live
        // session is passed through for the memory tools inside the summarizer.
        let history_snap: Vec<ChatMessage> = session.history.to_vec();

        // Prefer normal retain-based compaction. When the boundary is missing or
        // the incremental range is empty (typical: single-user long tool chain
        // where every work unit shares one user_start), fall back to folding
        // the completed prefix only — never past an open trailing user.
        let compact_outcome = if let Some(boundary) = boundary {
            match self
                .execute_compaction(
                    &history_snap,
                    system_prompt,
                    tool_specs,
                    boundary,
                    model_id,
                    Arc::clone(&provider),
                    session,
                    Some(compress_budget),
                )
                .await
            {
                Ok(result) => Ok(result),
                Err(e) if is_empty_compact_error(&e) => {
                    tracing::warn!(
                        session = %session.id,
                        err = %e,
                        history_len = history_snap.len(),
                        protected_from = ?open_user_protected_from(&history_snap),
                        "incremental compaction empty; fold completed prefix (protect open user)"
                    );
                    self.execute_full_fold_compaction(
                        &history_snap,
                        system_prompt,
                        tool_specs,
                        model_id,
                        Arc::clone(&provider),
                        session,
                        compress_budget,
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        } else {
            tracing::warn!(
                session = %session.id,
                history_len = history_snap.len(),
                force,
                protected_from = ?open_user_protected_from(&history_snap),
                "no retain boundary; fold completed prefix (protect open user)"
            );
            self.execute_full_fold_compaction(
                &history_snap,
                system_prompt,
                tool_specs,
                model_id,
                Arc::clone(&provider),
                session,
                compress_budget,
            )
            .await
        };

        match compact_outcome {
            Ok(result) => {
                let version = session.compact_version + 1;
                let summary_prefix = "[CONTEXT COMPACTION — REFERENCE ONLY] ";

                // Inject active task/goal state so the model retains its plan
                // across context compaction (P1-B1: the session's own board).
                let task_injection = if let Some(boards) = task_boards {
                    let board = boards.board(&session.id);
                    let state = board.read().await;
                    state.format_for_injection()
                } else {
                    None
                };

                let summary_text = match &task_injection {
                    Some(tasks) => format!("{}{}\n\n{}", summary_prefix, result.summary, tasks),
                    None => format!("{}{}", summary_prefix, result.summary),
                };
                let summary_msg = ChatMessage::user_text(summary_text);
                // Use compact_end (not the retain boundary) so full-fold
                // retain=0 still records the correct up_to_message.
                let last_compacted_id = session
                    .message_ids
                    .get(result.compact_end.saturating_sub(1))
                    .copied()
                    .unwrap_or(0);
                session.apply_compaction(
                    result.compact_start,
                    result.compact_end,
                    summary_msg,
                    version,
                    last_compacted_id,
                    result.summary_tokens,
                );
                session
                    .token_tracker
                    .adjust_for_compaction(result.removed_tokens, result.summary_tokens);
                if let Some(ref hook) = session.persist {
                    hook.save_compaction(
                        &session.id,
                        &SummaryRecord {
                            id: 0,
                            version,
                            summary: result.summary.clone(),
                            up_to_message: last_compacted_id,
                            token_estimate: Some(result.summary_tokens),
                            created_at: chrono::Utc::now(),
                        },
                    );
                    let surviving: Vec<(i64, ChatMessage)> = session
                        .message_ids
                        .iter()
                        .zip(session.history.iter())
                        .map(|(&id, msg)| (id, msg.clone()))
                        .collect();
                    hook.rotate_history(&session.id, &surviving);
                }
                let messages = self.rebuild_messages(system_prompt, &session.history, tool_specs);
                tracing::info!(
                    summary_tokens = result.summary_tokens,
                    removed_tokens = result.removed_tokens,
                    estimate,
                    window,
                    "compaction completed"
                );
                Some((messages, result.removed_tokens, result.summary_tokens))
            }
            Err(e) => {
                tracing::warn!(
                    session = %session.id,
                    model = %model_id,
                    msg_count = session.history.len(),
                    err = %e,
                    "compaction failed, continuing"
                );
                None
            }
        }
    }

    /// Compact repeatedly until the history estimate drops below the compaction
    /// threshold, or no further progress is possible.
    ///
    /// A single `maybe_compact` pass folds only a bounded prefix into the rolling
    /// summary, so a history far over the window may need multiple passes within
    /// one turn. `force` applies only to the first pass; later passes are gated
    /// by `should_compact`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn compact_until_fit(
        &self,
        session: &mut Session,
        system_prompt: &str,
        model_id: &str,
        provider: Arc<dyn ChatProvider>,
        tool_specs: &[ToolSpec],
        task_boards: Option<&Arc<crate::tools::TaskBoards>>,
        force: bool,
    ) -> Option<Vec<ChatMessage>> {
        const MAX_PASSES: usize = 10;
        let configured_retain = self.retain_work_units();
        let mut retain = configured_retain;
        let mut latest: Option<Vec<ChatMessage>> = None;
        let mut stall_count: usize = 0;

        for pass in 0..MAX_PASSES {
            let force_pass = force && pass == 0;
            let override_retain = if retain != configured_retain {
                Some(retain)
            } else {
                None
            };
            match self
                .maybe_compact(
                    session,
                    system_prompt,
                    model_id,
                    Arc::clone(&provider),
                    tool_specs,
                    task_boards,
                    force_pass,
                    override_retain,
                )
                .await
            {
                Some((messages, removed, summary)) => {
                    let net = removed.saturating_sub(summary);
                    if removed > 0 && (net as f64) / (removed as f64) < 0.05 {
                        stall_count += 1;
                    } else {
                        stall_count = 0;
                    }

                    if stall_count >= 2 && retain > 1 {
                        retain -= 1;
                        tracing::info!(
                            retain,
                            pass,
                            "compaction stalled, lowering retain_work_units"
                        );
                        stall_count = 0;
                    }
                    latest = Some(messages);
                }
                None => break,
            }
        }

        // Last-resort fallback: hard-fold the completed prefix only. Never swallow
        // an open trailing user (protected tail). Never half-cut mid-tool-chain
        // without a summary — that drops user turns and yields illegal shapes.
        if latest.is_none() && force && !session.history.is_empty() {
            let protected_from =
                open_user_protected_from(&session.history).unwrap_or(session.history.len());
            let fold_end = protected_from;
            if fold_end == 0 {
                tracing::warn!(
                    session = %session.id,
                    msg_count = session.history.len(),
                    "compaction last-resort: nothing before open user; skip fold"
                );
            } else {
                let version = session.compact_version + 1;
                let to_fold = &session.history[..fold_end];
                let removed_tokens: u64 = to_fold.iter().map(estimate_message_tokens).sum();
                let hard = hard_fold_history(to_fold);
                let summary_text =
                    format!("[CONTEXT COMPACTION — REFERENCE ONLY] {}", hard);
                let summary_msg = ChatMessage::user_text(summary_text);
                let summary_tokens = estimate_message_tokens(&summary_msg);
                let last_compacted_id = session
                    .message_ids
                    .get(fold_end.saturating_sub(1))
                    .copied()
                    .unwrap_or(0);
                tracing::warn!(
                    session = %session.id,
                    fold_end,
                    protected_from,
                    msg_count = session.history.len(),
                    removed_tokens,
                    summary_tokens,
                    "compaction exhausted all passes; hard-fold completed prefix (protect open user)"
                );
                session.apply_compaction(
                    0,
                    fold_end,
                    summary_msg,
                    version,
                    last_compacted_id,
                    summary_tokens,
                );
                session
                    .token_tracker
                    .adjust_for_compaction(removed_tokens, summary_tokens);
                if let Some(ref hook) = session.persist {
                    hook.save_compaction(
                        &session.id,
                        &SummaryRecord {
                            id: 0,
                            version,
                            summary: hard,
                            up_to_message: last_compacted_id,
                            token_estimate: Some(summary_tokens),
                            created_at: chrono::Utc::now(),
                        },
                    );
                    let surviving: Vec<(i64, ChatMessage)> = session
                        .message_ids
                        .iter()
                        .zip(session.history.iter())
                        .map(|(&id, msg)| (id, msg.clone()))
                        .collect();
                    hook.rotate_history(&session.id, &surviving);
                }
                let messages = self.rebuild_messages(system_prompt, &session.history, tool_specs);
                latest = Some(messages);
            }
        } else if latest.is_none() && force {
            tracing::warn!(
                session = %session.id,
                msg_count = session.history.len(),
                "compaction repeatedly failed; context will continue growing"
            );
        }

        latest
    }

    /// Public read of the configured retain_work_units.
    pub fn retain_work_units(&self) -> usize {
        self.retain_work_units
    }

    /// Configured timezone offset in hours, sourced from the shared
    /// `ResourceProvider` (`[prompt] timezone_offset`). Used by the
    /// per-turn attachment diff for date injection.
    pub fn timezone_offset(&self) -> i32 {
        self.resources.timezone_offset()
    }

    /// True if the supplied token count has crossed the compaction
    /// threshold for `context_window`. Caller passes
    /// `session.token_tracker.total_tokens()` — the engine is
    /// stateless w.r.t. token tracking.
    pub fn should_compact(&self, total_tokens: u64, context_window: u64) -> bool {
        let threshold = (context_window as f64 * self.compact_threshold) as u64;
        total_tokens >= threshold
    }

    /// Find the boundary index for compaction, given the context budget.
    pub fn compaction_boundary(
        &self,
        history: &[ChatMessage],
        context_window: u64,
        system_prompt_tokens: u64,
        tool_spec_tokens: u64,
    ) -> Option<usize> {
        self.compaction_boundary_with_retain(
            history,
            context_window,
            system_prompt_tokens,
            tool_spec_tokens,
            None,
        )
    }

    /// Find the boundary index for compaction with an optional override for
    /// `retain_work_units`. When `Some(n)`, `n` is used instead of the
    /// configured value. Used by `compact_until_fit` to progressively
    /// lower the retain count when compaction stalls.
    pub fn compaction_boundary_with_retain(
        &self,
        history: &[ChatMessage],
        context_window: u64,
        system_prompt_tokens: u64,
        tool_spec_tokens: u64,
        override_retain: Option<usize>,
    ) -> Option<usize> {
        let budget = context_window
            .saturating_sub(SUMMARY_OUTPUT_RESERVE)
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(tool_spec_tokens)
            .saturating_sub(COMPRESSION_SAFETY_MARGIN);
        if budget == 0 {
            return None;
        }
        tracing::debug!(
            context_window,
            summary_output_reserve = SUMMARY_OUTPUT_RESERVE,
            system_prompt_tokens,
            tool_spec_tokens,
            compression_safety_margin = COMPRESSION_SAFETY_MARGIN,
            compress_budget = budget,
            "computed compaction input budget"
        );
        let retain = override_retain.unwrap_or(self.retain_work_units).max(1);
        work_unit::find_compaction_boundary_for_budget(history, budget, retain)
    }

    /// Generate a compaction summary for `history[0..boundary]`.
    ///
    /// `tool_specs` must match the spec list used for the main LLM
    /// request so the provider's prefix cache key (model +
    /// system_prompt + tool_definitions) matches and the summarizer
    /// call hits the cache. Execution is gated separately by
    /// `MemoryToolExecutor`, which permits only the memory tools.
    ///
    /// When `compress_budget` is set and the slice to summarize exceeds it,
    /// skip the LLM summarizer and produce a deterministic hard-fold instead
    /// (avoids feeding an already-over-window history into the same model).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_compaction(
        &self,
        history: &[ChatMessage],
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        boundary: usize,
        model_id: &str,
        provider: Arc<dyn ChatProvider>,
        session: &crate::agents::session::Session,
        compress_budget: Option<u64>,
    ) -> anyhow::Result<CompactionResult> {
        // P0 invariant: never compact past an open trailing user turn.
        let protected_from = open_user_protected_from(history).unwrap_or(history.len());
        let boundary = boundary.min(protected_from);

        let (replace_start, compact_start, compact_end, existing_summary) =
            find_incremental_range(history, boundary);

        let to_compact: Vec<ChatMessage> = history[compact_start..compact_end].to_vec();
        if to_compact.is_empty() {
            anyhow::bail!("no content to compact");
        }

        let compacted_count = to_compact.len();
        let removed_tokens: u64 = history[replace_start..compact_end]
            .iter()
            .map(estimate_message_tokens)
            .sum();

        tracing::info!(
            replace_start,
            compact_start,
            compact_end,
            boundary,
            has_existing_summary = existing_summary.is_some(),
            "compaction range determined"
        );

        let slice_tokens: u64 = to_compact.iter().map(estimate_message_tokens).sum();
        let use_hard = compress_budget.is_some_and(|b| b == 0 || slice_tokens > b);

        let summary = if use_hard {
            tracing::warn!(
                session = %session.id,
                slice_tokens,
                compress_budget = ?compress_budget,
                "compaction slice exceeds summarizer budget; hard-fold"
            );
            let mut body = hard_fold_history(&to_compact);
            if let Some(existing) = existing_summary.as_deref() {
                let existing = existing
                    .strip_prefix("[CONTEXT COMPACTION — REFERENCE ONLY]")
                    .unwrap_or(existing)
                    .trim();
                if !existing.is_empty() {
                    body = format!(
                        "## Prior Summary\n{}\n\n## Folded Updates\n{}",
                        truncate_chars(existing, HARD_PRIOR_SUMMARY_CHARS),
                        body
                    );
                }
            }
            body
        } else {
            match self
                .summarize(
                    &to_compact,
                    existing_summary.as_deref(),
                    system_prompt,
                    tool_specs,
                    model_id,
                    provider,
                    session,
                )
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        session = %session.id,
                        err = %e,
                        "summarize failed; hard-fold fallback"
                    );
                    let mut body = hard_fold_history(&to_compact);
                    if let Some(existing) = existing_summary.as_deref() {
                        let existing = existing
                            .strip_prefix("[CONTEXT COMPACTION — REFERENCE ONLY]")
                            .unwrap_or(existing)
                            .trim();
                        if !existing.is_empty() {
                            body = format!(
                                "## Prior Summary\n{}\n\n## Folded Updates\n{}",
                                truncate_chars(existing, HARD_PRIOR_SUMMARY_CHARS),
                                body
                            );
                        }
                    }
                    body
                }
            }
        };

        let (ok, reasons) = audit_summary_quality(&to_compact, &summary);
        if !ok {
            tracing::warn!(reasons = ?reasons, "summary quality audit failed (non-blocking)");
        }

        let summary = append_evidence_index(
            summary,
            &to_compact,
            compact_start,
            compact_end,
            boundary,
            model_id,
        );
        let summary_tokens = estimate_message_tokens(&ChatMessage::user_text(summary.clone()));

        Ok(CompactionResult {
            compact_start: replace_start,
            compact_end,
            summary,
            summary_tokens,
            removed_tokens,
            compacted_count,
        })
    }

    /// Fold the completed prefix of history into a summary.
    ///
    /// Used when retain-based boundary is missing or incremental range is empty
    /// (single-user long tool chains). **Never** extends past an open trailing
    /// user (`open_user_protected_from`) — after apply, that user remains intact.
    #[allow(clippy::too_many_arguments)]
    async fn execute_full_fold_compaction(
        &self,
        history: &[ChatMessage],
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        model_id: &str,
        provider: Arc<dyn ChatProvider>,
        session: &crate::agents::session::Session,
        compress_budget: u64,
    ) -> anyhow::Result<CompactionResult> {
        if history.is_empty() {
            anyhow::bail!("no content to compact");
        }

        // Fold only up to the protected open-user tail (or whole history if none).
        let fold_end = open_user_protected_from(history).unwrap_or(history.len());
        if fold_end == 0 {
            anyhow::bail!("no content to compact");
        }

        // If the only content under fold_end is a prior summary (or the
        // incremental slice is otherwise empty), hard-fold that prefix
        // directly — re-entering execute_compaction would bail again.
        let (_rs, cs, ce, _existing) = find_incremental_range(history, fold_end);
        if cs >= ce {
            // Prefer folding completed work units before the open user if any
            // exist earlier; otherwise hard-fold [0, fold_end).
            let summary = hard_fold_history(&history[..fold_end]);
            let summary_tokens =
                estimate_message_tokens(&ChatMessage::user_text(summary.clone()));
            let removed_tokens: u64 =
                history[..fold_end].iter().map(estimate_message_tokens).sum();
            tracing::warn!(
                session = %session.id,
                history_len = history.len(),
                fold_end,
                "full-fold: empty incremental slice; hard-fold completed prefix"
            );
            return Ok(CompactionResult {
                compact_start: 0,
                compact_end: fold_end,
                summary,
                summary_tokens,
                removed_tokens,
                compacted_count: fold_end,
            });
        }

        // boundary = fold_end → compact completed prefix; find_incremental_range
        // strips an existing leading summary from the LLM input. compact_end is
        // clamped again inside execute_compaction.
        self.execute_compaction(
            history,
            system_prompt,
            tool_specs,
            fold_end,
            model_id,
            provider,
            session,
            Some(compress_budget),
        )
        .await
        .map(|mut r| {
            // Replace from 0 through the (already protected) compact_end.
            r.compact_start = 0;
            r.compact_end = r.compact_end.min(fold_end);
            r.removed_tokens = history[..r.compact_end]
                .iter()
                .map(estimate_message_tokens)
                .sum();
            r.compacted_count = r.compact_end;
            r
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn summarize(
        &self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        model_id: &str,
        provider: Arc<dyn ChatProvider>,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<String> {
        match self
            .do_summarize(
                to_compact,
                existing_summary,
                system_prompt,
                tool_specs,
                model_id,
                provider,
                session,
            )
            .await
        {
            Ok(s) if !s.trim().is_empty() => Ok(s),
            Ok(_) => {
                tracing::warn!(
                    session = %session.id,
                    model = %model_id,
                    "summarize returned empty text"
                );
                anyhow::bail!("summarize returned empty text")
            }
            Err(e) => {
                tracing::warn!(
                    session = %session.id,
                    model = %model_id,
                    err = %e,
                    "summarize failed"
                );
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn do_summarize(
        &self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        model_id: &str,
        provider: Arc<dyn ChatProvider>,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<String> {
        let mut messages: Vec<ChatMessage> = Vec::new();

        if !system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(system_prompt));
        }

        for msg in to_compact {
            messages.push(strip_images(msg));
        }

        let prompt = build_summarizer_prompt(to_compact.len(), existing_summary);
        messages.push(ChatMessage::user_text(prompt));

        let thinking = self
            .registry
            .get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| {
                if cfg.reasoning {
                    Some(ThinkingConfig {
                        enabled: true,
                        effort: None,
                    })
                } else {
                    None
                }
            });

        let mut round = 0;
        let final_text = loop {
            round += 1;
            if round > self.max_rounds {
                anyhow::bail!("summarize loop exceeded {} rounds", self.max_rounds);
            }

            let req = ChatRequest {
                model: model_id,
                messages: &messages,
                temperature: None,
                max_tokens: Some(SUMMARY_OUTPUT_RESERVE as u32),
                thinking: thinking.clone(),
                stop: None,
                seed: None,
                // Carry the *full* tool list (not just the executable subset) so
                // the prefix-cache key (model + system_prompt + tool_definitions)
                // matches the main request and the summarizer call hits the
                // cache. Gating happens in the executor: MemoryToolExecutor
                // permits only the memory tools and blocks everything else.
                tools: if tool_specs.is_empty() {
                    None
                } else {
                    Some(tool_specs)
                },
                stream: true,
            };

            let stream = provider.chat(req)?;
            let response = self.collect_summary_stream(stream).await?;

            if let Some(ref usage) = response.usage {
                if let Some(cached) = usage.cached_input_tokens {
                    tracing::info!(
                        round,
                        cached_tokens = cached,
                        total_input = usage.input_tokens.unwrap_or(0),
                        "summarizer cache hit"
                    );
                }
            }

            if response.tool_calls.is_empty() {
                break response.text;
            }

            tracing::info!(
                round,
                tool_calls = response.tool_calls.len(),
                "summarize: model requested tool calls"
            );

            let mut assistant_msg = ChatMessage::assistant_text(&response.text);
            assistant_msg.tool_calls = Some(response.tool_calls.clone());
            if let Some(ref thinking_text) = response.reasoning_content {
                assistant_msg.parts.insert(
                    0,
                    ContentPart::Thinking {
                        thinking: thinking_text.clone(),
                        signature: response.thinking_signature.clone(),
                    },
                );
            }
            messages.push(assistant_msg);

            for call in &response.tool_calls {
                tracing::info!(tool = %call.name, id = %call.id, "summarize: executing tool");
                let result = self.memory_executor.execute(call, session).await;
                let (result_content, is_error) = match &result {
                    Ok(r) => {
                        let mut out = r.output.clone();
                        if let Some(ref err) = r.error {
                            if out.is_empty() {
                                out = format!("error: {}", err);
                            }
                        }
                        (out, !r.success)
                    }
                    Err(e) => (format!("error: {}", e), true),
                };
                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(is_error);
                messages.push(tool_msg);
            }
        };

        Ok(final_text)
    }

    async fn collect_summary_stream(
        &self,
        mut stream: BoxStream<StreamEvent>,
    ) -> anyhow::Result<SummaryResponse> {
        let mut text = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut thinking_signature: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage: Option<ChatUsage> = None;
        let chunk_timeout = crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT;

        loop {
            match tokio::time::timeout(chunk_timeout, stream.next()).await {
                Ok(Some(event)) => match event {
                    StreamEvent::Delta { text: delta } => text.push_str(&delta),
                    StreamEvent::Thinking { text: delta } => {
                        if !delta.is_empty() {
                            if let Some(rc) = &mut reasoning_content {
                                rc.push_str(&delta);
                            } else {
                                reasoning_content = Some(delta);
                            }
                        }
                    }
                    StreamEvent::ToolCallStart {
                        id,
                        name,
                        initial_arguments,
                    } => {
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments: initial_arguments,
                        });
                    }
                    StreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        delta,
                    } => {
                        let idx = index as usize;
                        while tool_calls.len() <= idx {
                            tool_calls.push(ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
                        }
                        let call = &mut tool_calls[idx];
                        if !id.is_empty() {
                            call.id = id;
                        }
                        if !name.is_empty() {
                            call.name = name;
                        }
                        call.arguments.push_str(&delta);
                    }
                    StreamEvent::ToolCallEnd {
                        id,
                        name,
                        arguments,
                    } => {
                        if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                            call.name = name;
                            call.arguments = arguments;
                        }
                    }
                    StreamEvent::Usage(u) => {
                        if let Some(ref mut existing) = usage {
                            if u.input_tokens.is_some() {
                                existing.input_tokens = u.input_tokens;
                            }
                            if u.output_tokens.is_some() {
                                existing.output_tokens = u.output_tokens;
                            }
                            if u.cached_input_tokens.is_some() {
                                existing.cached_input_tokens = u.cached_input_tokens;
                            }
                        } else {
                            usage = Some(u);
                        }
                    }
                    StreamEvent::Done { .. } => break,
                    StreamEvent::ThinkingSignature { signature } => {
                        thinking_signature = Some(signature);
                    }
                    StreamEvent::HttpError { message, .. } => {
                        anyhow::bail!("summarizer stream error: {}", message)
                    }
                    StreamEvent::Error(e) => anyhow::bail!("summarizer stream error: {}", e),
                    StreamEvent::ModelUsed { .. } => {} // informational; summarizer keeps its model_id
                },
                Ok(None) => {
                    tracing::warn!("summarizer stream ended without Done event");
                    break;
                }
                Err(_) => anyhow::bail!(
                    "summarizer stream chunk timeout after {}s",
                    chunk_timeout.as_secs()
                ),
            }
        }

        Ok(SummaryResponse {
            text,
            reasoning_content,
            thinking_signature,
            tool_calls,
            usage,
        })
    }
}

// ── Private helpers ─────────────────────────────────────────────────────

struct SummaryResponse {
    text: String,
    reasoning_content: Option<String>,
    thinking_signature: Option<String>,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    usage: Option<ChatUsage>,
}

fn find_incremental_range(
    history: &[ChatMessage],
    boundary: usize,
) -> (usize, usize, usize, Option<String>) {
    let last_summary = history[..boundary].iter().rposition(|m| {
        m.role == "user"
            && m.text_content()
                .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
    });
    match last_summary {
        Some(idx) => {
            let existing = history[idx].text_content();
            (idx, idx + 1, boundary, Some(existing))
        }
        None => (0, 0, boundary, None),
    }
}

fn is_empty_compact_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("no content to compact")
}

/// If history ends with one or more unanswered `user` messages, return the
/// index of the first real (non-compaction-summary) open user in that trailing
/// streak.
///
/// Compaction must never set `compact_end` past this index — the open user
/// (and any preceding pure-attachment user siblings in the trailing streak)
/// must survive `apply_compaction` as live history so the model still sees
/// the current request. Leading compaction-summary user messages inside the
/// trailing streak are *not* protected (they can be re-folded) and are skipped
/// when computing the index.
fn open_user_protected_from(history: &[ChatMessage]) -> Option<usize> {
    if history.is_empty() {
        return None;
    }
    // Start of the trailing user-role streak (may include summaries + reminders).
    let mut streak_start = history.len();
    while streak_start > 0 && history[streak_start - 1].role == "user" {
        streak_start -= 1;
    }
    if streak_start == history.len() {
        return None; // last message is not user → no open user turn
    }
    // Skip compaction summaries at the front of the streak; protect from the
    // first non-summary user (attachment reminder or real request).
    let mut protected = streak_start;
    while protected < history.len()
        && history[protected]
            .text_content()
            .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
    {
        protected += 1;
    }
    if protected >= history.len() {
        // Trailing streak is only prior summary/summaries — no open request.
        None
    } else {
        Some(protected)
    }
}

/// Strip a leading `<system-reminder>...</system-reminder>` block so hard-fold
/// keeps the real user body instead of burning the per-msg budget on injection.
fn strip_leading_system_reminder(text: &str) -> &str {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("<system-reminder>") {
        if let Some(end) = rest.find("</system-reminder>") {
            return rest[end + "</system-reminder>".len()..].trim();
        }
    }
    text
}

/// Max chars kept from a prior summary when hard-folding an update on top.
const HARD_PRIOR_SUMMARY_CHARS: usize = 6_000;
/// Per-message body budget in hard-fold (user / assistant text).
const HARD_MSG_CHARS: usize = 400;
/// Per tool-result body budget in hard-fold.
const HARD_TOOL_CHARS: usize = 200;
/// Total hard-fold body budget (before evidence index).
const HARD_TOTAL_CHARS: usize = 12_000;

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Truncate preferring the tail (user intent often at the end after injection).
fn truncate_chars_prefer_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let skip = count.saturating_sub(keep);
    let mut out = String::from("…");
    out.extend(s.chars().skip(skip));
    out
}

/// User text for hard-fold: strip system-reminder, keep body, prefer tail if long.
fn user_text_for_hard_fold(text: &str) -> String {
    if text.starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]") {
        return format!(
            "[prior compaction summary, {} chars]",
            text.chars().count()
        );
    }
    let body = strip_leading_system_reminder(text);
    if body.is_empty() {
        if text.contains("<system-reminder>") {
            return "[system-reminder only]".to_string();
        }
        return String::new();
    }
    // Short bodies kept whole; long bodies keep the tail (request text).
    if body.chars().count() <= HARD_MSG_CHARS {
        body.to_string()
    } else {
        truncate_chars_prefer_tail(body, HARD_MSG_CHARS)
    }
}

/// Collect File-part path markers so hard-fold does not drop media references.
/// `text_content()` only joins Text parts; pure-image user turns would otherwise
/// fold to empty / system-reminder-only and lose `[图片: path]`.
fn media_markers_for_hard_fold(msg: &ChatMessage) -> String {
    let mut markers = Vec::new();
    for part in &msg.parts {
        if let ContentPart::File {
            path, mime_type, ..
        } = part
        {
            markers.push(crate::providers::media::marker_for_file(
                path,
                mime_type.as_deref(),
            ));
        }
    }
    markers.join(" ")
}

/// Deterministic, LLM-free fold of a message slice into a compact text body.
/// Preserves user turns, assistant text (or tool names), short tool results,
/// and media path markers from File parts.
fn hard_fold_history(msgs: &[ChatMessage]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(
        "[HARD FOLD] Summarizer skipped or failed; deterministic history fold. \
         Do NOT invent user tasks from this fold; only continue work the user \
         explicitly requested in clear text."
            .to_string(),
    );

    let mut user_n = 0usize;
    let mut tool_n = 0usize;
    let mut asst_n = 0usize;

    for m in msgs {
        match m.role.as_str() {
            "user" => {
                user_n += 1;
                let text = m.text_content();
                let media = media_markers_for_hard_fold(m);
                // Prior compaction markers are re-injected separately when present.
                if text.starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]") {
                    lines.push(format!(
                        "U{user_n}: [prior compaction summary, {} chars]",
                        text.chars().count()
                    ));
                    continue;
                }
                let body = user_text_for_hard_fold(&text);
                let line = match (body.is_empty(), media.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => media,
                    (false, true) => body,
                    (false, false) => format!("{media} {body}"),
                };
                lines.push(format!("U{user_n}: {line}"));
            }
            "assistant" => {
                asst_n += 1;
                let text = m.text_content();
                let tools = m
                    .tool_calls
                    .as_ref()
                    .map(|tcs| {
                        tcs.iter()
                            .map(|tc| tc.name.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() && !tools.is_empty() {
                    lines.push(format!("A{asst_n}: [tools: {tools}]"));
                } else if !tools.is_empty() {
                    lines.push(format!(
                        "A{asst_n}: {} [tools: {tools}]",
                        truncate_chars(text.trim(), HARD_MSG_CHARS)
                    ));
                } else {
                    lines.push(format!(
                        "A{asst_n}: {}",
                        truncate_chars(text.trim(), HARD_MSG_CHARS)
                    ));
                }
            }
            "tool" => {
                tool_n += 1;
                let name = m.name.as_deref().unwrap_or("tool");
                let id = m.tool_call_id.as_deref().unwrap_or("?");
                let body = m.text_content();
                let err = m.is_error.unwrap_or(false);
                let flag = if err { " ERR" } else { "" };
                lines.push(format!(
                    "T{tool_n}{flag} {name}#{id}: {}",
                    truncate_chars(body.trim(), HARD_TOOL_CHARS)
                ));
            }
            other => {
                lines.push(format!(
                    "{other}: {}",
                    truncate_chars(m.text_content().trim(), HARD_MSG_CHARS)
                ));
            }
        }
    }

    lines.push(format!(
        "\n## Counts\nuser={user_n} assistant={asst_n} tool={tool_n} total={}",
        msgs.len()
    ));

    let mut out = lines.join("\n");
    if out.chars().count() > HARD_TOTAL_CHARS {
        // Keep head + tail so early user goal and late tool results survive.
        let head_budget = HARD_TOTAL_CHARS * 2 / 3;
        let tail_budget = HARD_TOTAL_CHARS / 3;
        let head: String = out.chars().take(head_budget).collect();
        let total_chars = out.chars().count();
        let tail: String = out
            .chars()
            .skip(total_chars.saturating_sub(tail_budget))
            .collect();
        out = format!("{head}\n…\n{tail}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_range_without_existing_summary_compacts_prefix() {
        let history = vec![
            ChatMessage::user_text("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user_text("u2"),
        ];

        let (replace_start, compact_start, compact_end, existing) =
            find_incremental_range(&history, 2);

        assert_eq!(replace_start, 0);
        assert_eq!(compact_start, 0);
        assert_eq!(compact_end, 2);
        assert!(existing.is_none());
    }

    #[test]
    fn incremental_range_excludes_existing_summary_from_new_content() {
        let history = vec![
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] previous"),
            ChatMessage::user_text("new user"),
            ChatMessage::assistant_text("new assistant"),
            ChatMessage::user_text("retained"),
        ];

        let (replace_start, compact_start, compact_end, existing) =
            find_incremental_range(&history, 3);

        assert_eq!(replace_start, 0);
        assert_eq!(compact_start, 1);
        assert_eq!(compact_end, 3);
        assert_eq!(
            existing.as_deref(),
            Some("[CONTEXT COMPACTION — REFERENCE ONLY] previous")
        );
    }

    #[test]
    fn incremental_range_detects_no_new_content_after_existing_summary() {
        let history = vec![
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] previous"),
            ChatMessage::user_text("retained"),
        ];

        let (replace_start, compact_start, compact_end, existing) =
            find_incremental_range(&history, 1);

        assert_eq!(replace_start, 0);
        assert_eq!(compact_start, 1);
        assert_eq!(compact_end, 1);
        assert!(existing.is_some());
    }

    #[test]
    fn empty_compact_error_detected() {
        let err = anyhow::anyhow!("no content to compact");
        assert!(is_empty_compact_error(&err));
        let other = anyhow::anyhow!("summarize failed");
        assert!(!is_empty_compact_error(&other));
    }

    /// xiaoliu-class failure: one user + many tool rounds after a prior summary
    /// makes every work unit share user_start → incremental range is empty.
    #[test]
    fn single_user_tool_chain_incremental_range_is_empty() {
        let mut history = vec![
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior work"),
            ChatMessage::user_text("中兴在互联网有多大份额？"),
        ];
        for i in 0..20 {
            let mut a = ChatMessage::assistant_text("");
            a.tool_calls = Some(vec![ToolCall {
                id: format!("c{i}"),
                name: "web_search".into(),
                arguments: format!(r#"{{"query":"q{i}"}}"#),
            }]);
            history.push(a);
            let mut t = ChatMessage::text("tool", format!("result {i}"));
            t.tool_call_id = Some(format!("c{i}"));
            history.push(t);
        }

        // Boundary pinned at the sole real user (index 1) — nothing after the
        // existing summary and before boundary is compactable.
        let boundary = 1;
        let (replace_start, compact_start, compact_end, existing) =
            find_incremental_range(&history, boundary);
        assert_eq!(replace_start, 0);
        assert_eq!(compact_start, 1);
        assert_eq!(compact_end, 1);
        assert!(existing.is_some());
        assert!(history[compact_start..compact_end].is_empty());

        // Full-fold boundary = len has content.
        let (rs, cs, ce, _) = find_incremental_range(&history, history.len());
        assert_eq!(rs, 0);
        assert_eq!(cs, 1);
        assert_eq!(ce, history.len());
        assert!(!history[cs..ce].is_empty());
    }

    #[test]
    fn hard_fold_preserves_user_and_tool_names() {
        let mut a = ChatMessage::assistant_text("looking up");
        a.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "web_search".into(),
            arguments: "{}".into(),
        }]);
        let mut t = ChatMessage::text("tool", "ZTE share is X%");
        t.tool_call_id = Some("c1".into());
        let history = vec![
            ChatMessage::user_text("中兴份额?"),
            a,
            t,
            ChatMessage::assistant_text("share is X%"),
        ];
        let fold = hard_fold_history(&history);
        assert!(fold.contains("中兴份额"));
        assert!(fold.contains("web_search"));
        assert!(fold.contains("ZTE share") || fold.contains("share is X%"));
        assert!(fold.contains("user=1"));
        assert!(fold.chars().count() < HARD_TOTAL_CHARS);
        assert!(
            fold.contains("Do NOT invent user tasks"),
            "hard-fold banner must discourage fake tasks, got: {fold}"
        );
    }

    #[test]
    fn summarizer_prompt_forbids_invented_tasks() {
        let p = build_summarizer_prompt(12, None);
        assert!(p.contains("do NOT invent user tasks") || p.contains("NEVER invent"));
        assert!(p.contains("继续") || p.contains("continue"));
        let merge = build_summarizer_prompt(3, Some("old summary"));
        assert!(merge.contains("NEVER invent") || merge.contains("do NOT invent"));
    }

    #[test]
    fn hard_fold_respects_total_budget() {
        let mut history = Vec::new();
        history.push(ChatMessage::user_text("goal ".repeat(200)));
        for i in 0..200 {
            history.push(ChatMessage::assistant_text(format!(
                "assistant blob {i} {}",
                "x".repeat(300)
            )));
            let mut t = ChatMessage::text("tool", format!("tool blob {i} {}", "y".repeat(300)));
            t.tool_call_id = Some(format!("c{i}"));
            history.push(t);
        }
        let fold = hard_fold_history(&history);
        // Allow small overhead for the head/tail splice marker.
        assert!(
            fold.chars().count() <= HARD_TOTAL_CHARS + 8,
            "fold len {}",
            fold.chars().count()
        );
        assert!(fold.contains("goal"));
    }

    #[test]
    fn hard_fold_then_apply_leaves_single_user_message() {
        // Simulates last-resort path: hard fold entire completed history → apply.
        let mut history = vec![ChatMessage::user_text("q")];
        for i in 0..10 {
            let mut a = ChatMessage::assistant_text("");
            a.tool_calls = Some(vec![ToolCall {
                id: format!("c{i}"),
                name: "web_search".into(),
                arguments: "{}".into(),
            }]);
            history.push(a);
            let mut t = ChatMessage::text("tool", "r");
            t.tool_call_id = Some(format!("c{i}"));
            history.push(t);
        }
        assert!(open_user_protected_from(&history).is_none());
        let body = hard_fold_history(&history);
        let summary = format!("[CONTEXT COMPACTION — REFERENCE ONLY] {body}");
        // After replace, only one user message remains — legal for any provider.
        let mut after = history.clone();
        let end = after.len();
        after.drain(0..end);
        after.insert(0, ChatMessage::user_text(summary));
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].role, "user");
        assert!(after[0]
            .text_content()
            .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]"));
    }

    #[test]
    fn open_user_protected_from_trailing_user() {
        let history = vec![
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior"),
            ChatMessage::assistant_text("done"),
            ChatMessage::user_text("new request"),
        ];
        assert_eq!(open_user_protected_from(&history), Some(2));
    }

    #[test]
    fn open_user_protected_from_trailing_user_streak() {
        // attachment system-reminder user + real user at tail
        let history = vec![
            ChatMessage::assistant_text("old"),
            ChatMessage::user_text("<system-reminder>\n## Skills\n</system-reminder>"),
            ChatMessage::user_text("actual question"),
        ];
        assert_eq!(open_user_protected_from(&history), Some(1));
    }

    #[test]
    fn open_user_protected_from_completed_turn_is_none() {
        let history = vec![
            ChatMessage::user_text("q"),
            ChatMessage::assistant_text("a"),
        ];
        assert_eq!(open_user_protected_from(&history), None);
    }

    #[test]
    fn hard_fold_strips_system_reminder_keeps_body() {
        let reminder = "<system-reminder>\n## Memory\n- lots of injected memory text that would burn the 400 char budget if kept first\n".repeat(5);
        let text = format!(
            "{reminder}</system-reminder>\n现在的memory没有和原始消息内容建立关联 这个问题需要解决"
        );
        let history = vec![ChatMessage::user_text(text)];
        let fold = hard_fold_history(&history);
        assert!(
            fold.contains("memory没有和原始消息"),
            "fold must keep user body, got: {fold}"
        );
        assert!(
            !fold.contains("<system-reminder>"),
            "fold should not keep raw system-reminder tags"
        );
    }

    #[test]
    fn hard_fold_preserves_image_file_marker() {
        // Pure-image user turn: text_content() is empty; marker must still appear
        // so view_image can find the path after hard-fold.
        let path = "sessions/5687fcde/files/image-3e8610fd.png";
        let msg = ChatMessage {
            role: "user".into(),
            parts: vec![ContentPart::File {
                path: path.into(),
                mime_type: Some("image/png".into()),
                name: Some("shot.png".into()),
                size_bytes: Some(829_062),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
            model: None,
            usage: None,
        };
        assert!(msg.text_content().is_empty());
        let fold = hard_fold_history(&[msg]);
        assert!(
            fold.contains(&format!("[图片: {path}]")),
            "hard-fold must keep image marker, got: {fold}"
        );
    }

    #[test]
    fn hard_fold_image_plus_text_keeps_both() {
        let path = "sessions/x/files/a.png";
        let msg = ChatMessage {
            role: "user".into(),
            parts: vec![
                ContentPart::File {
                    path: path.into(),
                    mime_type: Some("image/png".into()),
                    name: None,
                    size_bytes: Some(1000),
                },
                ContentPart::Text {
                    text: "这是什么".into(),
                },
            ],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
            model: None,
            usage: None,
        };
        let fold = hard_fold_history(&[msg]);
        assert!(fold.contains(&format!("[图片: {path}]")), "got: {fold}");
        assert!(fold.contains("这是什么"), "got: {fold}");
    }

    #[test]
    fn full_fold_range_stops_before_open_user() {
        // Repro of 14f4fba2: summary + long chain + new open user.
        // fold_end must be the open user index, not history.len().
        let mut history = vec![
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior work"),
            ChatMessage::user_text("old question"),
        ];
        for i in 0..5 {
            let mut a = ChatMessage::assistant_text("");
            a.tool_calls = Some(vec![ToolCall {
                id: format!("c{i}"),
                name: "web_search".into(),
                arguments: "{}".into(),
            }]);
            history.push(a);
            let mut t = ChatMessage::text("tool", format!("result {i}"));
            t.tool_call_id = Some(format!("c{i}"));
            history.push(t);
        }
        history.push(ChatMessage::user_text(
            "现在的memory没有和原始消息内容建立关联 这个问题需要解决",
        ));

        let protected = open_user_protected_from(&history).expect("open user");
        assert_eq!(protected, history.len() - 1);

        // Simulate full-fold apply: compact [0, protected), leave open user.
        let body = hard_fold_history(&history[..protected]);
        let summary = format!("[CONTEXT COMPACTION — REFERENCE ONLY] {body}");
        let mut after = history.clone();
        after.drain(0..protected);
        after.insert(0, ChatMessage::user_text(summary));

        assert!(
            after.len() >= 2,
            "must keep summary + open user, got len={}",
            after.len()
        );
        assert!(after[0]
            .text_content()
            .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]"));
        let last = after.last().unwrap();
        assert_eq!(last.role, "user");
        assert!(
            last.text_content()
                .contains("memory没有和原始消息内容建立关联"),
            "open user body must survive verbatim: {}",
            last.text_content()
        );
    }

    #[test]
    fn empty_incremental_with_open_user_does_not_claim_full_len() {
        // summary + unanswered user: protect the open user, not the summary.
        let history = vec![
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior"),
            ChatMessage::user_text("unanswered"),
        ];
        let protected = open_user_protected_from(&history).unwrap();
        assert_eq!(protected, 1);
        let (_rs, cs, ce, _) = find_incremental_range(&history, protected);
        // Only the summary sits before protected; incremental slice is empty.
        assert!(cs >= ce || history[cs..ce].is_empty());
        // Hard-fold of prefix leaves the open user after apply.
        let mut after = history.clone();
        after.drain(0..protected);
        after.insert(
            0,
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] folded"),
        );
        assert_eq!(after.len(), 2);
        assert_eq!(after[1].text_content(), "unanswered");
    }

    #[test]
    fn open_user_skips_compaction_summary_in_trailing_streak() {
        let history = vec![
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] only summary"),
        ];
        assert_eq!(open_user_protected_from(&history), None);
    }
}

fn strip_images(msg: &ChatMessage) -> ChatMessage {
    let mut cleaned = msg.clone();
    cleaned.parts = cleaned
        .parts
        .into_iter()
        .map(|part| match part {
            ContentPart::File {
                path, mime_type, ..
            } => ContentPart::Text {
                text: crate::providers::media::marker_for_file(&path, mime_type.as_deref()),
            },
            other => other,
        })
        .collect();
    cleaned
}

fn build_summarizer_prompt(msg_count: usize, existing_summary: Option<&str>) -> String {
    // Anti-hallucination rules shared by merge and fresh summary paths.
    // Prevents fabricating active user tasks from compressed/dirty history
    // (root cause of pure-image turns being rewritten as full Villagers-style quests).
    const ANTI_FAKE_TASK_RULES: &str = "\
CRITICAL — do NOT invent user tasks:\n\
- NEVER invent, reconstruct, or complete a user request that was not explicitly present\n\
  in the messages being summarized. If the latest user input is only an image marker,\n\
  a bare word like \"继续\"/\"continue\", a system-reminder, or incomplete fragments,\n\
  do NOT invent a full task for the agent to execute.\n\
- Conversation State / Pending may only list work the user EXPLICITLY asked for in\n\
  clear natural-language text. Prefer OMIT over guess.\n\
- Image-only or media-marker turns are NOT open tasks unless the user also wrote what\n\
  to do with them.\n\
- Do NOT promote items found only in prior compaction summaries, tool chatter, or\n\
  assistant speculation into active user tasks.\n\
- If uncertain whether work is still open, put a short note under Pending as\n\
  \"[Pending] unclear — user should restate\" rather than inventing steps.\n";

    match existing_summary {
        Some(base) => format!(
            "Below is a PREVIOUS SUMMARY followed by NEW conversation messages.\n\
             \n\
             === PREVIOUS SUMMARY ===\n{base}\n\
             === END PREVIOUS SUMMARY ===\n\
             \n\
             Merge the new messages into the previous summary. Produce a single \n\
             updated summary that covers everything.\n\
             \n\
             Output the summary as plain text with the following sections. \n\
             Include the first section ONLY if there is genuinely unfinished, user-requested work \n\
             that the model MUST actively continue. If the user's last request was completed or \n\
             the topic has moved on, OMIT it entirely.\n\
             \n\
             ## Conversation State (OPTIONAL — omit if nothing is actively in progress)\n\
             Only include genuinely unfinished work the user explicitly asked for. \n\
             Do NOT include completed tasks, background context, or system status here.\n\
             \n\
             ## Key Decisions\n\
             Important choices made and why.\n\
             \n\
             ## Technical Context\n\
             Files modified, code locations, APIs used, configurations changed.\n\
             \n\
             ## Resolved\n\
             Tasks/questions that were completed or answered.\n\
             \n\
             ## Pending\n\
             Tasks/questions still open or deferred.\n\
             \n\
             ## Errors & Fixes\n\
             Problems encountered and their solutions.\n\
             \n\
             ## Evidence Index\n\
             Compact evidence needed for later verification: file paths with line numbers where available, commands run, commit hashes, CI/run IDs, artifact paths, log paths, versions, PIDs, and deployment targets.\n\
             \n\
             Rules:\n\
             - Mark resolved items clearly (prefix with [Resolved])\n\
             - Mark pending items clearly (prefix with [Pending])\n\
             - Omit raw tool output (large code blocks, logs, file contents)\n\
             - Use the same language as the conversation\n\
             - Be thorough but concise: every important detail should be preserved\n\
             - Each fact appears in exactly ONE section only — the most appropriate one. Never repeat the same information across Conversation State / Key Decisions / Resolved / Pending. If something was completed, it belongs in Resolved, not in Conversation State.\n\
             \n\
             {ANTI_FAKE_TASK_RULES}"
        ),
        None => format!(
            "Summarize the conversation history above. This summary will replace \
             the full history, so it MUST preserve all information needed to continue \
             the conversation seamlessly.\n\
             \n\
             Output the summary as plain text.\n\
             \n\
             Required sections:\n\
             1. **Conversation State (OPTIONAL)**: Only include if there is genuinely unfinished, user-requested work in progress. Omit entirely if the user's requests have been completed.\n\
             2. **Key Decisions**: Important choices made and why.\n\
             3. **Technical Context**: Files modified, code locations, APIs used, configurations changed.\n\
             4. **Errors & Fixes**: Problems encountered and their solutions.\n\
             5. **Pending Work**: What still needs to be done. Only user-explicit open work; omit if none.\n\
             6. **Evidence Index**: Compact verification evidence such as file paths with line numbers where available, commands run, commit hashes, CI/run IDs, artifact paths, log paths, versions, PIDs, and deployment targets.\n\
             \n\
             Rules:\n\
             - Omit raw tool output (large code blocks, logs, file dumps) — keep only key facts\n\
             - Use the same language as the conversation\n\
             - Be thorough: losing context means the user has to repeat themselves\n\
             - This conversation has {msg_count} messages to summarize\n\
             \n\
             {ANTI_FAKE_TASK_RULES}"
        ),
    }
}

fn append_evidence_index(
    mut summary: String,
    messages: &[ChatMessage],
    compact_start: usize,
    compact_end: usize,
    boundary: usize,
    model_id: &str,
) -> String {
    let paths = extract_file_paths(messages);
    let commands = extract_shell_commands(messages);
    let commits = extract_commit_hashes(messages);
    let runs = extract_ci_runs(messages);

    let has_section = summary.contains("## Evidence Index") || summary.contains("Evidence Index");
    if !has_section {
        summary.push_str("\n\n## Evidence Index\n");
    } else {
        summary.push_str("\n\nAdditional evidence captured by compaction:\n");
    }
    summary.push_str(&format!(
        "- Compaction range: compact_start={}, compact_end={}, boundary={}, messages_compacted={}, model={}\n",
        compact_start,
        compact_end,
        boundary,
        messages.len(),
        model_id
    ));
    if !paths.is_empty() {
        summary.push_str(&format!(
            "- File paths: {}\n",
            paths
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !commands.is_empty() {
        summary.push_str(&format!(
            "- Commands: {}\n",
            commands
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    if !commits.is_empty() {
        summary.push_str(&format!(
            "- Commit hashes: {}\n",
            commits
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !runs.is_empty() {
        summary.push_str(&format!(
            "- CI/run IDs: {}\n",
            runs.iter().take(20).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    summary
}

fn extract_shell_commands(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r#"(?s)"command"\s*:\s*"([^"]{1,240})""#).unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut commands = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(1) {
                let value = m.as_str().replace("\\n", " ");
                if seen.insert(value.clone()) {
                    commands.push(value);
                }
            }
        }
    }
    commands
}

fn extract_commit_hashes(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut hashes = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(0) {
                let value = m.as_str().to_string();
                if seen.insert(value.clone()) {
                    hashes.push(value);
                }
            }
        }
    }
    hashes
}

fn extract_ci_runs(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(?:run id|run_id|workflow run|CI run)[:= ]+([0-9]{5,})\b")
            .unwrap()
    });
    let mut seen = std::collections::HashSet::new();
    let mut runs = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(1) {
                let value = m.as_str().to_string();
                if seen.insert(value.clone()) {
                    runs.push(value);
                }
            }
        }
    }
    runs
}

fn audit_summary_quality(to_compact: &[ChatMessage], summary: &str) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();

    if summary.chars().count() < 100 {
        reasons.push(format!(
            "summary too short: {} chars (minimum 100)",
            summary.chars().count()
        ));
    }

    let original_paths = extract_file_paths(to_compact);
    if !original_paths.is_empty() {
        let preserved = original_paths
            .iter()
            .filter(|p| summary.contains(*p))
            .count();
        if preserved == 0 && original_paths.len() <= 5 {
            reasons.push(format!(
                "no file paths preserved (original had {})",
                original_paths.len()
            ));
        }
    }

    (reasons.is_empty(), reasons)
}

fn extract_file_paths(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"(?:/[\w/.-]+\.\w{1,5})|(?:src/[\w/.-]+)").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(0) {
                let p = m.as_str().to_string();
                if seen.insert(p.clone()) {
                    paths.push(p);
                }
            }
        }
    }
    paths
}
