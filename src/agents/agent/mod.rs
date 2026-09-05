//! `Agent` — "what the agent *is*" (its config) separated from "what it
//! has access to" (the [`AgentRuntime`]).
//!
//! `Agent::run` is the orchestrator's per-turn entry point. It drives the
//! LLM stream, executes tool calls, applies the per-turn loop-breaker,
//! performs context compaction via [`CompactionEngine`] when the token count
//! crosses the threshold, and persists history after each step. When the
//! session has a streaming channel attached, per-chunk `TurnEvent`s
//! (`Chunk` / `Thinking` / `ToolCall` / `ToolResult`) are pushed to the
//! optional `TurnStream`.

use anyhow::Result;

mod exec_marker;
mod finalize;
mod injections;
mod retry;
mod run_recovery;
mod stream_collect;
mod tool_filter;
mod tool_phase;
mod turn_state;

#[cfg(test)]
pub(crate) mod tests;

use crate::agents::AgentRuntime;

pub(crate) use tool_filter::fold_absent_tool;

use exec_marker::{last_user_text, persist_last};
use retry::chat_with_retry;
use stream_collect::push_or_drop;
use tool_filter::{filter_modality_redundant_tools, filter_turn_scoped_tools};
use finalize::OverflowRecovery;
use tool_phase::ToolBatchOutcome;
use turn_state::TurnState;

use crate::agents::session::Session;
use crate::agents::turn::{TurnContext, TurnResult};
use crate::api::turn_event::TurnEvent;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::capability_chat::{ChatMessage, StopReason, ToolSpec};
use crate::providers::Capability;
use tokio::sync::OwnedMutexGuard;

// ── Module map ──────────────────────────────────────────────────────────────
// mod.rs            Agent identity + run/run_inner orchestration
// exec_marker.rs    persist_last + exec-marker trio + ExecMarkerGuard + last_user_text + llm_usage
// tool_filter.rs    allowed_tools + filter_turn_scoped_tools + modality filters + fold_absent_tool
// stream_collect.rs CollectedResponse + push_or_drop + collect_stream
// injections.rs     reinject_after_compaction + inject_per_round (transient <system-reminder> injections)
// retry.rs          is_transient_llm_error + backoff_duration + chat_with_retry
// turn_state.rs     TurnState: loop-carried retry counters + turn flags (+ has_pending)
// finalize.rs       finalize_turn: Done event → persist → memory/skill forks → TurnResult (batch 4)
// tool_phase.rs     execute_tool_batch: tool-batch for-loop + 4 flow-control detectors + ToolBatchOutcome (batch 4)
// run_recovery.rs   run_recovery: resume mid-turn history (3 cases, exec-marker guard) (batch 6)

/// "An agent" — just its config (name, system prompt fragment, three
/// capability filters, optional model override). Everything else lives
/// on `AgentRuntime`.
///
/// Named `Agent` while the legacy `agent_impl::Agent` (factory for
/// `AgentLoop`) still exists. H45 deletes that, at which point this
/// type takes the `Agent` name per RFC v2.
#[derive(Clone)]
pub struct Agent {
    pub config: SubAgentConfig,
}

impl Agent {
    pub fn new(config: SubAgentConfig) -> Self {
        Self { config }
    }

    /// Run one user turn. Mutates `session.history` and persists via
    /// `session.persist` if set; returns the assistant's final text.
    ///
    /// The user message is expected to already be in `session.history`
    /// (caller's responsibility — matches RFC §三.A where SessionContext
    /// does pre-turn bookkeeping). This entrypoint only drives the
    /// LLM ↔ tool loop.
    ///
    /// v4 (RFC inbound-spool §6.4): runs `run_recovery` first as a safety
    /// net — if the history is mid-turn (a crash interrupted the previous
    /// turn before it was dispatched), the recovery finishes that turn and
    /// its result is returned instead of running the LLM on malformed
    /// history. When no recovery is needed, delegates to [`Self::run_inner`].
    pub async fn run(
        &self,
        session: &mut OwnedMutexGuard<Session>,
        turn_guard: &mut OwnedMutexGuard<()>,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<TurnResult> {
        if let Some(tr) = self
            .run_recovery(session, turn_guard, turn_ctx.clone(), runtime)
            .await?
        {
            return Ok(tr);
        }
        self.run_inner(session, turn_guard, turn_ctx, runtime).await
    }

    /// The raw LLM ↔ tool loop (no recovery pre-check). Called by [`Self::run`]
    /// after recovery finds nothing to do, and by `run_recovery`'s tail for
    /// Cases B/C. Never calls back into [`Self::run`] — that would recurse
    /// through `run_recovery` indefinitely.
    async fn run_inner(
        &self,
        session: &mut OwnedMutexGuard<Session>,
        turn_guard: &mut OwnedMutexGuard<()>,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<TurnResult> {
        // Turn setup (tool view, provider+model, loop breaker, token tracker
        // seed, request prefix, spec conversion, orphan-tool folding) —
        // extracted to `prepare_turn` below (batch 4).
        let (provider, model_id, allowed_tools, tool_specs, mut messages, mut loop_breaker) =
            self.prepare_turn(session, &turn_ctx, runtime).await?;

        // Shared CompactionEngine singleton — RFC v2 target shape. Token
        // tracking lives solely on `Session.token_tracker`; CompactionEngine
        // only carries threshold/retain_units + summarizer state.
        let context = &runtime.context_engine;
        // Loop-carried turn state (see `turn_state::TurnState`).
        let mut turn_state = TurnState::default();
        const MAX_EMPTY_RETRIES: usize = 3;
        const MAX_OVERFLOW_RETRIES: usize = 3;

        loop {
            // Shutdown checkpoint between LLM calls (mirrors AgentLoop chat_loop).
            if crate::is_shutting_down() {
                return Ok(TurnResult {
                    text: String::new(),
                    stop_reason: StopReason::EndTurn,
                    pending_retry: None,
                    has_pending: false,
                });
            }

            // User-cancel checkpoint: if the session's TurnStream surfaces a
            // CancellationToken, honor it. RFC §7.6 (Phase 1.5): cancel
            // lives on the per-turn stream now, not on Channel directly.
            if let Some(token) = session.turn_stream.as_ref().and_then(|s| s.cancel_token()) {
                if token.is_cancelled() {
                    return Ok(TurnResult {
                        text: String::new(),
                        stop_reason: StopReason::EndTurn,
                        pending_retry: None,
                        has_pending: false,
                    });
                }
            }

            // User-cancel checkpoint (non-streaming channels): session.cancel_token
            // is set by process_turn and triggered by `/stop`. Works for all
            // channels regardless of TurnStream support.
            if let Some(ref token) = session.cancel_token {
                if token.is_cancelled() {
                    return Ok(TurnResult {
                        text: String::new(),
                        stop_reason: StopReason::EndTurn,
                        pending_retry: None,
                        has_pending: false,
                    });
                }
            }

            // Pre-send compaction guard + post-compaction re-injection and
            // the per-round transient injections — extracted to injections.rs (batch 3).
            messages = self
                .reinject_after_compaction(
                    session,
                    messages,
                    runtime,
                    &turn_ctx,
                    &provider,
                    &model_id,
                    &tool_specs,
                )
                .await;

            self.inject_per_round(session, &mut messages).await;

            // LLM request with same-model transient-error retry —
            // extracted to retry.rs as `chat_with_retry` (batch 3).
            //
            // `session` is `&mut OwnedMutexGuard<Session>` here: field access
            // goes through `Deref`/`DerefMut` trait calls, so the borrow
            // checker can't prove `&session.id` and `&mut session.turn_stream`
            // below are disjoint within one call expression (it can for a
            // plain `&mut Session`, where field projection is direct). A
            // single reborrow into a plain `&mut Session` restores that.
            let response = {
                let session: &mut Session = session;
                chat_with_retry(
                    &session.id,
                    session.history.len(),
                    &provider,
                    &messages,
                    &tool_specs,
                    &model_id,
                    &turn_ctx,
                    &mut session.turn_stream,
                    runtime.user_registry.as_ref(),
                )
                .await?
            };

            // Update Session.token_tracker from the API response. The tracker is
            // the source of truth for *usage reporting* (display + persistence,
            // surviving restarts); it is intentionally NOT the input to the
            // compaction decision — `maybe_compact` sizes the request directly.
            if let Some(ref usage) = response.usage {
                let input = usage.input_tokens.unwrap_or(0);
                let output = usage.output_tokens.unwrap_or(0);
                let cached = usage.cached_input_tokens.unwrap_or(0);
                session
                    .token_tracker
                    .update_from_usage(input, output, cached);
                if let Some(ref hook) = session.persist {
                    hook.save_token_count(&session.id, session.token_tracker.total_tokens());
                }
            }

            // Context-overflow backstop — extracted to finalize.rs (batch 4).
            match self
                .handle_context_overflow(
                    session,
                    &turn_ctx,
                    &provider,
                    &model_id,
                    &tool_specs,
                    context,
                    runtime,
                    response.stop_reason == StopReason::ContextOverflow,
                    &mut turn_state,
                    MAX_OVERFLOW_RETRIES,
                )
                .await?
            {
                OverflowRecovery::Compacted(compacted) => {
                    messages = compacted;
                    continue;
                }
                OverflowRecovery::GiveUp(tr) => return Ok(tr),
                OverflowRecovery::NotOverflow => {}
            }
            // No tool calls → final response. Persist + return.
            if response.tool_calls.is_empty() {
                tracing::debug!(
                    model = %model_id,
                    stop = ?response.stop_reason,
                    text_len = response.text.len(),
                    has_reasoning = response.reasoning_content.is_some(),
                    reasoning_len = response.reasoning_content.as_ref().map(|s| s.len()).unwrap_or(0),
                    "no tool calls in response — treating as final"
                );
                if response.text.trim().is_empty() {
                    // Empty response: retry up to MAX_EMPTY_RETRIES like
                    // AgentLoop's chat_loop does. The provider sometimes
                    // returns empty on EndTurn / StopSequence (transient)
                    // or MaxTokens (output budget exhausted). We don't yet
                    // do the boosted-max_tokens retry; we just re-call.
                    turn_state.empty_response_retries += 1;
                    let tool_use_without_calls = response.stop_reason == StopReason::ToolUse;
                    if turn_state.empty_response_retries > MAX_EMPTY_RETRIES {
                        if tool_use_without_calls {
                            tracing::warn!(
                                model = %model_id,
                                session = %session.id,
                                tool_call_events = response.tool_call_events,
                                "tool_use stop without tool calls after retries, giving up"
                            );
                        } else {
                            tracing::warn!(
                                stop = ?response.stop_reason,
                                model = %model_id,
                                is_override = turn_ctx.model_id.is_some(),
                                "empty response after {} retries, giving up",
                                MAX_EMPTY_RETRIES
                            );
                        }
                        // Surface a user-visible notice instead of silent give-up.
                        // Override path mentions locked model + /model off; default
                        // routing keeps a shorter actionable message.
                        let msg = crate::agents::user_messages::msg_empty_response(
                            turn_ctx.model_id,
                        );
                        push_or_drop(
                            &mut session.turn_stream,
                            TurnEvent::Done {
                                text: msg.clone(),
                            },
                        )
                        .await;
                        session.add_assistant(msg.clone());
                        persist_last(session);
                        return Ok(TurnResult {
                            text: msg,
                            stop_reason: response.stop_reason,
                            pending_retry: Some(last_user_text(session)),
                            has_pending: false,
                        });
                    }
                    if tool_use_without_calls {
                        tracing::warn!(
                            model = %model_id,
                            session = %session.id,
                            attempt = turn_state.empty_response_retries,
                            tool_call_events = response.tool_call_events,
                            "tool_use stop without tool calls, retrying"
                        );
                    } else {
                        tracing::warn!(
                            attempt = turn_state.empty_response_retries,
                            stop = ?response.stop_reason,
                            "empty response, retrying"
                        );
                    }
                    // No Done yet — we will re-call the model.
                    continue;
                }
                // Final response path — extracted to finalize.rs (batch 4).
                // (Done event → defer_collapse → persist → memory/skill
                // forks → TurnResult; statement order per RFC §4.5.)
                return self
                    .finalize_turn(
                        session,
                        &response,
                        &messages,
                        &tool_specs,
                        runtime,
                        &model_id,
                        &provider,
                        &turn_state,
                        loop_breaker.total_calls(),
                    )
                    .await;
            }

            // Tool-batch execution — extracted to tool_phase.rs (batch 4).
            // (assistant append + persist → per-call loop with events, exec
            // marker, execute, 4 flow-control detectors, hard limit →
            // ToolBatchOutcome.)
            match self
                .execute_tool_batch(
                    session,
                    turn_guard,
                    &response,
                    &mut messages,
                    runtime,
                    &model_id,
                    &turn_ctx.permission_mode,
                    &allowed_tools,
                    &tool_specs,
                    &mut turn_state,
                    &mut loop_breaker,
                )
                .await?
            {
                ToolBatchOutcome::Continue => {}
                ToolBatchOutcome::EndTurn(tr) => return Ok(*tr),
                ToolBatchOutcome::Abort(err) => return Err(err.into()),
            }

            // Loop back to the next LLM call with the appended tool_result messages.
        }
    }

    /// One-time turn setup for `run_inner`: resolve the filtered tool view,
    /// provider + model, loop-breaker counter, seed the token tracker, and
    /// assemble the LLM request prefix (with orphan-tool folding).
    ///
    /// Extracted from `run_inner` (batch 4); statement order verbatim.
    async fn prepare_turn<'a>(
        &'a self,
        session: &mut Session,
        turn_ctx: &TurnContext<'_>,
        runtime: &'a AgentRuntime,
    ) -> Result<(
        std::sync::Arc<dyn crate::providers::ChatProvider>,
        String,
        Vec<std::sync::Arc<dyn crate::providers::Tool>>,
        Vec<ToolSpec>,
        Vec<ChatMessage>,
        crate::agents::loop_breaker::LoopBreakerCounter,
    )> {
        // Resolve filtered tool view from runtime + per-agent config.
        let mut allowed_tools = self.allowed_tools(runtime);
        filter_turn_scoped_tools(&mut allowed_tools, session);

        // Resolve provider + model.
        //
        // - No override → the Chat fallback wrapper (fans out across the
        //   configured chain on retryable / fallbackable errors).
        // - Override (`/model`) → the raw per-model provider only. Same-model
        //   short retries (short RateLimit / 5xx / timeout) may sleep and retry
        //   here. We intentionally NEVER fall back to a different model or to
        //   the routing Fallback chain — the user pinned this model. On
        //   Billing / long model_cooldown / Auth / ModelNotFound the turn fails
        //   with an actionable message (switch via `/model` or `/model off`).
        let (provider, model_id) = match turn_ctx.model_id {
            Some(m) => {
                let result = runtime
                    .providers
                    .get_chat_provider_by_model(m)
                    .ok_or_else(|| anyhow::anyhow!("model '{}' not found in registry", m))?;
                tracing::info!(
                    model = %result.1,
                    reason = "session_override",
                    "model resolved (no auto-degrade to fallback)"
                );
                result
            }
            None => {
                let result = runtime.providers.get_chat_provider(Capability::Chat)?;
                tracing::info!(
                    model = %result.1,
                    reason = "routing_default",
                    "model resolved"
                );
                result
            }
        };

        // Shared ToolExecutor singleton — per the target architecture,
        // the executor is stateless w.r.t. which tools the agent may
        // call; the `allowed_tools` slice computed above is passed
        // explicitly on every `execute` call. Consumed by
        // `execute_tool_batch` (tool_phase.rs, batch 4).

        // Per-turn loop breaker counter — allocated fresh each turn by
        // the shared `runtime.loop_breaker` singleton. Per-agent
        // `SubAgentConfig.max_tool_calls` overrides the runtime default;
        // when None, the shared config wins.
        let loop_breaker = match self.config.max_tool_calls {
            Some(n) => runtime.loop_breaker.new_counter_with_max(n),
            None => runtime.loop_breaker.new_counter(),
        };

        // Seed Session.token_tracker from history when fresh (display/usage only).
        // The compaction decision does NOT read the tracker — it uses a direct
        // per-request estimate in `maybe_compact` — so a stale tracker restored on
        // restart can no longer suppress compaction.
        if session.token_tracker.is_fresh() {
            session
                .token_tracker
                .seed_from_history(turn_ctx.system_prompt, &session.history);
        }

        // Assemble the LLM request prefix once. Subsequent rebuilds re-clone
        // the session's growing history.
        let system_msg = ChatMessage::system_text(turn_ctx.system_prompt);
        let mut messages: Vec<ChatMessage> = std::iter::once(system_msg.clone())
            .chain(session.history.iter().cloned())
            .collect();
        crate::agents::session::sanitize_history(&mut messages);

        if let Some(policy) = runtime.providers.get_chat_media_policy(&model_id) {
            filter_modality_redundant_tools(&mut allowed_tools, &messages, policy, &model_id);
        }
        // Convert capability_tool::ToolSpec → capability_chat::ToolSpec
        // (the LLM request type). Same fields, different module homes —
        // a unification candidate for a separate cleanup.
        let tool_specs: Vec<ToolSpec> = allowed_tools
            .iter()
            .map(|t| {
                let s = t.spec();
                ToolSpec {
                    name: s.name,
                    description: Some(s.description),
                    input_schema: s.parameters,
                }
            })
            .collect();

        // Loop-carried turn state (see `turn_state::TurnState`): retry
        // counters (bounded by MAX_EMPTY_RETRIES / MAX_OVERFLOW_RETRIES)
        // and the turn-scoped flags threaded through the loop body.
        // Send tools are not always declared (loaded on demand), so fold any
        // prior references that would become orphan tool calls.
        fold_absent_tool(&mut messages, &tool_specs, "send_message", "消息发送结果");
        fold_absent_tool(&mut messages, &tool_specs, "send_media", "媒体发送结果");

        Ok((
            provider,
            model_id,
            allowed_tools,
            tool_specs,
            messages,
            loop_breaker,
        ))
    }

}
