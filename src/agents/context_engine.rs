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
use crate::agents::tokens::{estimate_history_tokens, estimate_message_tokens, estimate_tokens};
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
    /// (`estimate_history_tokens`) rather than the token tracker, so a stale or
    /// under-counted tracker can't let an over-window request slip through. Returns
    /// `None` when no compaction was needed (`!force`) or none was possible (no
    /// boundary / summarizer error).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn maybe_compact(
        &self,
        session: &mut Session,
        system_prompt: &str,
        model_id: &str,
        provider: Arc<dyn ChatProvider>,
        tool_specs: &[ToolSpec],
        task_state: Option<&Arc<tokio::sync::RwLock<crate::tools::TaskState>>>,
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
        let estimate = estimate_history_tokens(system_prompt, &session.history);
        tracing::debug!(
            model_id,
            estimate,
            window,
            threshold = (window as f64 * self.compact_threshold) as u64,
            force,
            history_len = session.history.len(),
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
        let boundary = self.compaction_boundary_with_retain(
            &session.history,
            window,
            sys_prompt_tokens,
            tool_spec_tokens,
            override_retain,
        )?;

        // Snapshot history for the summarizer (which reads a slice); the live
        // session is passed through for the memory tools inside the summarizer.
        let history_snap: Vec<ChatMessage> = session.history.to_vec();
        match self
            .execute_compaction(
                &history_snap,
                system_prompt,
                tool_specs,
                boundary,
                model_id,
                Arc::clone(&provider),
                session,
            )
            .await
        {
            Ok(result) => {
                let version = session.compact_version + 1;
                let summary_prefix = "[CONTEXT COMPACTION — REFERENCE ONLY] ";

                // Inject active task/goal state so the model retains its plan
                // across context compaction.
                let task_injection = if let Some(task_state) = task_state {
                    let state = task_state.read().await;
                    state.format_for_injection()
                } else {
                    None
                };

                let summary_text = match &task_injection {
                    Some(tasks) => format!("{}{}\n\n{}", summary_prefix, result.summary, tasks),
                    None => format!("{}{}", summary_prefix, result.summary),
                };
                let summary_msg = ChatMessage::user_text(summary_text);
                let last_compacted_id = session
                    .message_ids
                    .get(boundary.saturating_sub(1))
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
                    for (i, id) in session.message_ids.iter_mut().enumerate() {
                        *id = (i + 1) as i64;
                    }
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
        task_state: Option<&Arc<tokio::sync::RwLock<crate::tools::TaskState>>>,
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
                    task_state,
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

        // Last-resort fallback: force-drop oldest half when compaction fails.
        if latest.is_none() && force && session.history.len() > 6 {
            let drop_boundary = session.history.len() / 2;
            let version = session.compact_version + 1;
            let dropped_tokens: u64 = session.history[..drop_boundary]
                .iter()
                .map(estimate_message_tokens)
                .sum();
            tracing::warn!(
                session = %session.id,
                msg_count = session.history.len(),
                dropped = drop_boundary,
                dropped_tokens,
                "compaction exhausted all passes; force-dropping oldest half as last resort"
            );
            session.drop_pre_boundary(drop_boundary, version);
            session
                .token_tracker
                .adjust_for_compaction(dropped_tokens, 0);
            let messages = self.rebuild_messages(system_prompt, &session.history, tool_specs);
            latest = Some(messages);
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
    ) -> anyhow::Result<CompactionResult> {
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

        let summary = self
            .summarize(
                &to_compact,
                existing_summary.as_deref(),
                system_prompt,
                tool_specs,
                model_id,
                provider,
                session,
            )
            .await?;

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
             Compact evidence needed for later verification: file paths with line numbers where available, commands run, commit hashes, CI/run IDs, artifact paths, log paths, versions, PIDs, and deployment targets.\n\\
             \n\
             Rules:\n\
             - Mark resolved items clearly (prefix with [Resolved])\n\
             - Mark pending items clearly (prefix with [Pending])\n\
             - Omit raw tool output (large code blocks, logs, file contents)\n\
             - Use the same language as the conversation\n\
             - Be thorough but concise: every important detail should be preserved\n\
             - Each fact appears in exactly ONE section only — the most appropriate one. Never repeat the same information across Conversation State / Key Decisions / Resolved / Pending. If something was completed, it belongs in Resolved, not in Conversation State."
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
             5. **Pending Work**: What still needs to be done.\n\
             6. **Evidence Index**: Compact verification evidence such as file paths with line numbers where available, commands run, commit hashes, CI/run IDs, artifact paths, log paths, versions, PIDs, and deployment targets.\n\\
             \n\
             Rules:\n\
             - Omit raw tool output (large code blocks, logs, file dumps) — keep only key facts\n\
             - Use the same language as the conversation\n\
             - Be thorough: losing context means the user has to repeat themselves\n\
             - This conversation has {msg_count} messages to summarize"
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
