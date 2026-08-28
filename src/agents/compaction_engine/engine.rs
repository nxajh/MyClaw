use std::sync::Arc;

use futures_util::StreamExt;

use crate::agents::resource_provider::ResourceProvider;
use crate::scheduling_runtime::work_unit;
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

use super::evidence::append_evidence_index;
use super::fold::HARD_PRIOR_SUMMARY_CHARS;
use super::fold::{find_incremental_range, hard_fold_history, is_empty_compact_error};
use super::fold::{open_user_protected_from, strip_images, truncate_chars};
use super::summarizer::{audit_summary_quality, build_summarizer_prompt, SummaryResponse};

/// Result returned by `CompactionEngine::execute_compaction`.
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
pub struct CompactionEngine {
    compact_threshold: f64,
    retain_work_units: usize,
    registry: Arc<dyn ProviderRegistry>,
    resources: Arc<ResourceProvider>,
    memory_executor: MemoryToolExecutor,
    max_rounds: usize,
}

#[allow(dead_code)] // some accessors retained for /compact + future callers
impl CompactionEngine {
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

