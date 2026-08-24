//! Turn finalization (extracted verbatim from `run_inner` in batch 4).
//!
//! Covers the no-tool-calls end-of-turn path: emit `Done` (before persist —
//! RFC §4.5 red line), append + persist the final assistant message, then
//! fire the two background extraction forks (memory + skill) and build the
//! `TurnResult`.

use std::sync::Arc;

use super::exec_marker::{llm_usage, persist_last};
use super::stream_collect::{CollectedResponse, push_or_drop};
use super::turn_state::TurnState;
use crate::agents::context_engine::ContextEngine;
use crate::agents::session::Session;
use crate::agents::turn::{TurnContext, TurnResult};
use crate::agents::turn_event::TurnEvent;
use crate::providers::ProviderRegistry;
use crate::providers::capability_chat::{ChatMessage, StopReason, ToolSpec};

/// Outcome of the context-overflow backstop. Extracted from `run_inner`
/// (batch 4) together with `handle_context_overflow`.
pub(super) enum OverflowRecovery {
    /// Not a `ContextOverflow` stop — caller proceeds normally.
    NotOverflow,
    /// Compaction recovered; caller retries with the compacted messages.
    Compacted(Vec<ChatMessage>),
    /// Compaction could not recover (or retries exhausted); terminal
    /// `TurnResult` for the caller to return.
    GiveUp(TurnResult),
}

impl super::Agent {
    /// Finalize a turn whose LLM response carried no tool calls: push the
    /// `Done` event, persist the final assistant message, spawn the memory /
    /// skill extraction forks, and return the `TurnResult`.
    ///
    /// Extracted from `run_inner` (batch 4). Statement order is part of the
    /// RFC §4.5 red lines — in particular the `Done` event fires BEFORE
    /// `persist_last`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finalize_turn(
        &self,
        session: &mut Session,
        response: &CollectedResponse,
        messages: &[ChatMessage],
        tool_specs: &[ToolSpec],
        runtime: &crate::agents::AgentRuntime,
        model_id: &str,
        provider: &Arc<dyn crate::providers::ChatProvider>,
        turn_state: &TurnState,
        loop_breaker_total_calls: usize,
    ) -> anyhow::Result<TurnResult> {
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
        // neither flag applies there. issue #140: a shell background
        // spawn is an origin turn exactly like a delegation spawn —
        // same preview treatment.
        if (turn_state.async_delegation_spawned || turn_state.shell_pending_spawned)
            && !session.turn_silenced
        {
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
        let effective_model: &str = response.actual_model.as_deref().unwrap_or(model_id);
        msg.model = Some(effective_model.to_string());
        msg.usage = llm_usage(
            response,
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
        if !turn_state.turn_called_memory && session.parent_session_id.is_none() {
            let mut fork_messages = messages.to_vec();
            fork_messages.push(msg.clone());
            let fork_input = crate::agents::memory_fork::ForkInput {
                messages: fork_messages,
                model_id: model_id.to_string(),
                provider: Arc::clone(provider),
                tool_specs: tool_specs.to_vec(),
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
        if !turn_state.turn_had_error
            && loop_breaker_total_calls >= 5
            && session.parent_session_id.is_none()
        {
            let mut skill_messages = messages.to_vec();
            skill_messages.push(msg.clone());
            let skill_input = crate::agents::skill_extract::SkillExtractInput {
                messages: skill_messages,
                model_id: model_id.to_string(),
                provider: Arc::clone(provider),
                tool_specs: tool_specs.to_vec(),
                tool_registry: Arc::clone(&runtime.tools),
                session_id: session.id.clone(),
                base_dir: runtime.defaults.prompt.base_dir.clone(),
                channel: session.resolve_channel(),
                reply_target: session.reply_target().map(str::to_string),
            };
            tokio::spawn(async move {
                crate::agents::skill_extract::run_skill_extract(skill_input).await;
            });
        }

        Ok(TurnResult {
            text: response.text.clone(),
            stop_reason: response.stop_reason,
            pending_retry: None,
            // 单 preview (2026-08-12): an EndTurn on a turn that spawned
            // async delegations is the ORIGIN turn of a suspension
            // sequence — `has_pending` marks it as such so the
            // dispatcher suspends instead of ending. The model may have
            // continued with other work after the spawn (the old forced
            // truncation is gone); its output is the preview content.
            // issue #140: a shell background spawn is the same kind of
            // origin turn.
            has_pending: turn_state.has_pending(),
        })
    }

    /// Context-overflow backstop. The provider rejected the request as
    /// too large (empty body, mapped to `ContextOverflow` instead of a
    /// misleading `EndTurn`). This only happens if the pre-send guard's
    /// estimate undershot the real token count; force a compaction and
    /// retry rather than blindly re-sending the same over-window request.
    ///
    /// Extracted from `run_inner` (batch 4); statement order verbatim.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_context_overflow(
        &self,
        session: &mut Session,
        turn_ctx: &TurnContext<'_>,
        provider: &Arc<dyn crate::providers::ChatProvider>,
        model_id: &str,
        tool_specs: &[ToolSpec],
        context: &ContextEngine,
        runtime: &crate::agents::AgentRuntime,
        is_overflow: bool,
        turn_state: &mut TurnState,
        max_overflow_retries: usize,
    ) -> anyhow::Result<OverflowRecovery> {
        if is_overflow {
            turn_state.overflow_retries += 1;
            let recovered = if turn_state.overflow_retries <= max_overflow_retries {
                context
                    .compact_until_fit(
                        session,
                        turn_ctx.system_prompt,
                        model_id,
                        Arc::clone(provider),
                        tool_specs,
                        runtime.task_boards.as_ref(),
                        true, // force: bypass the threshold, we know it overflowed
                    )
                    .await
            } else {
                None
            };
            match recovered {
                Some(compacted) => {
                    tracing::warn!(
                        attempt = turn_state.overflow_retries,
                        "context overflow reported by provider; compacted and retrying"
                    );
                    Ok(OverflowRecovery::Compacted(compacted))
                }
                None => {
                    // Can't reduce further (history already minimal) or out of
                    // attempts — surface a clear message instead of silently
                    // giving up like the empty-response path would.
                    tracing::warn!(
                        attempt = turn_state.overflow_retries,
                        "context overflow and compaction could not recover; giving up"
                    );
                    let msg = "⚠️ 当前对话已超出该模型的上下文上限，压缩后仍无法容纳。\
                        请使用 /new 开启新会话，或精简后重试。"
                        .to_string();
                    session.add_assistant(msg.clone());
                    persist_last(session);
                    Ok(OverflowRecovery::GiveUp(TurnResult {
                        text: msg,
                        stop_reason: StopReason::ContextOverflow,
                        pending_retry: None,
                        has_pending: false,
                    }))
                }
            }
        } else {
            Ok(OverflowRecovery::NotOverflow)
        }
    }
}
