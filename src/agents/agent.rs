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

use std::sync::Arc;

use anyhow::Result;

use futures_util::StreamExt;

use crate::agents::AgentRuntime;

use crate::agents::error::AgentError;
use crate::agents::loop_breaker::{LoopBreak, LoopBreakReason};
use crate::agents::session::Session;
use crate::agents::turn::{TurnContext, TurnResult};
use crate::agents::turn_event::TurnEvent;
use crate::agents::user_registry::UserRegistry;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::capability_chat::{
    ChatMessage, ChatMessageUsage, ChatRequest, StopReason, ToolSpec,
};
use crate::providers::{
    BoxStream, Capability, ContentPart, FileModality, MediaInlineDecision, MediaPolicy,
    ProviderRegistry, StreamEvent, ToolCall, modality_from_mime,
};

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
        // explicitly on every `execute` call.
        let tool_executor = &runtime.tool_executor;

        // Per-turn loop breaker counter — allocated fresh each turn by
        // the shared `runtime.loop_breaker` singleton. Per-agent
        // `SubAgentConfig.max_tool_calls` overrides the runtime default;
        // when None, the shared config wins.
        let mut loop_breaker = match self.config.max_tool_calls {
            Some(n) => runtime.loop_breaker.new_counter_with_max(n),
            None => runtime.loop_breaker.new_counter(),
        };

        // Shared ContextEngine singleton — RFC v2 target shape. Token
        // tracking lives solely on `Session.token_tracker`; ContextEngine
        // only carries threshold/retain_units + summarizer state.
        let context = &runtime.context_engine;
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

        let permission_mode = turn_ctx.permission_mode;
        let mut empty_response_retries: usize = 0;
        const MAX_EMPTY_RETRIES: usize = 3;
        let mut overflow_retries: usize = 0;
        const MAX_OVERFLOW_RETRIES: usize = 3;
        // Track whether the main agent called memory_manage this turn —
        // if so, the forked extraction is redundant (mutual exclusion).
        let mut turn_called_memory = false;
        // Track whether any tool call errored this turn — skill extraction
        // only fires on clean turns (no errors).
        let mut turn_had_error = false;
        // Async delegation is a turn boundary: after the entire provider batch
        // has been executed and persisted, return to SessionContext so the
        // origin turn releases its lock before the completion wake is queued.
        let mut async_delegation_spawned = false;
        // Send tools are not always declared (loaded on demand), so fold any
        // prior references that would become orphan tool calls.
        fold_absent_tool(&mut messages, &tool_specs, "send_message", "消息发送结果");
        fold_absent_tool(&mut messages, &tool_specs, "send_media", "媒体发送结果");

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

            // Pre-send compaction guard: compact BEFORE the request when the
            // history we're about to send is over threshold. Driven by a direct
            // history estimate (not the token tracker) so a stale/under-counted
            // tracker can't let an over-window request through. This is the real
            // fix for the "974 msgs sent at 31k tracked → context overflow" bug.
            // Loops until under threshold so a history far over the window
            // converges within this turn rather than one chunk per user turn.
            if let Some(compacted) = context.compact_until_fit(
                session,
                turn_ctx.system_prompt,
                &model_id,
                Arc::clone(&provider),
                &tool_specs,
                runtime.task_boards.as_ref(),
                false,
            )
            .await
            {
                messages = compacted;

                // Post-compaction re-injection: compaction summarizes away old
                // <system-reminder> blocks (skills/memory/date/autonomy/agents).
                // The agent loop bypasses process_turn, so the diff-based
                // attachment injection in session_context.rs never runs. Re-run
                // the diffs here against the freshly-compacted history; any
                // missing reminders are injected as transient messages (not
                // persisted to history, exactly like sub-agent inbox below).
                let reminder = {
                    let skills_snap = runtime.skills.read();
                    let history_clone = session.history.clone();
                    session.attachments.diff_skills(&skills_snap, &history_clone);
                    let agent_list: Vec<(String, String)> = runtime
                        .agents
                        .values_cloned()
                        .into_iter()
                        .map(|a| {
                            (
                                a.config.name.clone(),
                                a.config.description.clone().unwrap_or_default(),
                            )
                        })
                        .collect();
                    session.attachments.diff_agents(&agent_list, &history_clone);
                    session.attachments.diff_date(
                        runtime.context_engine.timezone_offset(),
                        &history_clone,
                    );
                    session.attachments.diff_autonomy(&permission_mode, &history_clone);
                    let memory_root = &runtime.defaults.prompt.memory_root;
                    let memory_entries: Vec<crate::memory::IndexEntry> =
                        if !memory_root.is_empty() {
                            let memory_dir = std::path::Path::new(memory_root);
                            let files = crate::memory::scan_memory_files(memory_dir);
                            files.iter().map(crate::memory::IndexEntry::from).collect()
                        } else {
                            Vec::new()
                        };
                    session.attachments.diff_memory(&memory_entries, &history_clone);
                    let text = session.attachments.build_text(&skills_snap);
                    session.attachments.clear_pending();
                    text
                };
                if let Some(reminder_text) = reminder {
                    tracing::info!(
                        session = %session.id,
                        "injected system-reminder snapshot after compaction"
                    );
                    let snapshot_msg = ChatMessage::user_text(reminder_text);
                    if messages.len() > 1 {
                        messages.insert(1, snapshot_msg);
                    } else {
                        messages.push(snapshot_msg);
                    }
                }
            }

            // Time-awareness: sub-agent sessions carry a wall-clock kill
            // deadline. Inject the remaining budget as a transient
            // `<system-reminder>` before every LLM request (not persisted —
            // same consumption model as the inbox below) so the sub-agent
            // can pace itself and, at ≤20% remaining, is told to wrap up
            // instead of being killed mid-flight with nothing delivered.
            if let Some(deadline) = session.delegation_deadline {
                let remaining = deadline.remaining_secs();
                if remaining > 0 {
                    messages.push(ChatMessage::user_text(deadline.render_reminder()));
                }
            }

            // RFC agent-messaging §3.4/§3.7: drain the sub-agent inbox before
            // this LLM request so parent → sub messages are visible on the
            // next tool round. Injected as a `<system-reminder>` user message
            // (not persisted to history — the tool-loop alternation stays
            // clean and injection is consumption). Placement is deliberately
            // AFTER compaction so a compaction pass cannot drop the batch.
            // §3.7: if the batch exceeds the per-round budget, only the
            // newest complete messages are injected and the older remainder
            // is re-queued for a later round (never dropped, never truncated).
            if let Some(mailbox) = &session.sub_agent_inbox {
                let mut pending = Vec::new();
                {
                    let mut rx = mailbox.rx.lock().await;
                    while let Ok(mail) = rx.try_recv() {
                        pending.push(mail);
                    }
                }
                if !pending.is_empty() {
                    let (kept, deferred) =
                        crate::agents::delegation::select_within_injection_budget(pending);
                    if !deferred.is_empty() {
                        tracing::warn!(
                            session = %session.id,
                            deferred = deferred.len(),
                            "inbox batch over injection budget; deferring older messages to a later round"
                        );
                        for mail in deferred {
                            let _ = mailbox.tx.send(mail).await;
                        }
                    }
                    if !kept.is_empty() {
                        tracing::info!(
                            session = %session.id,
                            count = kept.len(),
                            "injecting sub-agent inbox messages before LLM request"
                        );
                        messages.push(ChatMessage::user_text(
                            crate::agents::delegation::render_agent_mail_reminder(&kept),
                        ));
                    }
                }
            }

            // RFC §3.5/§4.3: per-turn injections (user-level mailbox +
            // pending friend requests), rendered by `dispatch_turn` and
            // stashed on the session. 注入即消费 — injected into this first
            // LLM request then cleared; pending requests are re-rendered
            // every turn by `dispatch_turn` while they remain.
            if !session.turn_injections.is_empty() {
                let injections = std::mem::take(&mut session.turn_injections);
                for text in injections {
                    messages.push(ChatMessage::user_text(text));
                }
            }

            let response = {
                tracing::info!(
                    session = %session.id,
                    msg_count = messages.len(),
                    model = %model_id,
                    history_len = session.history.len(),
                    "agent: sending LLM request"
                );
                const MAX_LLM_RETRIES: usize = 2;
                let mut attempt: usize = 0;
                loop {
                    let thinking = turn_ctx.thinking.cloned();
                    let req = ChatRequest {
                        model: &model_id,
                        messages: &messages,
                        temperature: None,
                        max_tokens: None,
                        thinking,
                        stop: None,
                        seed: None,
                        tools: if tool_specs.is_empty() {
                            None
                        } else {
                            Some(&tool_specs)
                        },
                        stream: true,
                    };
                    let stream = provider.chat(req)?;
                    match collect_stream(
                        stream,
                        &mut session.turn_stream,
                        runtime.user_registry.as_ref(),
                    )
                    .await
                    {
                        Ok(resp) => break Ok(resp),
                        Err(e) if attempt < MAX_LLM_RETRIES && is_transient_llm_error(&e) => {
                            attempt += 1;
                            tracing::warn!(
                                model = %model_id, attempt,
                                err = %e,
                                "LLM call failed with transient error, retrying"
                            );
                            tokio::time::sleep(backoff_duration(attempt)).await;
                            continue;
                        }
                        Err(e) => break Err(e),
                    }
                }
            }?;

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

            // Context-overflow backstop. The provider rejected the request as
            // too large (empty body, mapped to `ContextOverflow` instead of a
            // misleading `EndTurn`). This only happens if the pre-send guard's
            // estimate undershot the real token count; force a compaction and
            // retry rather than blindly re-sending the same over-window request.
            if response.stop_reason == StopReason::ContextOverflow {
                overflow_retries += 1;
                let recovered = if overflow_retries <= MAX_OVERFLOW_RETRIES {
                    context.compact_until_fit(
                        session,
                        turn_ctx.system_prompt,
                        &model_id,
                        Arc::clone(&provider),
                        &tool_specs,
                        runtime.task_boards.as_ref(),
                        true, // force: bypass the threshold, we know it overflowed
                    )
                    .await
                } else {
                    None
                };
                match recovered {
                    Some(compacted) => {
                        messages = compacted;
                        tracing::warn!(
                            attempt = overflow_retries,
                            "context overflow reported by provider; compacted and retrying"
                        );
                        continue;
                    }
                    None => {
                        // Can't reduce further (history already minimal) or out of
                        // attempts — surface a clear message instead of silently
                        // giving up like the empty-response path would.
                        tracing::warn!(
                            attempt = overflow_retries,
                            "context overflow and compaction could not recover; giving up"
                        );
                        let msg = "⚠️ 当前对话已超出该模型的上下文上限，压缩后仍无法容纳。\
                            请使用 /new 开启新会话，或精简后重试。"
                            .to_string();
                        session.add_assistant(msg.clone());
                        persist_last(session);
                        return Ok(TurnResult {
                            text: msg,
                            stop_reason: StopReason::ContextOverflow,
                            pending_retry: None,
                            has_pending: false,
                        });
                    }
                }
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
                    empty_response_retries += 1;
                    let tool_use_without_calls = response.stop_reason == StopReason::ToolUse;
                    if empty_response_retries > MAX_EMPTY_RETRIES {
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
                            attempt = empty_response_retries,
                            tool_call_events = response.tool_call_events,
                            "tool_use stop without tool calls, retrying"
                        );
                    } else {
                        tracing::warn!(
                            attempt = empty_response_retries,
                            stop = ?response.stop_reason,
                            "empty response, retrying"
                        );
                    }
                    // No Done yet — we will re-call the model.
                    continue;
                }
                // Emit Done before persisting so streaming UI gets final text.
                // 单 preview (2026-08-12): the ORIGIN turn that spawned async
                // delegations must keep its preview in progress form — with
                // ordinary-turn semantics `Done` collapses it into the one-line
                // summary right away, and the suspension's notice turns would
                // then take over a collapsed summary instead of the live
                // progress ("先 summary 再 progress", user-confirmed). Marking
                // the stream `defer_collapse` appends this turn's final
                // commentary as a 💬 line and KEEPS the preview; only the
                // FINAL resume turn collapses it (final_takeover → summary
                // line; the final answer is delivered as a separate message
                // by the fallback — user-confirmed shape: 2 messages).
                // Silenced resume turns already
                // get `defer_collapse` from `process_turn`; the final loud
                // resume turn has `async_delegation_spawned == false`, so
                // neither flag applies there.
                if async_delegation_spawned && !session.turn_silenced {
                    if let Some(stream) = session.turn_stream.as_mut() {
                        stream.defer_collapse();
                    }
                }
                push_or_drop(
                    &mut session.turn_stream,
                    TurnEvent::Done {
                        text: response.text.clone(),
                    },
                )
                .await;
                let mut msg = ChatMessage::assistant_text(response.text.clone());
                let effective_model: &str =
                    response.actual_model.as_deref().unwrap_or(&model_id);
                msg.model = Some(effective_model.to_string());
                msg.usage = llm_usage(
                    &response,
                    runtime
                        .providers
                        .get_chat_provider_id_by_model(effective_model),
                    effective_model,
                );
                session.history.push(msg.clone());
                session.message_ids.push(0);
                persist_last(session);

                // Fire-and-forget memory extraction fork (claude-code pattern).
                // Skipped when the main agent already called memory_manage this
                // turn (mutual exclusion) or when this is a sub-agent session.
                if !turn_called_memory && session.parent_session_id.is_none() {
                    let mut fork_messages = messages.clone();
                    fork_messages.push(msg.clone());
                    let fork_input = crate::agents::memory_fork::ForkInput {
                        messages: fork_messages,
                        model_id: model_id.clone(),
                        provider: Arc::clone(&provider),
                        tool_specs: tool_specs.clone(),
                        tool_registry: Arc::clone(&runtime.tools),
                        session_owner: session.owner.clone(),
                        session_id: session.id.clone(),
                        memory_root: runtime.defaults.prompt.memory_root.clone(),
                        registry: Arc::clone(&runtime.providers) as Arc<dyn ProviderRegistry>,
                    };
                    tokio::spawn(async move {
                        crate::agents::memory_fork::run_memory_fork(fork_input).await;
                    });
                }

                // Fire-and-forget skill extraction fork.
                // Fires once per session when the turn had enough tool calls
                // (>= 5) and no errors — indicating a substantive, successful
                // interaction that may contain reusable procedures.
                if !turn_had_error
                    && loop_breaker.total_calls() >= 5
                    && session.parent_session_id.is_none()
                {
                    let mut skill_messages = messages.clone();
                    skill_messages.push(msg.clone());
                    let skill_input = crate::agents::skill_extract::SkillExtractInput {
                        messages: skill_messages,
                        model_id: model_id.clone(),
                        provider: Arc::clone(&provider),
                        tool_specs: tool_specs.clone(),
                        tool_registry: Arc::clone(&runtime.tools),
                        session_id: session.id.clone(),
                        workspace_dir: runtime.defaults.prompt.workspace_dir.clone(),
                    };
                    tokio::spawn(async move {
                        crate::agents::skill_extract::run_skill_extract(skill_input).await;
                    });
                }

                return Ok(TurnResult {
                    text: response.text,
                    stop_reason: response.stop_reason,
                    pending_retry: None,
                    // 单 preview (2026-08-12): an EndTurn on a turn that spawned
                    // async delegations is the ORIGIN turn of a suspension
                    // sequence — `has_pending` marks it as such so the
                    // dispatcher suspends instead of ending. The model may have
                    // continued with other work after the spawn (the old forced
                    // truncation is gone); its output is the preview content.
                    has_pending: async_delegation_spawned,
                });
            }

            // Tool calls present — append assistant message with the calls
            // (preserving thinking content for re-send), execute each tool,
            // append tool_result messages, then loop for the next LLM call.
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
            let effective_model: &str = response.actual_model.as_deref().unwrap_or(&model_id);
            let usage = llm_usage(
                &response,
                runtime
                    .providers
                    .get_chat_provider_id_by_model(effective_model),
                effective_model,
            );
            messages.push(assistant_msg);
            session.add_assistant_with_tools(
                response.text.clone(),
                response.tool_calls.clone(),
                response.reasoning_content.clone(),
                response.thinking_signature.clone(),
                Some(effective_model.to_string()),
                usage,
            );
            persist_last(session);

            for (i, call) in response.tool_calls.iter().enumerate() {
                // Track memory_manage calls for fork mutual exclusion.
                if call.name == "memory_manage" {
                    turn_called_memory = true;
                }
                // Execute the tool call first, then check the limit.
                // Checking before execution would leave orphan tool_calls
                // (assistant declares a call but no result is appended).

                // Emit ToolCall event before execution (streaming UIs show
                // the call spinner).
                {
                    let args: serde_json::Value =
                        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
                    push_or_drop(
                        &mut session.turn_stream,
                        TurnEvent::ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args,
                        },
                    )
                    .await;
                }

                // Notify channel of tool call start (for reply progress).
                // RFC channel-role-split §1.2: resolved from the live
                // registry, not a turn-installed handle.
                if let (Some(ch), Some(rt)) =
                    (session.resolve_channel(), session.reply_target())
                {
                    ch.on_tool_event(
                        rt,
                        crate::channels::ToolEvent::Start {
                            tool_name: call.name.clone(),
                            tool_call_id: call.id.clone(),
                        },
                    )
                    .await;
                }

                // Write exec marker before execution so recovery can detect
                // a tool that killed the daemon (e.g. `myclaw update`).
                exec_marker_write(
                    runtime.sessions_dir.as_deref(),
                    &session.id,
                    &call.id,
                );

                let result = tool_executor
                    .execute(call, session, Some(&permission_mode), &allowed_tools)
                    .await;

                // Clear the marker now that execute() returned — the tool
                // completed (or errored), so re-execution by recovery is
                // no longer the concern. Guard ensures cleanup on any path.
                let _marker_guard = ExecMarkerGuard {
                    sessions_dir: runtime.sessions_dir.clone(),
                    session_id: session.id.clone(),
                };

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
                if is_error {
                    turn_had_error = true;
                }

                match loop_breaker.record_and_check(&call.name, &call.arguments, &result_content) {
                    LoopBreak::Detected(reason) => {
                        tracing::warn!(
                            session = %session.id,
                            tool = %call.name,
                            reason = ?reason,
                            "loop breaker triggered"
                        );
                        // Record-and-check triggered: append the result for
                        // this call so the pair is complete, then strip any
                        // remaining unexecuted tool_calls and abort.
                        let mut tool_msg = ChatMessage::text("tool", &result_content);
                        tool_msg.tool_call_id = Some(call.id.clone());
                        tool_msg.is_error = Some(is_error);
                        messages.push(tool_msg);
                        session.add_tool_result(
                            call.id.clone(),
                            &call.name,
                            result_content.clone(),
                            is_error,
                        );

                        let remaining = response.tool_calls.len() - i - 1;
                        if remaining > 0 {
                            session.strip_trailing_tool_calls(remaining);
                        }
                        persist_last(session);
                        return Err(AgentError::LoopBreak { reason }.into());
                    }
                    LoopBreak::None => {}
                }

                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(is_error);
                messages.push(tool_msg);

                // Persist the tool result to disk BEFORE any async
                // notification. persist_last is synchronous (no await
                // points), so once execute() returns we can write the
                // result immediately without risk of task cancellation
                // at a later await point (push_or_drop, on_tool_event).
                // If the task is cancelled during a downstream await,
                // the result is already safe on disk.
                session.add_tool_result(call.id.clone(), &call.name, result_content.clone(), is_error);
                persist_last(session);

                // Emit ToolResult event after the result is persisted.
                push_or_drop(
                    &mut session.turn_stream,
                    TurnEvent::ToolResult {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        output: result_content,
                    },
                )
                .await;

                // Notify channel of tool call completion (for reply progress).
                // RFC channel-role-split §1.2: resolved from the live
                // registry, not a turn-installed handle.
                if let (Some(ch), Some(rt)) =
                    (session.resolve_channel(), session.reply_target())
                {
                    ch.on_tool_event(
                        rt,
                        crate::channels::ToolEvent::End {
                            tool_name: call.name.clone(),
                            tool_call_id: call.id.clone(),
                            success: !is_error,
                        },
                    )
                    .await;
                }

                // Detect successful async delegation only after this result has
                // been persisted. Continue executing the rest of the provider
                // batch; the boundary is checked after the batch below.
                if call.name == "agent_delegate" && !is_error {
                    let mode = serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .ok()
                        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(str::to_owned));
                    if mode.as_deref() == Some("async") {
                        async_delegation_spawned = true;
                    }
                }

                // sessions_yield (docs/delegation-notice-queue-rfc.md §3.2):
                // explicit hand-off — deterministic EndTurn, discard remaining
                // tool_calls. has_pending reuses async_delegation_spawned so
                // suspension proceeds normally when sub-agents were spawned.
                if call.name == "sessions_yield" && !is_error {
                    let remaining = response.tool_calls.len() - i - 1;
                    if remaining > 0 {
                        session.strip_trailing_tool_calls(remaining);
                    }
                    tracing::info!(
                        session = %session.id,
                        "sessions_yield called; ending turn deterministically"
                    );
                    return Ok(TurnResult {
                        text: response.text.clone(),
                        stop_reason: StopReason::EndTurn,
                        pending_retry: None,
                        has_pending: async_delegation_spawned,
                    });
                }

                // After executing this call, check if we've hit the hard
                // limit. If so, strip remaining unexecuted tool_calls and
                // abort — avoids orphan tool_calls in history.
                if loop_breaker.total_calls() >= loop_breaker.max_tool_calls() {
                    let remaining = response.tool_calls.len() - i - 1;
                    if remaining > 0 {
                        session.strip_trailing_tool_calls(remaining);
                    }
                    tracing::warn!(
                        session = %session.id,
                        total_calls = loop_breaker.total_calls(),
                        max = loop_breaker.max_tool_calls(),
                        "loop breaker triggered: max tool calls exceeded"
                    );
                    return Err(AgentError::LoopBreak {
                        reason: LoopBreakReason::MaxCalls {
                            count: loop_breaker.total_calls(),
                            limit: loop_breaker.max_tool_calls(),
                        },
                    }
                    .into());
                }
            }

            if async_delegation_spawned {
                tracing::debug!(
                    session = %session.id,
                    parent = session.parent_session_id.as_deref().unwrap_or("none"),
                    "async delegation spawned this turn; has_pending flag set at EndTurn"
                );
            }

            // Loop back to the next LLM call with the appended tool_result messages.
        }
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

    /// Filter `runtime.tools` through `self.config.allows_tool/skill/mcp`.
    /// MVP: ignores `source()` distinctions because `allows_tool` is a
    /// flat name check; C18 (full) will switch to per-source dispatch
    /// once SubAgentConfig.tools is the structured `ToolFilter` form.
    fn allowed_tools(&self, runtime: &AgentRuntime) -> Vec<Arc<dyn crate::providers::Tool>> {
        runtime
            .tools
            .all_tools()
            .into_iter()
            .filter(|t| {
                let name = t.name();
                if self.config.allows_tool(name) {
                    return true;
                }
                // Tools whose source is an MCP server: route via the mcp filter.
                if let crate::providers::ToolSource::Mcp { server } = t.source() {
                    return self.config.allows_mcp(&server);
                }
                false
            })
            .collect()
    }
}

fn filter_turn_scoped_tools(
    allowed_tools: &mut Vec<Arc<dyn crate::providers::Tool>>,
    session: &Session,
) {
    if let Some(allowlist) = &session.turn_tool_allowlist {
        allowed_tools.retain(|tool| allowlist.contains(&tool.name().to_string()));
    }

    allowed_tools.retain(|tool| {
        let keep = match tool.name() {
            "send_message" => {
                // RFC agent-messaging §3: sub-agent sessions get the tool
                // even without a channel — `recipient` targeting reaches the
                // parent agent via the DelegationEvent channel.
                // RFC channel-role-split §1.2: main-agent visibility is now
                // `resolve_channel().is_some() || parent_session_id.is_some()`
                // — a headless (scheduled) turn whose routing key resolves to
                // a real channel keeps the tool (sending intermediate notices
                // from background turns is legitimate, not a coupling bug).
                if session.parent_session_id.is_some() {
                    true
                } else if session.resolve_channel().is_some() {
                    let has_receiver = session.reply_target().is_some();
                    let has_text_send = has_receiver;
                    let has_file_send = session
                        .resolve_channel()
                        .is_some_and(|ch| ch.capabilities().supports_file_send);
                    has_text_send || has_file_send
                } else {
                    false
                }
            }
            "send_media" => false,
            // RFC §4.2: friend tools are main-agent-only — contacts are
            // user-level state and sub-agents never see them.
            "friend_request" | "friend_accept" | "friend_decline" | "friend_list" => {
                session.parent_session_id.is_none()
            }
            _ => true,
        };
        if !keep {
            tracing::debug!(
                tool = tool.name(),
                session = %session.id,
                "filter_turn_scoped_tools: dropped"
            );
        }
        keep
    });
}

/// Remove media-retrieval tools (`view_video`, `view_image`, `hear_audio`)
/// when the model that will handle the request natively supports that input
/// modality. This prevents the model from choosing a tool call over inline
/// content analysis.
///
/// - With an override: that model's capabilities determine the filter.
/// - Without override: the primary model in the routing chain is used. On
///   transient failure the FallbackChatProvider may retry with a different
///   model — acceptable trade-off vs. always keeping redundant tools.
fn native_media_availability(messages: &[ChatMessage], policy: MediaPolicy) -> (bool, bool, bool) {
    let mut image = false;
    let mut audio = false;
    let mut video = false;
    for msg in messages {
        for part in &msg.parts {
            let ContentPart::File {
                path,
                mime_type,
                size_bytes,
                ..
            } = part
            else {
                continue;
            };
            let modality = modality_from_mime(mime_type.as_deref(), path);
            if policy.decision_for(modality, *size_bytes) != MediaInlineDecision::Inline {
                continue;
            }
            match modality {
                FileModality::Image => image = true,
                FileModality::Audio => audio = true,
                FileModality::Video => video = true,
                FileModality::Other => {}
            }
        }
    }
    (image, audio, video)
}

fn filter_modality_redundant_tools(
    allowed_tools: &mut Vec<Arc<dyn crate::providers::Tool>>,
    messages: &[ChatMessage],
    policy: MediaPolicy,
    model_id: &str,
) {
    let (native_image, native_audio, native_video) = native_media_availability(messages, policy);

    allowed_tools.retain(|tool| {
        let drop = match tool.name() {
            "view_video" => native_video,
            "view_image" => native_image,
            "hear_audio" => native_audio,
            _ => false,
        };
        if drop {
            tracing::info!(
                model = %model_id,
                tool = tool.name(),
                native_image,
                native_audio,
                native_video,
                "filter_modality_redundant_tools: dropping tool, current request has inline native media"
            );
        }
        !drop
    });
}

/// Backstop for a rare edge (e.g. config hot-reload drops the aux model
/// mid-session): if the request won't declare `tool_name` but history still
/// references it, fold each such call + its result into inline `[label]: …` text
/// on the calling assistant message and drop the tool-result message, so no
/// orphan tool call survives to be rejected by the provider. No-op when the tool
/// is declared. Operates on the cloned `messages` only.
pub(crate) fn fold_absent_tool(
    messages: &mut Vec<ChatMessage>,
    tool_specs: &[ToolSpec],
    tool_name: &str,
    label: &str,
) {
    if tool_specs.iter().any(|t| t.name == tool_name) {
        return;
    }
    let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages.iter() {
        if m.role == "assistant" {
            if let Some(tcs) = &m.tool_calls {
                for tc in tcs.iter().filter(|tc| tc.name == tool_name) {
                    ids.insert(tc.id.clone());
                }
            }
        }
    }
    if ids.is_empty() {
        return;
    }
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages.iter() {
        if m.role == "tool" {
            if let Some(id) = &m.tool_call_id {
                if ids.contains(id) {
                    results.insert(id.clone(), m.text_content());
                }
            }
        }
    }
    for m in messages.iter_mut() {
        if m.role != "assistant" {
            continue;
        }
        let Some(tcs) = m.tool_calls.take() else {
            continue;
        };
        let (mine, rest): (Vec<_>, Vec<_>) = tcs.into_iter().partition(|tc| tc.name == tool_name);
        m.tool_calls = if rest.is_empty() { None } else { Some(rest) };
        for tc in mine {
            if let Some(out) = results.get(&tc.id) {
                if !out.is_empty() {
                    m.parts.push(ContentPart::Text {
                        text: format!("[{label}]: {out}"),
                    });
                }
            }
        }
    }
    messages.retain(|m| {
        !(m.role == "tool" && m.tool_call_id.as_ref().is_some_and(|id| ids.contains(id)))
    });
}

/// Persist `session.history.last()` via `session.persist` and write the
/// returned backend ID into `session.message_ids.last_mut()`. Mirrors the
/// legacy `AgentLoop` pattern — without the id-capture, message_ids stays
/// at the 0 placeholder forever, which breaks compaction's
/// `last_compacted_id` lookup (it would always be 0).
fn persist_last(session: &mut Session) {
    let hook = match session.persist.clone() {
        Some(h) => h,
        None => return,
    };
    let msg = match session.history.last().cloned() {
        Some(m) => m,
        None => return,
    };
    if let Some(id) = hook.persist_message(&session.id, &msg) {
        if let Some(slot) = session.message_ids.last_mut() {
            *slot = id;
        }
    }
}

// ── Exec marker ─────────────────────────────────────────────────────────────
//
// When a tool call kills the daemon (e.g. `shell("myclaw update")` triggers
// `systemctl restart`), `execute()` never returns and the tool result is
// never persisted. On restart, recovery sees an orphan tool_call and blindly
// re-executes it — killing the daemon again in an infinite loop.
//
// The exec-marker breaks this cycle: before executing any tool we write a
// tiny file `sessions/<id>/.exec_marker` containing the call_id. If the
// daemon dies during execution the file survives. On recovery, any pending
// call whose id matches the marker is treated as "interrupted" — a synthetic
// error result is appended instead of re-executing.

/// Write the call_id to `.exec_marker` so recovery can detect an
/// interrupted execution. Silently no-ops if `sessions_dir` is None.
fn exec_marker_write(sessions_dir: Option<&std::path::Path>, session_id: &str, call_id: &str) {
    let Some(dir) = sessions_dir else {
        return;
    };
    let path = dir.join(crate::ids::bare_dir_name(session_id)).join(".exec_marker");
    let _ = std::fs::write(&path, call_id);
}

/// Read the call_id from `.exec_marker`, or `None` if absent.
fn exec_marker_read(sessions_dir: Option<&std::path::Path>, session_id: &str) -> Option<String> {
    let dir = sessions_dir?;
    let path = dir.join(crate::ids::bare_dir_name(session_id)).join(".exec_marker");
    std::fs::read_to_string(&path).ok()
}

/// Remove `.exec_marker`. Silently no-ops if the file doesn't exist.
fn exec_marker_clear(sessions_dir: Option<&std::path::Path>, session_id: &str) {
    let Some(dir) = sessions_dir else {
        return;
    };
    let path = dir.join(crate::ids::bare_dir_name(session_id)).join(".exec_marker");
    let _ = std::fs::remove_file(&path);
}

/// RAII guard that clears the exec marker when dropped. Created after
/// `execute()` returns so the marker is removed as soon as the tool
/// finishes — even on early returns from the loop.
struct ExecMarkerGuard {
    sessions_dir: Option<std::path::PathBuf>,
    session_id: String,
}

impl Drop for ExecMarkerGuard {
    fn drop(&mut self) {
        exec_marker_clear(self.sessions_dir.as_deref(), &self.session_id);
    }
}

/// Pull the text of the most recent user message from history, if any.
/// Used for `TurnResult.pending_retry` so the orchestrator can surface
/// a "retry?" prompt without re-asking the user for their input.
fn last_user_text(session: &Session) -> String {
    session
        .history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.text_content())
        .unwrap_or_default()
}

fn llm_usage(
    response: &CollectedResponse,
    provider: Option<String>,
    model: &str,
) -> Option<ChatMessageUsage> {
    let has_usage = response.usage.is_some();
    let usage = response.usage.as_ref();
    if !has_usage && provider.is_none() && model.is_empty() {
        return None;
    }
    Some(ChatMessageUsage {
        provider,
        model: Some(model.to_string()),
        input_tokens: usage.and_then(|u| u.input_tokens),
        cached_input_tokens: usage.and_then(|u| u.cached_input_tokens),
        output_tokens: usage.and_then(|u| u.output_tokens),
        reasoning_tokens: usage.and_then(|u| u.reasoning_tokens),
        cache_write_tokens: usage.and_then(|u| u.cache_write_tokens),
        stop_reason: Some(format!("{:?}", response.stop_reason)),
    })
}

/// Bundle of fields extracted from one streaming LLM response. Mirrors
/// the shape of `agent_impl::types::CollectedResponse` but is defined
/// here so `agent.rs` doesn't reach into `agent_impl/` internals.
struct CollectedResponse {
    text: String,
    reasoning_content: Option<String>,
    thinking_signature: Option<String>,
    tool_calls: Vec<ToolCall>,
    tool_call_events: usize,
    stop_reason: StopReason,
    usage: Option<crate::providers::ChatUsage>,
    /// Model that actually produced the stream (from the fallback chain's
    /// `ModelUsed` announcement). `None` when the caller's `model_id` is
    /// authoritative (direct provider, override, or no failover).
    actual_model: Option<String>,
}

/// Push a `TurnEvent` to `session.turn_stream`, dropping the stream on
/// permanent transport failure (RFC §7.6.5(a)).
///
/// Without this short-circuit, `Agent::run` would keep generating chunks
/// for a disconnected client and waste LLM output budget. After drop,
/// subsequent `push_or_drop` calls become no-ops; the end-of-turn fallback
/// `send_message` in `SessionContext::process_turn` then ensures the user
/// still receives the final text via the non-streaming path.
async fn push_or_drop(
    turn_stream: &mut Option<Box<dyn crate::channels::TurnStream>>,
    event: TurnEvent,
) {
    let Some(stream) = turn_stream.as_mut() else {
        return;
    };
    if let Err(e) = stream.push(event).await {
        tracing::warn!(
            err = %e,
            "turn_stream push failed; dropping stream for remainder of turn"
        );
        *turn_stream = None;
    }
}

/// Read a full stream into a [`CollectedResponse`]. When `channel`
/// per-chunk `TurnEvent::Chunk` / `Thinking` events are pushed via
/// `session.turn_stream.push(…)` as text streams in (RFC §7.6).
/// Simplified compared to `AgentLoop::collect_stream_inner` — no
/// max_output_bytes guard, no cancellation token.
///
/// `user_registry` (P4 第二波): when present, chunks are rendered through
/// [`RefRenderer`] and the final text through [`render_refs`] so `<ref id="…"/>`
/// tags surface as `@昵称(u/uid)`. `None` (tests/CLI) → passthrough.
async fn collect_stream(
    stream: BoxStream<StreamEvent>,
    turn_stream: &mut Option<Box<dyn crate::channels::TurnStream>>,
    user_registry: Option<&Arc<UserRegistry>>,
) -> anyhow::Result<CollectedResponse> {
    let mut stream = stream;
    let mut text = String::new();
    let mut reasoning_content: Option<String> = None;
    let mut thinking_signature: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_call_events: usize = 0;
    let mut usage: Option<crate::providers::ChatUsage> = None;
    let mut actual_model: Option<String> = None;
    let mut received_first_chunk = false;
    let mut ref_renderer = crate::agents::mention::RefRenderer::new();

    // Overall budget for the whole stream read (RFC v2 §六.A): bounds the
    // worst case of many small-but-alive chunks trickling forever. Each
    // individual wait is already bounded by the first-chunk/interval
    // timeouts; this caps the total wall-clock spent in this function.
    let stream_started_at = std::time::Instant::now();

    let stop_reason = loop {
        if stream_started_at.elapsed() > crate::agents::llm_stream::STREAM_TOTAL_TIMEOUT {
            anyhow::bail!(
                "stream total timeout after {}s",
                crate::agents::llm_stream::STREAM_TOTAL_TIMEOUT.as_secs()
            );
        }
        let event_opt = if !received_first_chunk {
            match tokio::time::timeout(
                crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT,
                stream.next(),
            )
            .await
            {
                Ok(ev) => ev,
                Err(_) => anyhow::bail!(
                    "stream chunk timeout after {}s, no data received",
                    crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT.as_secs()
                ),
            }
        } else {
            // Bound mid-stream stalls: once data has started flowing, a long
            // silence means a broken/hung connection (distinct from a slow
            // cold start, which the first-chunk timeout covers).
            match tokio::time::timeout(
                crate::agents::llm_stream::STREAM_CHUNK_INTERVAL_TIMEOUT,
                stream.next(),
            )
            .await
            {
                Ok(ev) => ev,
                Err(_) => anyhow::bail!(
                    "stream stalled: no chunk for {}s mid-response",
                    crate::agents::llm_stream::STREAM_CHUNK_INTERVAL_TIMEOUT.as_secs()
                ),
            }
        };

        let event = match event_opt {
            Some(e) => {
                received_first_chunk = true;
                e
            }
            // The stream ended without a terminal `Done`/`Error`/`HttpError`.
            // A well-behaved provider always sends one; reaching here means the
            // connection was closed mid-response. Fail the turn instead of
            // persisting a truncated reply as if it were complete.
            None => anyhow::bail!(
                "stream ended without a completion marker (provider truncated the response)"
            ),
        };

        match event {
            StreamEvent::Delta { text: delta } => {
                text.push_str(&delta);
                // P4 第二波：显示层渲染（跨 chunk 缓冲 `<ref …` 前缀）。
                let out_delta = match user_registry {
                    Some(r) => ref_renderer.push(&delta, r.as_ref()),
                    None => delta.clone(),
                };
                push_or_drop(
                    turn_stream,
                    TurnEvent::Chunk {
                        delta: out_delta,
                    },
                )
                .await;
            }
            StreamEvent::Thinking { text: delta } => {
                if !delta.is_empty() {
                    push_or_drop(
                        turn_stream,
                        TurnEvent::Thinking {
                            delta: delta.clone(),
                        },
                    )
                    .await;
                    if let Some(rc) = &mut reasoning_content {
                        rc.push_str(&delta);
                    } else {
                        reasoning_content = Some(delta);
                    }
                }
            }
            StreamEvent::ThinkingSignature { signature } => {
                thinking_signature = Some(signature);
            }
            StreamEvent::ToolCallStart {
                id,
                name,
                initial_arguments,
            } => {
                tool_call_events += 1;
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: initial_arguments,
                });
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                delta,
                name,
            } => {
                tool_call_events += 1;
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
                tool_call_events += 1;
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
            StreamEvent::ModelUsed { model } => {
                // Last announcement wins: a mid-stream failover re-announces
                // with the entry that ultimately completed the request.
                actual_model = Some(model);
            }
            StreamEvent::Done { reason } => break reason,
            StreamEvent::HttpError { status, message } => {
                return Err(crate::providers::ProviderHttpError { status, message }.into());
            }
            StreamEvent::Error(e) => anyhow::bail!("stream error: {}", e),
        }
    };

    // Log complete tool calls for debugging (not just SSE deltas).
    if !tool_calls.is_empty() {
        for tc in &tool_calls {
            tracing::info!(
                tool_call_id = %tc.id,
                name = %tc.name,
                arguments = %tc.arguments,
                "tool call complete"
            );
        }
    }

    Ok(CollectedResponse {
        text: match user_registry {
            Some(r) => {
                // P4 第二波：整段渲染（含 flush 未闭合前缀）——Done/fallback 同源。
                let mut full = text;
                full.push_str(&ref_renderer.flush());
                crate::agents::mention::render_refs(&full, r.as_ref())
            }
            None => text,
        },
        reasoning_content,
        thinking_signature,
        tool_calls,
        tool_call_events,
        stop_reason,
        usage,
        actual_model,
    })
}

/// Whether an LLM error is worth sleeping and retrying on the *same* model.
///
/// Aligns with [`ClassifiedError::should_same_model_retry`]: short RateLimit,
/// Overloaded, ServerError, Timeout. Billing / long RateLimit / auth / not-found
/// are never same-model-retried.
///
/// Session `/model` override uses this path only — it never degrades to the
/// routing Fallback chain. Prefer `/model off` or `/model <other>` when the
/// pinned model is unavailable.
fn is_transient_llm_error(err: &anyhow::Error) -> bool {
    use crate::providers::{ClassifiedError, ProviderHttpError};
    // HTTP errors: classify via the existing pipeline
    if let Some(http_err) = err.downcast_ref::<ProviderHttpError>() {
        let classified = ClassifiedError::classify("", http_err.status, &http_err.message);
        return classified.should_same_model_retry();
    }
    // Stream errors: SSE interruption, connection drop, etc.
    let msg = err.to_string();
    if msg.starts_with("stream error:") {
        return true;
    }
    if msg.contains("stream chunk timeout") {
        return true;
    }
    if msg.contains("stream stalled") {
        return true;
    }
    if msg.contains("stream total timeout") {
        return true;
    }
    if msg.contains("stream ended without a completion marker") {
        return true;
    }
    false
}

/// Short exponential backoff: 1s, 2s.
/// Gateway blips usually clear in well under a second; total added latency
/// is bounded to ~3s (two retries) for an interactive turn.
fn backoff_duration(attempt: usize) -> std::time::Duration {
    std::time::Duration::from_secs(1u64 << attempt.min(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::context_engine::ContextEngine;
    use crate::agents::resource_provider::ResourceProvider;
    use crate::agents::session::Session;
    use crate::agents::tool_executor::ToolExecutor;
    use crate::agents::{AgentRegistry, LoopBreaker, LoopBreakerConfig};
    use crate::config::agent::{PermissionMode, RunMode};
    use crate::config::sub_agent::SubAgentConfig;
    use crate::providers::{
        ChatModelConfig, ChatProvider, EmbeddingProvider, ImageGenerationProvider,
        ProviderSummary, SearchFallbackEntry, SearchProvider, SttProvider, Tool, ToolResult,
        TtsProvider, VideoGenerationProvider,
    };
    use async_trait::async_trait;
    use parking_lot::RwLock;

    #[test]
    fn fold_view_image_inlines_results_when_tool_absent() {
        let mut asst = ChatMessage::assistant_text("让我看看");
        asst.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "view_image".into(),
            arguments: "{}".into(),
        }]);
        let mut tool_res = ChatMessage::text("tool", "一只红色的猫");
        tool_res.tool_call_id = Some("c1".into());
        let mut messages = vec![ChatMessage::user_text("这是什么"), asst, tool_res];

        // No view_image in tool_specs → fold.
        fold_absent_tool(&mut messages, &[], "view_image", "图片查看结果");

        assert!(
            !messages.iter().any(|m| m.role == "tool"),
            "tool-result message must be dropped"
        );
        assert!(
            messages.iter().all(|m| m
                .tool_calls
                .as_ref()
                .map(|tcs| tcs.iter().all(|tc| tc.name != "view_image"))
                .unwrap_or(true)),
            "no view_image tool call may survive"
        );
        let folded = messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .any(|t| t.contains("一只红色的猫"));
        assert!(
            folded,
            "result text must be inlined onto the assistant message"
        );
    }

    #[test]
    fn fold_view_image_is_noop_when_tool_present() {
        let mut asst = ChatMessage::assistant_text("");
        asst.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "view_image".into(),
            arguments: "{}".into(),
        }]);
        let mut messages = vec![asst];
        let specs = vec![ToolSpec {
            name: "view_image".into(),
            description: None,
            input_schema: serde_json::json!({}),
        }];
        fold_absent_tool(&mut messages, &specs, "view_image", "图片查看结果");
        // Tool declared → calls preserved untouched.
        assert!(
            messages[0]
                .tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.name == "view_image"))
        );
    }

    // (Image placeholdering moved to `providers::media::lower_media_for`, which
    // owns its own unit tests; the agent no longer renders images.)

    fn empty_config() -> SubAgentConfig {
        SubAgentConfig {
            name: "test".into(),
            system_prompt: String::new(),
            tools: Default::default(),
            skills: Default::default(),
            mcp: Default::default(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: Default::default(),
            timeout: None,
        }
    }

    /// A `ProviderRegistry` that errors on everything — enough to satisfy
    /// `AgentRuntime`'s type, never enough to run a real turn.
    struct BailingRegistry;

    #[rustfmt::skip]
    impl ProviderRegistry for BailingRegistry {
        fn get_chat_provider(&self, _c: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> { anyhow::bail!("test stub") }
        fn get_chat_provider_with_hint(&self, _c: Capability, _h: Option<&str>) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> { anyhow::bail!("test stub") }
        fn get_chat_fallback_chain(&self, _c: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>> { anyhow::bail!("test stub") }
        fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)> { anyhow::bail!("test stub") }
        fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)> { anyhow::bail!("test stub") }
        fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)> { anyhow::bail!("test stub") }
        fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)> { anyhow::bail!("test stub") }
        fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)> { anyhow::bail!("test stub") }
        fn get_search_fallback_chain(&self) -> anyhow::Result<Vec<SearchFallbackEntry>> { anyhow::bail!("test stub") }
        fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)> { anyhow::bail!("test stub") }
        fn get_chat_model_config(&self, _m: &str) -> anyhow::Result<&ChatModelConfig> { anyhow::bail!("test stub") }
        fn get_chat_provider_by_model(&self, _m: &str) -> Option<(Arc<dyn ChatProvider>, String)> { None }
        fn get_chat_provider_id_by_model(&self, _m: &str) -> Option<String> { None }
        fn get_chat_media_policy(&self, _m: &str) -> Option<crate::providers::MediaPolicy> { None }
        fn get_chat_routing_models(&self) -> Vec<String> { Vec::new() }
        fn get_all_provider_summaries(&self) -> Vec<ProviderSummary> { Vec::new() }
    }

    /// An `AgentRuntime` whose provider registry always bails — the turn
    /// under test must fail with "test stub" before touching any real LLM.
    fn bailing_runtime() -> AgentRuntime {
        let providers: Arc<dyn ProviderRegistry> = Arc::new(BailingRegistry);
        let tools = Arc::new(crate::agents::ToolRegistry::new());
        let skills = Arc::new(RwLock::new(crate::agents::SkillManager::new()));
        let agents = Arc::new(AgentRegistry::default());
        let resources = ResourceProvider::new(
            Arc::clone(&skills),
            Arc::clone(&agents),
            Vec::new(),
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
            String::new(),
            0,
        );
        let context_engine = Arc::new(ContextEngine::new(
            &crate::config::agent::ContextConfig::default(),
            Arc::clone(&providers),
            resources,
            Arc::clone(&tools),
        ));
        let tool_executor = Arc::new(ToolExecutor::new(30));
        let loop_breaker = Arc::new(LoopBreaker::new(LoopBreakerConfig::default()));
        AgentRuntime::new(
            providers,
            tools,
            skills,
            agents,
            context_engine,
            tool_executor,
            loop_breaker,
        )
    }

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
    fn filter_turn_scoped_tools_hides_send_tools_without_channel() {
        let mut session = Session::new("s".into());
        session.record_inbound(crate::channels::ChannelInboundMessage {
            id: "test".into(),
            sender: crate::channels::MessageSender::new("u"),
            receiver: crate::channels::MessageReceiver::new("s"),
            content: crate::channels::ChannelMessageContent::text("hi"),
            timestamp: 0,
            interruption_scope_id: None,
            silenced_override: None,
            run_mode: Default::default(),
        });
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("send_message")),
            Arc::new(NamedTool("send_media")),
            Arc::new(NamedTool("calculator")),
        ];

        filter_turn_scoped_tools(&mut tools, &session);

        assert_eq!(tool_names(&tools), vec!["calculator"]);
    }

    #[test]
    fn filter_turn_scoped_tools_keeps_send_message_for_sub_agent() {
        // RFC agent-messaging §3.3: a sub-agent gets send_message even with
        // no channel — `recipient` targeting reaches the parent agent via
        // the DelegationEvent channel.
        let mut session = Session::new("s".into());
        session.parent_session_id = Some("parent".into());
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("send_message")),
            Arc::new(NamedTool("send_media")),
            Arc::new(NamedTool("calculator")),
        ];

        filter_turn_scoped_tools(&mut tools, &session);

        assert_eq!(tool_names(&tools), vec!["send_message", "calculator"]);
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
    fn async_delegation_mode_detection() {
        // The boundary logic in Agent::run detects a successful async
        // agent_delegate by parsing the call arguments. This test
        // verifies the JSON parsing logic in isolation (the inline
        // detection mirrors this exactly).
        fn is_async_delegate(name: &str, is_error: bool, arguments: &str) -> bool {
            if name != "agent_delegate" || is_error {
                return false;
            }
            serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| {
                    v.get("mode").and_then(|m| m.as_str()).map(str::to_owned)
                })
                .map(|m| m == "async")
                .unwrap_or(false)
        }

        // async mode → detected
        assert!(is_async_delegate(
            "agent_delegate",
            false,
            r#"{"agent":"coder","task":"x","mode":"async"}"#
        ));
        // sync mode (explicit) → not detected
        assert!(!is_async_delegate(
            "agent_delegate",
            false,
            r#"{"agent":"coder","task":"x","mode":"sync"}"#
        ));
        // mode omitted → not detected (tool rejects missing mode since
        // 2026-08-14 — it became required; the parser treats it as non-async)
        assert!(!is_async_delegate(
            "agent_delegate",
            false,
            r#"{"agent":"coder","task":"x"}"#
        ));
        // error result → not detected even with async mode
        assert!(!is_async_delegate(
            "agent_delegate",
            true,
            r#"{"agent":"coder","task":"x","mode":"async"}"#
        ));
        // different tool → not detected
        assert!(!is_async_delegate(
            "file_read",
            false,
            r#"{"path":"x"}"#
        ));
    }

    #[test]
    fn sessions_yield_detection_logic() {
        // The yield detection in Agent::run (RFC delegation-notice-queue §3.2)
        // triggers a deterministic EndTurn when the model calls sessions_yield
        // without error. has_pending follows async_delegation_spawned. This
        // test verifies the condition logic in isolation (mirrors the inline
        // check exactly).
        fn is_yield(name: &str, is_error: bool) -> bool {
            name == "sessions_yield" && !is_error
        }

        // Normal call → yields EndTurn
        assert!(is_yield("sessions_yield", false));

        // Error result → NOT detected (tool errored, don't truncate)
        assert!(!is_yield("sessions_yield", true));

        // Different tool → not detected
        assert!(!is_yield("calculator", false));
        assert!(!is_yield("agent_delegate", false));

        // When yield triggers without prior async delegation, has_pending is
        // false — plain EndTurn, no suspension. When agent_delegate(async)
        // ran earlier in the same batch, async_delegation_spawned is true, so
        // has_pending is true (suspension proceeds normally).
        let mut async_delegation_spawned = false;
        assert_eq!(async_delegation_spawned, false); // pre-condition

        // Simulate agent_delegate(async) running before sessions_yield
        async_delegation_spawned = true;
        // yield with prior delegation → has_pending = true
        assert_eq!(async_delegation_spawned, true);
    }

    #[test]
    fn fold_send_message_inlines_results_when_tool_absent() {
        let mut asst = ChatMessage::assistant_text("发送一下");
        asst.tool_calls = Some(vec![ToolCall {
            id: "s1".into(),
            name: "send_message".into(),
            arguments: "{}".into(),
        }]);
        let mut tool_res = ChatMessage::text("tool", "已发送消息。");
        tool_res.tool_call_id = Some("s1".into());
        let mut messages = vec![ChatMessage::user_text("发给我"), asst, tool_res];

        fold_absent_tool(&mut messages, &[], "send_message", "消息发送结果");

        assert!(!messages.iter().any(|m| m.role == "tool"));
        assert!(messages.iter().all(|m| {
            m.tool_calls
                .as_ref()
                .map(|tcs| tcs.iter().all(|tc| tc.name != "send_message"))
                .unwrap_or(true)
        }));
        assert!(messages.iter().flat_map(|m| &m.parts).any(|p| match p {
            ContentPart::Text { text } => text.contains("已发送消息"),
            _ => false,
        }));
    }

    #[test]
    fn fold_send_media_legacy_calls_when_tool_absent() {
        let mut asst = ChatMessage::assistant_text("发媒体");
        asst.tool_calls = Some(vec![ToolCall {
            id: "m1".into(),
            name: "send_media".into(),
            arguments: "{}".into(),
        }]);
        let mut tool_res = ChatMessage::text("tool", "已发送媒体。");
        tool_res.tool_call_id = Some("m1".into());
        let mut messages = vec![asst, tool_res];

        fold_absent_tool(&mut messages, &[], "send_media", "媒体发送结果");

        assert!(!messages.iter().any(|m| m.role == "tool"));
        assert!(messages[0].tool_calls.is_none());
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

    fn events_to_stream(
        events: Vec<crate::providers::StreamEvent>,
    ) -> BoxStream<crate::providers::StreamEvent> {
        use futures_util::stream;
        Box::pin(stream::iter(events))
    }

    #[tokio::test]
    async fn collect_stream_accepts_thinking_without_signature() {
        use crate::providers::StreamEvent;

        // Non-Anthropic providers (Xiaomi MiMo, …) speak Anthropic-compatible
        // protocol but never emit signature_delta. Collect must succeed; the
        // anthropic renderer's filter_map drops the unreplayable block at
        // send time so the next turn doesn't 400.
        let s = events_to_stream(vec![
            StreamEvent::Thinking {
                text: "let me think...".into(),
            },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed without signature: {e}"),
        };
        assert_eq!(resp.reasoning_content.as_deref(), Some("let me think..."));
        assert!(resp.thinking_signature.is_none());
    }

    #[tokio::test]
    async fn collect_stream_accepts_thinking_with_signature() {
        use crate::providers::StreamEvent;

        let s = events_to_stream(vec![
            StreamEvent::Thinking {
                text: "thinking...".into(),
            },
            StreamEvent::ThinkingSignature {
                signature: "sig123".into(),
            },
            StreamEvent::Delta {
                text: "hello".into(),
            },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert_eq!(resp.reasoning_content.as_deref(), Some("thinking..."));
        assert_eq!(resp.thinking_signature.as_deref(), Some("sig123"));
        assert_eq!(resp.text, "hello");
    }

    #[tokio::test]
    async fn collect_stream_accepts_no_thinking() {
        use crate::providers::StreamEvent;

        // No thinking at all → no signature needed.
        let s = events_to_stream(vec![
            StreamEvent::Delta { text: "hi".into() },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert!(resp.reasoning_content.is_none());
        assert!(resp.thinking_signature.is_none());
    }

    #[tokio::test]
    async fn collect_stream_captures_model_used() {
        use crate::providers::StreamEvent;

        // The fallback chain announces the actual model before content flows;
        // a mid-stream failover re-announces, and the last one wins.
        let s = events_to_stream(vec![
            StreamEvent::ModelUsed {
                model: "glm-5.2".into(),
            },
            StreamEvent::Delta { text: "hi".into() },
            StreamEvent::ModelUsed {
                model: "qwen3.7-plus".into(),
            },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert_eq!(resp.actual_model.as_deref(), Some("qwen3.7-plus"));
        assert_eq!(resp.text, "hi");
    }

    #[tokio::test]
    async fn collect_stream_model_used_absent_when_direct() {
        use crate::providers::StreamEvent;

        // Direct/provider path (no fallback wrapper) emits no ModelUsed;
        // actual_model must stay None so the caller keeps its model_id.
        let s = events_to_stream(vec![
            StreamEvent::Delta { text: "hi".into() },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert!(resp.actual_model.is_none());
    }

    #[tokio::test]
    async fn collect_stream_rejects_truncated_stream() {
        use crate::providers::StreamEvent;

        // Stream ends with content but no terminal Done/Error — i.e. the
        // provider connection was closed mid-response. collect_stream must
        // fail the turn rather than persist the partial text as complete.
        let s = events_to_stream(vec![StreamEvent::Delta {
            text: "中方".into(),
        }]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        assert!(
            collect_stream(s, &mut turn_stream, None).await.is_err(),
            "truncated stream (no completion marker) must error"
        );
    }

    #[tokio::test]
    async fn collect_stream_propagates_provider_error() {
        use crate::providers::StreamEvent;

        // A provider that detects truncation emits StreamEvent::Error after
        // the partial deltas; that must surface as a turn failure.
        let s = events_to_stream(vec![
            StreamEvent::Delta {
                text: "partial".into(),
            },
            StreamEvent::Error("stream closed before completion".into()),
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        assert!(
            collect_stream(s, &mut turn_stream, None).await.is_err(),
            "provider Error must fail the turn"
        );
    }

    // ── Exec marker tests ─────────────────────────────────────────────────

    #[test]
    fn exec_marker_write_read_clear_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path();
        let session_id = "test_session_abc";

        // Initially no marker.
        assert!(exec_marker_read(Some(sessions_dir), session_id).is_none());

        // Write a marker.
        std::fs::create_dir_all(sessions_dir.join(crate::ids::bare_dir_name(session_id))).unwrap();
        exec_marker_write(Some(sessions_dir), session_id, "call_xyz");

        // Read it back.
        assert_eq!(
            exec_marker_read(Some(sessions_dir), session_id).as_deref(),
            Some("call_xyz")
        );

        // Clear it.
        exec_marker_clear(Some(sessions_dir), session_id);
        assert!(exec_marker_read(Some(sessions_dir), session_id).is_none());
    }

    #[test]
    fn exec_marker_none_sessions_dir_is_noop() {
        // When sessions_dir is None (tests / CLI), all operations silently
        // do nothing — no panic, no file system access.
        exec_marker_write(None, "any_session", "any_call");
        assert!(exec_marker_read(None, "any_session").is_none());
        exec_marker_clear(None, "any_session");
    }

    #[test]
    fn exec_marker_guard_clears_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path();
        let session_id = "test_guard_session";

        std::fs::create_dir_all(sessions_dir.join(crate::ids::bare_dir_name(session_id))).unwrap();
        exec_marker_write(Some(sessions_dir), session_id, "call_guard");

        {
            let _guard = ExecMarkerGuard {
                sessions_dir: Some(sessions_dir.to_path_buf()),
                session_id: session_id.to_string(),
            };
            // Marker still present inside the scope.
            assert!(exec_marker_read(Some(sessions_dir), session_id).is_some());
        }
        // Guard dropped — marker cleared.
        assert!(exec_marker_read(Some(sessions_dir), session_id).is_none());
    }
}
