//! Tool-batch execution phase (extracted verbatim from `run_inner` in batch 4).
//!
//! Executes every tool call of one LLM response in provider order: emits the
//! `ToolCall`/`ToolResult` events, writes/clears the exec marker, runs the
//! tool, appends the tool_result to history (persisting BEFORE any async
//! notification — RFC §4.5 red line), and applies the four flow-control
//! detectors (loop breaker / shell background spawn / async delegation /
//! `sessions_yield`) plus the hard max-calls limit. The three "strip the
//! remaining unexecuted tool_calls" sites are kept verbatim and independent —
//! merging them is a behavior change (RFC §4.5).

use std::sync::Arc;

use super::exec_marker::{ExecMarkerGuard, exec_marker_write, llm_usage, persist_last};
use super::stream_collect::{CollectedResponse, push_or_drop};
use super::turn_state::TurnState;
use super::Agent;
use crate::agents::error::AgentError;
use crate::agents::loop_breaker::{LoopBreak, LoopBreakerCounter, LoopBreakReason};
use crate::agents::session::Session;
use crate::agents::turn::TurnResult;
use crate::api::turn_event::TurnEvent;
use crate::providers::capability_chat::{ChatMessage, StopReason, ToolSpec};

/// Outcome of one tool batch. `Continue` means the turn loop should call the
/// LLM again with the appended tool_result messages; the other variants are
/// the verbatim early-exit paths of the former inline loop.
pub(super) enum ToolBatchOutcome {
    /// All calls executed (or non-terminal detections only) — loop back to
    /// the next LLM call.
    Continue,
    /// `sessions_yield` was called: deterministic EndTurn (docs/
    /// delegation-notice-queue-rfc.md §3.2).
    EndTurn(Box<TurnResult>),
    /// Loop breaker or hard max-calls limit tripped.
    Abort(AgentError),
}

impl Agent {
    /// Append the assistant message with this batch's tool calls to the
    /// in-flight request `messages` and the persisted history, then execute
    /// each call. Extracted from `run_inner` (batch 4).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_tool_batch(
        &self,
        session: &mut Session,
        response: &CollectedResponse,
        messages: &mut Vec<ChatMessage>,
        runtime: &crate::agents::AgentRuntime,
        model_id: &str,
        permission_mode: &crate::config::agent::PermissionMode,
        allowed_tools: &[Arc<dyn crate::providers::Tool>],
        _tool_specs: &[ToolSpec],
        turn_state: &mut TurnState,
        loop_breaker: &mut LoopBreakerCounter,
    ) -> anyhow::Result<ToolBatchOutcome> {
        // Tool calls present — append assistant message with the calls
        // (preserving thinking content for re-send), execute each tool,
        // append tool_result messages, then loop for the next LLM call.
        let mut assistant_msg = ChatMessage::assistant_text(&response.text);
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        if let Some(ref thinking_text) = response.reasoning_content {
            assistant_msg.parts.insert(
                0,
                crate::providers::ContentPart::Thinking {
                    thinking: thinking_text.clone(),
                    signature: response.thinking_signature.clone(),
                },
            );
        }
        let effective_model: &str = response.actual_model.as_deref().unwrap_or(model_id);
        let usage = llm_usage(
            response,
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

        let tool_executor = &runtime.tool_executor;
        for (i, call) in response.tool_calls.iter().enumerate() {
            // Track memory_manage calls for fork mutual exclusion.
            if call.name == "memory_manage" {
                turn_state.turn_called_memory = true;
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
            if let (Some(ch), Some(rt)) = (session.resolve_channel(), session.reply_target()) {
                ch.on_tool_event(
                    rt,
                    crate::api::message::ToolEvent::Start {
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
                .execute(call, session, Some(permission_mode), allowed_tools)
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
                turn_state.turn_had_error = true;
            }

            // issue #140: detect a shell call that just armed itself for
            // a future completion notice — `state=running` is the header
            // ONLY `background: true`'s immediate return and the
            // timeout-conversion return use (checked via `starts_with`,
            // not `contains`, so a command's own echoed output — which
            // always comes AFTER this header, never before — can't spoof
            // a false match). The race-recovery path (`terminal_result`
            // in shell.rs) and the plain fast-completion path both start
            // with a different `state=` value, so neither is armed.
            if call.name == "shell" && !is_error && result_content.starts_with("state=running") {
                turn_state.shell_pending_spawned = true;
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
                    return Ok(ToolBatchOutcome::Abort(AgentError::LoopBreak { reason }));
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
                    is_error,
                },
            )
            .await;

            // Notify channel of tool call completion (for reply progress).
            // RFC channel-role-split §1.2: resolved from the live
            // registry, not a turn-installed handle.
            if let (Some(ch), Some(rt)) = (session.resolve_channel(), session.reply_target()) {
                ch.on_tool_event(
                    rt,
                    crate::api::message::ToolEvent::End {
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
                    turn_state.async_delegation_spawned = true;
                }
            }

            // sessions_yield (docs/delegation-notice-queue-rfc.md §3.2):
            // explicit hand-off — deterministic EndTurn, discard remaining
            // tool_calls. has_pending reuses async_delegation_spawned (and,
            // issue #140, shell_pending_spawned) so suspension proceeds
            // normally when sub-agents or background shell work were
            // spawned.
            if call.name == "sessions_yield" && !is_error {
                let remaining = response.tool_calls.len() - i - 1;
                if remaining > 0 {
                    session.strip_trailing_tool_calls(remaining);
                }
                tracing::info!(
                    session = %session.id,
                    "sessions_yield called; ending turn deterministically"
                );
                return Ok(ToolBatchOutcome::EndTurn(Box::new(TurnResult {
                    text: response.text.clone(),
                    stop_reason: StopReason::EndTurn,
                    pending_retry: None,
                    has_pending: turn_state.has_pending(),
                })));
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
                return Ok(ToolBatchOutcome::Abort(AgentError::LoopBreak {
                    reason: LoopBreakReason::MaxCalls {
                        count: loop_breaker.total_calls(),
                        limit: loop_breaker.max_tool_calls(),
                    },
                }));
            }
        }

        if turn_state.async_delegation_spawned || turn_state.shell_pending_spawned {
            tracing::debug!(
                session = %session.id,
                parent = session.parent_session_id.as_deref().unwrap_or("none"),
                async_delegation_spawned = turn_state.async_delegation_spawned,
                shell_pending_spawned = turn_state.shell_pending_spawned,
                "async work spawned this turn; has_pending flag set at EndTurn"
            );
        }

        Ok(ToolBatchOutcome::Continue)
    }
}
