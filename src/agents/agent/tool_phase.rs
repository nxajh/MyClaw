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

            // Guard constructed BEFORE the execute().await, not after: if
            // the task running this loop is itself cancelled while
            // execute() is in flight (e.g. an outer delegation timeout, or
            // a suspending shell background-spawn boundary), the guard
            // must already exist so its Drop still clears the marker.
            // Constructing it only after the await returned left a gap
            // where such a cancellation skipped construction entirely and
            // stranded `.exec_marker` with no corresponding daemon
            // restart (issue #232).
            //
            // Scoped to a block that ends the instant execute() returns —
            // not the whole loop body — so the guard's Drop (and the
            // marker clear it performs) happens immediately, before the
            // result-processing/persist/event-notification code below runs.
            // A wider scope left a "call already completed but marker still
            // present" window: if the daemon died anywhere in that stretch
            // (e.g. a std::process::exit from hot-switch's drain timeout,
            // which can fire on another thread at any instant, not just at
            // an await point), the marker would outlive the call it
            // described and could later be misattributed to a different,
            // unrelated orphan call in a future crash (issue #232).
            let result = {
                let _marker_guard = ExecMarkerGuard {
                    sessions_dir: runtime.sessions_dir.clone(),
                    session_id: session.id.clone(),
                };
                tool_executor
                    .execute(call, session, Some(permission_mode), allowed_tools)
                    .await
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

            // issue #238: `sessions_yield`'s own result is deliberately NOT
            // appended/persisted here — its tool_call is left unfulfilled in
            // history so whatever wakes this session next (a sub-agent/shell
            // completion, a new user message) can be delivered as ITS
            // tool_result instead of a synthesized `[user]` message. See
            // `Session::pending_yield` and the EndTurn branch below.
            let is_deferred_yield = call.name == "sessions_yield" && !is_error;

            if !is_deferred_yield {
                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(is_error);
                messages.push(tool_msg);
            }

            // Persist the tool result to disk BEFORE any async
            // notification. persist_last is synchronous (no await
            // points), so once execute() returns we can write the
            // result immediately without risk of task cancellation
            // at a later await point (push_or_drop, on_tool_event).
            // If the task is cancelled during a downstream await,
            // the result is already safe on disk.
            if is_deferred_yield {
                // issue #238 (known gap, disclosed): a NEW user message can
                // arrive and start a fresh turn while an earlier pending
                // yield (explicit or implicit) is still outstanding — if
                // THIS turn also calls sessions_yield, this silently
                // orphans the earlier tool_call_id (it never gets filled,
                // stays unfulfilled in history forever, only picked up by
                // run_recovery's generic Case A on a future restart). Not
                // fixed here — would need either a list of pending yields
                // instead of one, or the new yield absorbing/taking over
                // the old one's role. Logged so it's at least observable.
                if let Some(orphaned) = &session.pending_yield {
                    tracing::warn!(
                        session = %session.id,
                        orphaned_tool_call_id = %orphaned.tool_call_id,
                        new_tool_call_id = %call.id,
                        "issue #238: a new sessions_yield is overwriting an already-pending one; \
                         the orphaned tool_call will never be filled by try_fill_pending_yield"
                    );
                }
                session.pending_yield = Some(crate::agents::session::PendingYield {
                    tool_call_id: call.id.clone(),
                    implicit: false,
                });
            } else {
                session.add_tool_result(call.id.clone(), &call.name, result_content.clone(), is_error);
                persist_last(session);
            }

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::agents::agent::exec_marker::exec_marker_read;
    use crate::agents::agent::tests::{bailing_runtime, empty_config};
    use crate::agents::session::Session;
    use crate::agents::LoopBreakerConfig;
    use crate::config::agent::PermissionMode;
    use crate::providers::capability_chat::ToolCall;
    use crate::providers::{Tool, ToolResult};

    /// A tool whose `execute()` never returns within the test's timeout —
    /// stands in for any long-running tool call (delegation, shell, http)
    /// that an outer cancellation (delegation timeout, task abort) can cut
    /// off mid-flight.
    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }
        fn description(&self) -> &str {
            "slow"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::api::tool::ToolContext,
        ) -> anyhow::Result<ToolResult> {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    /// issue #232: `.exec_marker` must be cleared even when the task
    /// running `execute_tool_batch` is cancelled WHILE `tool_executor
    /// .execute()` is still in flight — not just after it returns
    /// normally. Before the fix, `ExecMarkerGuard` was constructed only
    /// after the `execute().await` resolved, so a cancellation during that
    /// await skipped guard construction entirely and stranded
    /// `.exec_marker` forever, even with no daemon restart in sight.
    #[tokio::test]
    async fn exec_marker_cleared_when_cancelled_mid_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().to_path_buf();
        let session_id = "test_cancel_mid_execute".to_string();
        std::fs::create_dir_all(
            sessions_dir.join(crate::ids::bare_dir_name(&session_id)),
        )
        .unwrap();

        let mut session = Session::new(session_id.clone());
        let runtime = bailing_runtime().with_sessions_dir(sessions_dir.clone());
        let agent = Agent::new(empty_config());

        let response = CollectedResponse {
            text: String::new(),
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "slow".into(),
                arguments: "{}".into(),
            }],
            tool_call_events: 1,
            stop_reason: StopReason::ToolUse,
            usage: None,
            actual_model: None,
        };
        let mut messages: Vec<ChatMessage> = Vec::new();
        let allowed_tools: Vec<Arc<dyn crate::providers::Tool>> = vec![Arc::new(SlowTool)];
        let mut turn_state = TurnState::default();
        let mut loop_breaker = LoopBreakerCounter::new(LoopBreakerConfig::default());

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            agent.execute_tool_batch(
                &mut session,
                &response,
                &mut messages,
                &runtime,
                "test-model",
                &PermissionMode::Full,
                &allowed_tools,
                &[],
                &mut turn_state,
                &mut loop_breaker,
            ),
        )
        .await;

        assert!(
            result.is_err(),
            "expected the timeout to cut execute_tool_batch off mid-tool-execution"
        );
        assert!(
            exec_marker_read(Some(&sessions_dir), &session_id).is_none(),
            "exec_marker must be cleared even when the batch is cancelled while \
             the tool call is still executing, not only when execute() returns"
        );
    }

    /// A `PersistHook` that records, at the moment `persist_message` is
    /// called for a `tool`-role message, whether `.exec_marker` was still
    /// present on disk — used to prove the guard's Drop (and the marker
    /// clear it performs) happens before persistence, not after.
    struct MarkerObservingPersistHook {
        sessions_dir: std::path::PathBuf,
        marker_present_at_tool_persist: std::sync::Mutex<Option<bool>>,
    }

    impl crate::agents::session::backend::PersistHook for MarkerObservingPersistHook {
        fn persist_message(
            &self,
            session_id: &str,
            message: &ChatMessage,
        ) -> Option<i64> {
            if message.role == "tool" {
                let present = exec_marker_read(Some(&self.sessions_dir), session_id).is_some();
                *self.marker_present_at_tool_persist.lock().unwrap() = Some(present);
            }
            None
        }
        fn save_file(
            &self,
            _session_id: &str,
            _preferred_name: Option<&str>,
            _bytes: &[u8],
            _mime_type: Option<&str>,
        ) -> Option<crate::storage::SavedSessionFile> {
            None
        }
        fn save_compaction(&self, _session_id: &str, _summary: &crate::storage::SummaryRecord) {}
        fn rotate_history(&self, _session_id: &str, _surviving: &[(i64, ChatMessage)]) {}
        fn save_token_count(&self, _session_id: &str, _total: u64) {}
        fn save_session_override(&self, _session_id: &str, _override_json: &str) {}
        fn save_last_message(
            &self,
            _session_id: &str,
            _msg: &crate::api::message::PersistedChannelMessage,
        ) {
        }
        fn truncate_messages(&self, _session_id: &str, _keep_count: usize) {}
    }

    /// issue #232: the exec-marker guard must be scoped to end the instant
    /// `execute()` returns, not the whole loop body — otherwise a call that
    /// has already completed (its side effect done) still leaves the marker
    /// on disk through result-processing and persistence, which could later
    /// misattribute that marker to a different, unrelated orphan call if the
    /// daemon dies before this loop iteration's scope naturally closes.
    #[tokio::test]
    async fn exec_marker_cleared_before_result_is_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().to_path_buf();
        let session_id = "test_marker_before_persist".to_string();
        std::fs::create_dir_all(
            sessions_dir.join(crate::ids::bare_dir_name(&session_id)),
        )
        .unwrap();

        let mut session = Session::new(session_id.clone());
        let hook = Arc::new(MarkerObservingPersistHook {
            sessions_dir: sessions_dir.clone(),
            marker_present_at_tool_persist: std::sync::Mutex::new(None),
        });
        session.persist = Some(hook.clone());

        let runtime = bailing_runtime().with_sessions_dir(sessions_dir.clone());
        let agent = Agent::new(empty_config());

        let response = CollectedResponse {
            text: String::new(),
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: vec![ToolCall {
                id: "call_persist_order".into(),
                name: "instant".into(),
                arguments: "{}".into(),
            }],
            tool_call_events: 1,
            stop_reason: StopReason::ToolUse,
            usage: None,
            actual_model: None,
        };
        let mut messages: Vec<ChatMessage> = Vec::new();

        struct InstantTool;
        #[async_trait]
        impl Tool for InstantTool {
            fn name(&self) -> &str {
                "instant"
            }
            fn description(&self) -> &str {
                "instant"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &crate::api::tool::ToolContext,
            ) -> anyhow::Result<ToolResult> {
                Ok(ToolResult {
                    success: true,
                    output: String::new(),
                    error: None,
                })
            }
        }

        let allowed_tools: Vec<Arc<dyn crate::providers::Tool>> = vec![Arc::new(InstantTool)];
        let mut turn_state = TurnState::default();
        let mut loop_breaker = LoopBreakerCounter::new(LoopBreakerConfig::default());

        agent
            .execute_tool_batch(
                &mut session,
                &response,
                &mut messages,
                &runtime,
                "test-model",
                &PermissionMode::Full,
                &allowed_tools,
                &[],
                &mut turn_state,
                &mut loop_breaker,
            )
            .await
            .unwrap();

        assert_eq!(
            *hook.marker_present_at_tool_persist.lock().unwrap(),
            Some(false),
            "exec_marker must already be cleared by the time the tool result is \
             persisted — a wide guard scope would still show it present here"
        );
    }

    /// issue #238: `sessions_yield`'s own tool_result must NOT be appended
    /// to history — its tool_call is left unfulfilled so whatever wakes the
    /// session next can be delivered as ITS result instead of a synthesized
    /// `[user]` message. `Session::pending_yield` records the tool_call_id
    /// so the caller knows where to deliver it.
    #[tokio::test]
    async fn sessions_yield_defers_its_own_tool_result() {
        let mut session = Session::new("test_yield_defers".to_string());
        let runtime = bailing_runtime();
        let agent = Agent::new(empty_config());

        let response = CollectedResponse {
            text: "waiting on sub-agents".to_string(),
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: vec![ToolCall {
                id: "call_yield_1".into(),
                name: "sessions_yield".into(),
                arguments: "{}".into(),
            }],
            tool_call_events: 1,
            stop_reason: StopReason::ToolUse,
            usage: None,
            actual_model: None,
        };
        let mut messages: Vec<ChatMessage> = Vec::new();
        let allowed_tools: Vec<Arc<dyn crate::providers::Tool>> =
            vec![Arc::new(crate::tools::SessionsYieldTool::new())];
        let mut turn_state = TurnState::default();
        let mut loop_breaker = LoopBreakerCounter::new(LoopBreakerConfig::default());

        let outcome = agent
            .execute_tool_batch(
                &mut session,
                &response,
                &mut messages,
                &runtime,
                "test-model",
                &PermissionMode::Full,
                &allowed_tools,
                &[],
                &mut turn_state,
                &mut loop_breaker,
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome, ToolBatchOutcome::EndTurn(_)),
            "sessions_yield must still end the turn deterministically"
        );
        assert!(
            !session
                .history
                .iter()
                .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("call_yield_1")),
            "sessions_yield's own result must not be persisted to history: {:?}",
            session.history
        );
        let pending = session
            .pending_yield
            .as_ref()
            .expect("pending_yield must be set after a sessions_yield call");
        assert_eq!(pending.tool_call_id, "call_yield_1");
        assert!(!pending.implicit, "an explicit model call must not be marked implicit");
    }
}
