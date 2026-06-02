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

use crate::agents::error::AgentError;
use crate::agents::loop_breaker::LoopBreak;
use crate::agents::tokens::estimate_tokens;
use crate::agents::session::Session;
use crate::agents::turn::{TurnContext, TurnResult};
use crate::agents::turn_event::TurnEvent;
use crate::agents::AgentRuntime;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, StopReason, ToolSpec};
use crate::providers::{BoxStream, Capability, ContentPart, StreamEvent, ToolCall};

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
    pub async fn run(
        &self,
        session: &mut Session,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<TurnResult> {
        // Resolve filtered tool view from runtime + per-agent config.
        let allowed_tools = self.allowed_tools(runtime);
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

        // Resolve provider + model.
        let (provider, model_id) = match turn_ctx.model_id {
            Some(m) => runtime
                .providers
                .get_chat_provider_by_model(m)
                .ok_or_else(|| anyhow::anyhow!("model '{}' not found in registry", m))?,
            None => runtime.providers.get_chat_provider(Capability::Chat)?,
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
        // Seed Session.token_tracker from history when fresh — restart
        // restoration is handled by SessionManager (it loads the
        // persisted total directly into the tracker on session load).
        if session.token_tracker.is_fresh() {
            session.token_tracker.seed_from_history(turn_ctx.system_prompt, &session.history);
        }

        // Assemble the LLM request prefix once. Subsequent rebuilds re-clone
        // the session's growing history.
        let system_msg = ChatMessage::system_text(turn_ctx.system_prompt);
        let mut messages: Vec<ChatMessage> = std::iter::once(system_msg.clone())
            .chain(session.history.iter().cloned())
            .collect();
        crate::agents::session::sanitize_history(&mut messages);

        let permission_mode = turn_ctx.permission_mode;
        let mut empty_response_retries: usize = 0;
        const MAX_EMPTY_RETRIES: usize = 3;
        // Image attachment: snapshot URLs / base64 from session.last_message
        // and only attach on the *first* LLM call of the turn — subsequent
        // iterations rebuild `messages` from history which already carries
        // the attached parts.
        let pending_image_urls: Option<Vec<String>> = session
            .last_message
            .as_ref()
            .and_then(|m| m.image_urls.clone());
        let pending_image_b64: Option<Vec<String>> = session
            .last_message
            .as_ref()
            .and_then(|m| m.image_base64.clone());
        let mut images_attached = false;
        let model_supports_images = runtime
            .providers
            .get_chat_model_config(&model_id)
            .map(|cfg| cfg.supports_image_input())
            .unwrap_or(false);

        loop {
            // Shutdown checkpoint between LLM calls (mirrors AgentLoop chat_loop).
            if crate::is_shutting_down() {
                return Ok(TurnResult {
                    text: String::new(),
                    stop_reason: StopReason::EndTurn,
                    pending_retry: None,
                });
            }

            // User-cancel checkpoint: if the session's TurnStream surfaces a
            // CancellationToken, honor it. RFC §7.6 (Phase 1.5): cancel
            // lives on the per-turn stream now, not on Channel directly.
            if let Some(token) = session
                .turn_stream
                .as_ref()
                .and_then(|s| s.cancel_token())
            {
                if token.is_cancelled() {
                    return Ok(TurnResult {
                        text: String::new(),
                        stop_reason: StopReason::EndTurn,
                        pending_retry: None,
                    });
                }
            }

            // Attach pending images to the last user message on the
            // first iteration only. Skipped silently for non-vision
            // models so the LLM doesn't see unsupported parts.
            if !images_attached && model_supports_images {
                if let Some(last_user) =
                    messages.iter_mut().rev().find(|m| m.role == "user")
                {
                    if let Some(urls) = pending_image_urls.as_ref() {
                        for url in urls {
                            last_user.parts.push(ContentPart::ImageUrl {
                                url: url.clone(),
                                detail: crate::providers::ImageDetail::Auto,
                            });
                        }
                    }
                    if let Some(b64s) = pending_image_b64.as_ref() {
                        for b64 in b64s {
                            last_user.parts.push(ContentPart::ImageB64 {
                                b64_json: b64.clone(),
                                media_type: None,
                                detail: crate::providers::ImageDetail::Auto,
                            });
                        }
                    }
                }
            }
            images_attached = true;

            let thinking = turn_ctx.thinking.cloned();
            let req = ChatRequest {
                model: &model_id,
                messages: &messages,
                temperature: None,
                max_tokens: None,
                thinking,
                stop: None,
                seed: None,
                tools: if tool_specs.is_empty() { None } else { Some(&tool_specs) },
                stream: true,
            };

            let stream = provider.chat(req)?;
            let response = collect_stream(stream, &mut session.turn_stream).await?;

            // Update Session.token_tracker from the API response. Target
            // architecture: tokens are tracked solely on the Session (so
            // they survive restarts via `last_total_tokens`); ContextEngine
            // reads the count via `should_compact(total, window)` instead
            // of keeping its own copy.
            if let Some(ref usage) = response.usage {
                let input = usage.input_tokens.unwrap_or(0);
                let output = usage.output_tokens.unwrap_or(0);
                let cached = usage.cached_input_tokens.unwrap_or(0);
                session.token_tracker.update_from_usage(input, output, cached);
                if let Some(ref hook) = session.persist {
                    hook.save_token_count(&session.id, session.token_tracker.total_tokens());
                }
            }

            // Compaction trigger. Read context_window for the active model
            // and ask ContextEngine if we should compact. The actual
            // summarizer call is deferred — execute_compaction needs the
            // tool_specs slice + the session's full state, which agent.rs
            // already has. Failures are logged and the turn continues
            // (AgentLoop has the same forgive-and-proceed policy).
            if let Ok(cfg) = runtime.providers.get_chat_model_config(&model_id) {
                if let Some(window) = cfg.context_window {
                    let total = session.token_tracker.total_tokens();
                    if context.should_compact(total, window) {
                        tracing::info!(
                            session = %session.id,
                            total,
                            window,
                            "context threshold crossed, attempting compaction"
                        );
                        let sys_prompt_tokens = estimate_tokens(turn_ctx.system_prompt);
                        let tool_spec_tokens: u64 = tool_specs
                            .iter()
                            .map(|s| {
                                estimate_tokens(&s.name)
                                    + s.description.as_deref().map_or(0, estimate_tokens)
                                    + estimate_tokens(&s.input_schema.to_string())
                                    + 8
                            })
                            .sum();
                        if let Some(boundary) = context.compaction_boundary(
                            &session.history,
                            window,
                            sys_prompt_tokens,
                            tool_spec_tokens,
                        ) {
                            // Snapshot history for the summarizer (which reads
                            // a slice). We pass the *current* session to the
                            // memory tools inside the summarizer.
                            let history_snap: Vec<ChatMessage> = session.history.to_vec();
                            match context
                                .execute_compaction(
                                    &history_snap,
                                    turn_ctx.system_prompt,
                                    &tool_specs,
                                    boundary,
                                    &model_id,
                                    session,
                                )
                                .await
                            {
                                Ok(result) => {
                                    let version = session.compact_version + 1;
                                    let summary_prefix = "[CONTEXT COMPACTION — REFERENCE ONLY] ";
                                    let summary_msg = ChatMessage::user_text(format!(
                                        "{}{}",
                                        summary_prefix, result.summary
                                    ));
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
                                    session.token_tracker.adjust_for_compaction(
                                        result.removed_tokens,
                                        result.summary_tokens,
                                    );
                                    // Rebuild the in-flight `messages` slice
                                    // so the next LLM call sees the compacted
                                    // history.
                                    messages = std::iter::once(ChatMessage::system_text(
                                        turn_ctx.system_prompt,
                                    ))
                                    .chain(session.history.iter().cloned())
                                    .collect();
                                    crate::agents::session::sanitize_history(&mut messages);
                                    tracing::info!(
                                        summary_tokens = result.summary_tokens,
                                        removed_tokens = result.removed_tokens,
                                        "compaction completed"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(err = %e, "compaction failed, continuing");
                                }
                            }
                        }
                    }
                }
            }

            // No tool calls → final response. Persist + return.
            if response.tool_calls.is_empty() {
                // Emit Done event before persisting so the streaming UI gets
                // the final-text signal in the canonical order.
                push_or_drop(
                    &mut session.turn_stream,
                    TurnEvent::Done { text: response.text.clone() },
                )
                .await;
                if response.text.trim().is_empty() {
                    // Empty response: retry up to MAX_EMPTY_RETRIES like
                    // AgentLoop's chat_loop does. The provider sometimes
                    // returns empty on EndTurn / StopSequence (transient)
                    // or MaxTokens (output budget exhausted). We don't yet
                    // do the boosted-max_tokens retry; we just re-call.
                    empty_response_retries += 1;
                    if empty_response_retries > MAX_EMPTY_RETRIES {
                        tracing::warn!(
                            "empty response after {} retries, giving up",
                            MAX_EMPTY_RETRIES
                        );
                        return Ok(TurnResult {
                            text: String::new(),
                            stop_reason: response.stop_reason,
                            pending_retry: Some(last_user_text(session)),
                        });
                    }
                    tracing::warn!(
                        attempt = empty_response_retries,
                        stop = ?response.stop_reason,
                        "empty response, retrying"
                    );
                    continue;
                }
                session.add_assistant(response.text.clone());
                persist_last(session);
                return Ok(TurnResult {
                    text: response.text,
                    stop_reason: response.stop_reason,
                    pending_retry: None,
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
            messages.push(assistant_msg);
            session.add_assistant_with_tools(
                response.text.clone(),
                response.tool_calls.clone(),
                response.reasoning_content.clone(),
                response.thinking_signature.clone(),
            );
            persist_last(session);

            for call in &response.tool_calls {
                // The shared LoopBreakerCounter enforces max_tool_calls;
                // the manual check below is replaced by its `MaxCalls`
                // reason at `record_and_check` below. We keep a tiny
                // early-exit so the per-call tool execution doesn't run
                // when we're already over budget.
                if loop_breaker.total_calls() >= loop_breaker.max_tool_calls() {
                    return Err(anyhow::anyhow!(
                        "tool call limit reached ({}), loop broken",
                        loop_breaker.max_tool_calls()
                    ));
                }

                // Emit ToolCall event before execution (streaming UIs show
                // the call spinner).
                {
                    let args: serde_json::Value = serde_json::from_str(&call.arguments)
                        .unwrap_or(serde_json::Value::Null);
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

                let result = tool_executor
                    .execute(call, session, Some(&permission_mode), &allowed_tools)
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

                match loop_breaker.record_and_check(
                    &call.name,
                    &call.arguments,
                    &result_content,
                ) {
                    LoopBreak::Detected(reason) => {
                        return Err(AgentError::LoopBreak {
                            reason: format!("{:?}", reason),
                        }
                        .into());
                    }
                    LoopBreak::None => {}
                }

                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(is_error);
                messages.push(tool_msg);

                // Emit ToolResult event after execution, before persisting,
                // so the UI updates the call status without waiting for
                // disk I/O.
                push_or_drop(
                    &mut session.turn_stream,
                    TurnEvent::ToolResult {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        output: result_content.clone(),
                    },
                )
                .await;

                session.add_tool_result(call.id.clone(), result_content, is_error);
                persist_last(session);
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
    ///   `run()` so the LLM continues.
    /// - **Case B** — trailing tool_results, no LLM response: just call
    ///   `run()`. The chat loop sends the current history to the LLM
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
            let allowed_tools = self.allowed_tools(runtime);
            let tool_executor = &runtime.tool_executor;

            for call in &pending_calls {
                let result = tool_executor
                    .execute(call, session, Some(&turn_ctx.permission_mode), &allowed_tools)
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
                session.add_tool_result(call.id.clone(), result_content, is_error);
                persist_last(session);
            }
        }

        // Cases B, C, and tail of A: drive Agent.run from the now-well-formed
        // history. The user message (if any) is already in history.
        let tr = self.run(session, turn_ctx, runtime).await?;
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

/// Bundle of fields extracted from one streaming LLM response. Mirrors
/// the shape of `agent_impl::types::CollectedResponse` but is defined
/// here so `agent.rs` doesn't reach into `agent_impl/` internals.
struct CollectedResponse {
    text: String,
    reasoning_content: Option<String>,
    thinking_signature: Option<String>,
    tool_calls: Vec<ToolCall>,
    stop_reason: StopReason,
    usage: Option<crate::providers::ChatUsage>,
}

/// Push a `TurnEvent` to `session.turn_stream`, dropping the stream on
/// permanent transport failure (RFC §7.6.5(a)).
///
/// Without this short-circuit, `Agent::run` would keep generating chunks
/// for a disconnected client and waste LLM output budget. After drop,
/// subsequent `push_or_drop` calls become no-ops; the end-of-turn fallback
/// `send_payload` in `SessionContext::process_turn` then ensures the user
/// still receives the final text via the non-streaming path.
async fn push_or_drop(
    turn_stream: &mut Option<Box<dyn crate::channels::TurnStream>>,
    event: TurnEvent,
) {
    let Some(stream) = turn_stream.as_mut() else { return };
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
async fn collect_stream(
    stream: BoxStream<StreamEvent>,
    turn_stream: &mut Option<Box<dyn crate::channels::TurnStream>>,
) -> anyhow::Result<CollectedResponse> {
    let mut stream = stream;
    let mut text = String::new();
    let mut reasoning_content: Option<String> = None;
    let mut thinking_signature: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage: Option<crate::providers::ChatUsage> = None;
    let mut received_first_chunk = false;

    let stop_reason = loop {
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
                push_or_drop(turn_stream, TurnEvent::Chunk { delta: delta.clone() }).await;
            }
            StreamEvent::Thinking { text: delta } => {
                if !delta.is_empty() {
                    push_or_drop(
                        turn_stream,
                        TurnEvent::Thinking { delta: delta.clone() },
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
            StreamEvent::ToolCallStart { id, name, initial_arguments } => {
                tool_calls.push(ToolCall { id, name, arguments: initial_arguments });
            }
            StreamEvent::ToolCallDelta { id, delta } => {
                if !id.is_empty() {
                    if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                        call.arguments.push_str(&delta);
                    } else {
                        tool_calls.push(ToolCall { id, name: String::new(), arguments: delta });
                    }
                } else if let Some(last) = tool_calls.last_mut() {
                    last.arguments.push_str(&delta);
                }
            }
            StreamEvent::ToolCallEnd { id, name, arguments } => {
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
            StreamEvent::Done { reason } => break reason,
            StreamEvent::HttpError { status, message } => {
                return Err(crate::providers::ProviderHttpError { status, message }.into());
            }
            StreamEvent::Error(e) => anyhow::bail!("stream error: {}", e),
        }
    };

    Ok(CollectedResponse {
        text,
        reasoning_content,
        thinking_signature,
        tool_calls,
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::session::Session;
    use crate::config::sub_agent::SubAgentConfig;

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
        }
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
        assert!(session.channel.is_none());
    }

    fn events_to_stream(events: Vec<crate::providers::StreamEvent>) -> BoxStream<crate::providers::StreamEvent> {
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
            StreamEvent::Thinking { text: "let me think...".into() },
            StreamEvent::Done { reason: crate::providers::StopReason::EndTurn },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream).await {
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
            StreamEvent::Thinking { text: "thinking...".into() },
            StreamEvent::ThinkingSignature { signature: "sig123".into() },
            StreamEvent::Delta { text: "hello".into() },
            StreamEvent::Done { reason: crate::providers::StopReason::EndTurn },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream).await {
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
            StreamEvent::Done { reason: crate::providers::StopReason::EndTurn },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert!(resp.reasoning_content.is_none());
        assert!(resp.thinking_signature.is_none());
    }

    #[tokio::test]
    async fn collect_stream_rejects_truncated_stream() {
        use crate::providers::StreamEvent;

        // Stream ends with content but no terminal Done/Error — i.e. the
        // provider connection was closed mid-response. collect_stream must
        // fail the turn rather than persist the partial text as complete.
        let s = events_to_stream(vec![
            StreamEvent::Delta { text: "中方".into() },
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        assert!(
            collect_stream(s, &mut turn_stream).await.is_err(),
            "truncated stream (no completion marker) must error"
        );
    }

    #[tokio::test]
    async fn collect_stream_propagates_provider_error() {
        use crate::providers::StreamEvent;

        // A provider that detects truncation emits StreamEvent::Error after
        // the partial deltas; that must surface as a turn failure.
        let s = events_to_stream(vec![
            StreamEvent::Delta { text: "partial".into() },
            StreamEvent::Error("stream closed before completion".into()),
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        assert!(
            collect_stream(s, &mut turn_stream).await.is_err(),
            "provider Error must fail the turn"
        );
    }
}
