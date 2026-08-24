//! `Agent` — "what the agent *is*" (its config) separated from "what it
//! has access to" (the [`AgentRuntime`]).
//!
//! `Agent::run` is the orchestrator's per-turn entry point. It drives the
//! LLM stream, executes tool calls, applies the per-turn loop-breaker,
//! performs context compaction via [`ContextEngine`] when the token count
//! crosses the threshold, and persists history after each step. When the
//! session has a streaming channel attached, per-chunk `TurnEvent`s
//! (`Chunk` / `Thinking` / `ToolCall` / `ToolResult`) are pushed to the
//! optional `TurnStream`.

use anyhow::Result;

mod exec_marker;
mod finalize;
mod injections;
mod retry;
mod stream_collect;
mod tool_filter;
mod tool_phase;
mod turn_state;

#[cfg(test)]
mod tests;

use crate::agents::AgentRuntime;

pub(crate) use tool_filter::fold_absent_tool;

use exec_marker::{exec_marker_clear, exec_marker_read, last_user_text, persist_last};
use retry::chat_with_retry;
use stream_collect::push_or_drop;
use tool_filter::{filter_modality_redundant_tools, filter_turn_scoped_tools};
use finalize::OverflowRecovery;
use tool_phase::ToolBatchOutcome;
use turn_state::TurnState;

use crate::agents::session::Session;
use crate::agents::turn::{TurnContext, TurnResult};
use crate::agents::turn_event::TurnEvent;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::capability_chat::{ChatMessage, StopReason, ToolSpec};
use crate::providers::Capability;

// ── Module map ──────────────────────────────────────────────────────────────
// mod.rs            Agent identity + run/run_inner/run_recovery orchestration
// exec_marker.rs    persist_last + exec-marker trio + ExecMarkerGuard + last_user_text + llm_usage
// tool_filter.rs    allowed_tools + filter_turn_scoped_tools + modality filters + fold_absent_tool
// stream_collect.rs CollectedResponse + push_or_drop + collect_stream
// injections.rs     reinject_after_compaction + inject_per_round (transient <system-reminder> injections)
// retry.rs          is_transient_llm_error + backoff_duration + chat_with_retry
// turn_state.rs     TurnState: loop-carried retry counters + turn flags (+ has_pending)
// finalize.rs       finalize_turn: Done event → persist → memory/skill forks → TurnResult (batch 4)
// tool_phase.rs     execute_tool_batch: tool-batch for-loop + 4 flow-control detectors + ToolBatchOutcome (batch 4)

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
        session: &mut Session,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<TurnResult> {
        if let Some(tr) = self.run_recovery(session, turn_ctx.clone(), runtime).await? {
            return Ok(tr);
        }
        self.run_inner(session, turn_ctx, runtime).await
    }

    /// The raw LLM ↔ tool loop (no recovery pre-check). Called by [`Self::run`]
    /// after recovery finds nothing to do, and by `run_recovery`'s tail for
    /// Cases B/C. Never calls back into [`Self::run`] — that would recurse
    /// through `run_recovery` indefinitely.
    async fn run_inner(
        &self,
        session: &mut Session,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<TurnResult> {
        // Turn setup (tool view, provider+model, loop breaker, token tracker
        // seed, request prefix, spec conversion, orphan-tool folding) —
        // extracted to `prepare_turn` below (batch 4).
        let (provider, model_id, allowed_tools, tool_specs, mut messages, mut loop_breaker) =
            self.prepare_turn(session, &turn_ctx, runtime).await?;

        // Shared ContextEngine singleton — RFC v2 target shape. Token
        // tracking lives solely on `Session.token_tracker`; ContextEngine
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
            let response = chat_with_retry(
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
            .await?;

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

    /// Resume a session whose history ends mid-turn (process crash,
    /// hot-switch during tool execution). Three cases handled, matching
    /// the legacy `AgentLoop::recover_interrupted_turn` semantics:
    ///
    /// - **Case A** — assistant tool_calls without matching tool_results:
    ///   re-execute each orphan call via the same ToolExecutor `run()`
    ///   uses, append the results to history, then fall through to
    ///   `run_inner()` so the LLM continues.
    /// - **Case B** — trailing tool_results, no LLM response: just call
    ///   `run_inner()`. The chat loop sends the current history to the LLM
    ///   without appending a fresh user message.
    /// - **Case C** — trailing user message, no LLM response: same as
    ///   Case B.
    ///
    /// Returns `None` if the session's history is empty or not in a
    /// mid-turn state (no recovery needed).
    pub async fn run_recovery(
        &self,
        session: &mut Session,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<Option<TurnResult>> {
        use std::collections::HashSet;

        if session.history.is_empty() {
            return Ok(None);
        }

        // Walk backwards collecting completed tool_call_ids and finding
        // any orphan tool_calls in the most recent assistant message.
        let mut completed_ids: HashSet<String> = HashSet::new();
        let mut pending_calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut has_trailing_tool_results = false;
        let mut last_is_user = false;

        for msg in session.history.iter().rev() {
            if msg.role == "tool" {
                if let Some(ref id) = msg.tool_call_id {
                    completed_ids.insert(id.clone());
                }
                has_trailing_tool_results = true;
            } else if msg.role == "assistant" {
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        if !completed_ids.contains(&call.id) {
                            pending_calls.push(call.clone());
                        }
                    }
                }
                break;
            } else if msg.role == "user" {
                last_is_user = true;
                break;
            } else {
                break;
            }
        }

        let needs_case_a = !pending_calls.is_empty();
        let needs_case_b = has_trailing_tool_results && pending_calls.is_empty();
        let needs_case_c = last_is_user;
        if !(needs_case_a || needs_case_b || needs_case_c) {
            return Ok(None);
        }

        // Case A: re-execute orphan tool_calls so history ends well-formed.
        if needs_case_a {
            tracing::info!(
                session = %session.id,
                missing_count = pending_calls.len(),
                "recovery: re-executing interrupted tool calls"
            );
            let mut allowed_tools = self.allowed_tools(runtime);
            filter_turn_scoped_tools(&mut allowed_tools, session);
            let tool_executor = &runtime.tool_executor;

            // Check the exec marker: if present, the call_id it contains was
            // mid-execution when the daemon died (e.g. `myclaw update` →
            // `systemctl restart` → SIGKILL). Re-running such a call would
            // kill the daemon again, creating a crash loop. Instead we
            // synthesize an error result so the LLM can assess the situation.
            let interrupted_id =
                exec_marker_read(runtime.sessions_dir.as_deref(), &session.id);
            if let Some(ref id) = interrupted_id {
                tracing::warn!(
                    session = %session.id,
                    interrupted_call_id = %id,
                    "recovery: exec marker found — a tool call was interrupted by daemon restart"
                );
            }

            for call in &pending_calls {
                // If this call was the one that killed the daemon, don't
                // blindly re-execute it — for `shell` in particular, a
                // tracked process can survive a `myclaw restart` hot switch
                // (see `crate::tools::shell::latest_entry_summary`), so
                // re-invoking would spawn a second copy racing the first.
                // Synthesize a result describing what's known instead and
                // let the LLM decide.
                if interrupted_id.as_deref() == Some(call.id.as_str()) {
                    let detail = if call.name == "shell" {
                        runtime
                            .sessions_dir
                            .as_deref()
                            .and_then(|dir| crate::tools::shell::latest_entry_summary(dir, &session.id))
                    } else {
                        None
                    };
                    let msg = match detail {
                        Some(d) => format!(
                            "[recovery: this command was interrupted by a daemon restart and was not re-executed. {}]",
                            d
                        ),
                        None => "[recovery: this command was interrupted by a \
                                   daemon restart and will not be re-executed. \
                                   It may have partially or fully completed. \
                                   Check the current state before proceeding.]"
                            .to_string(),
                    };
                    tracing::warn!(
                        session = %session.id,
                        call_id = %call.id,
                        tool = %call.name,
                        "recovery: interrupted call not re-executed"
                    );
                    session.add_tool_result(call.id.clone(), &call.name, msg, true);
                    persist_last(session);
                    continue;
                }

                let result = tool_executor
                    .execute(
                        call,
                        session,
                        Some(&turn_ctx.permission_mode),
                        &allowed_tools,
                    )
                    .await;
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
                session.add_tool_result(call.id.clone(), &call.name, result_content, is_error);
                persist_last(session);
            }

            // Clear any stale exec marker — recovery has handled all pending
            // calls, so the marker is no longer needed.
            exec_marker_clear(runtime.sessions_dir.as_deref(), &session.id);
        }

        // Cases B, C, and tail of A: drive the LLM loop from the now
        // well-formed history. The user message (if any) is already in
        // history. `run_inner` (not `run`): `run` would re-enter
        // `run_recovery` and recurse forever.
        let tr = self.run_inner(session, turn_ctx, runtime).await?;
        Ok(Some(tr))
    }
}

#[cfg(test)]
mod skeleton_tests {
    // Skeleton-level tests only (run/run_recovery end-to-end via
    // bailing_runtime). Symbol-level tests live beside their implementation;
    // shared fixtures live in `tests.rs` (batch 5, RFC §2).
    use super::*;
    use crate::agents::session::Session;
    use crate::config::agent::{PermissionMode, RunMode};
    use crate::providers::{Tool, ToolResult};
    use async_trait::async_trait;
    use std::sync::Arc;

    use super::tests::{bailing_runtime, empty_config};

    // (Image placeholdering moved to `providers::media::lower_media_for`, which
    // owns its own unit tests; the agent no longer renders images.)

    /// Regression guard for the v4 recovery refactor: `Agent::run` must
    /// pre-check `run_recovery`, and `run_recovery`'s Cases B/C must fall
    /// through to `run_inner` — NOT back into `run` (that would re-enter
    /// `run_recovery` and recurse until the stack overflows).
    ///
    /// Case C (trailing user message, no LLM response) exercises the full
    /// `run → run_recovery → run_inner` chain; the stub registry makes the
    /// first provider lookup fail with "test stub", which is exactly what
    /// we assert. A regression to `run_recovery` calling `run()` would
    /// abort the test with a stack overflow instead of returning Err.
    #[tokio::test]
    async fn run_prechecks_recovery_without_recursing() {
        let mut session = Session::new("sess-1".into());
        session.add_user("pending question".into());
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime();
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };
        let err = match agent.run(&mut session, turn_ctx, &runtime).await {
            Ok(_) => panic!("expected recovery to run the LLM and hit the stub registry"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("test stub"),
            "expected stub registry error, got: {err}"
        );
    }

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            self.0
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _session: &Session,
        ) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        tools.iter().map(|tool| tool.name().to_string()).collect()
    }

    #[test]
    fn turn_tool_allowlist_narrows_to_intersection() {
        // Some(["shell"]) → only "shell" survives (intersection with
        // whatever was already in the list).
        let mut session = Session::new("s".into());
        session.turn_tool_allowlist = Some(vec!["shell".into()]);
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("shell")),
            Arc::new(NamedTool("file_read")),
            Arc::new(NamedTool("calculator")),
        ];
        filter_turn_scoped_tools(&mut tools, &session);
        assert_eq!(tool_names(&tools), vec!["shell"]);
    }

    #[test]
    fn turn_tool_allowlist_empty_forbids_all() {
        // Some([]) → explicitly disable all tools.
        let mut session = Session::new("s".into());
        session.turn_tool_allowlist = Some(vec![]);
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("shell")),
            Arc::new(NamedTool("file_read")),
        ];
        filter_turn_scoped_tools(&mut tools, &session);
        assert!(tools.is_empty());
    }

    #[test]
    fn turn_tool_allowlist_none_preserves_existing() {
        // None → no extra filtering beyond the normal scoped filter.
        let mut session = Session::new("s".into());
        session.turn_tool_allowlist = None;
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("shell")),
            Arc::new(NamedTool("calculator")),
        ];
        filter_turn_scoped_tools(&mut tools, &session);
        assert_eq!(tool_names(&tools), vec!["shell", "calculator"]);
    }

    #[test]
    fn agent_holds_config() {
        let cfg = empty_config();
        let agent = Agent::new(cfg);
        assert_eq!(agent.config.name, "test");
    }

    #[test]
    fn session_persist_field_default_none() {
        let session = Session::new("s".into());
        assert!(session.persist.is_none());
        assert!(session.channels.is_none());
        assert!(session.channel_account.is_none());
        assert!(!session.turn_headless);
        assert!(session.resolve_channel().is_none());
    }

}
