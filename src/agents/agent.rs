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
use crate::agents::context_engine::ContextEngine;
use crate::agents::error::AgentError;
use crate::agents::loop_breaker::LoopBreak;
use crate::agents::session::Session;
use crate::agents::tokens::{estimate_history_tokens, estimate_tokens};
use crate::agents::turn::{TurnContext, TurnResult};
use crate::agents::turn_event::TurnEvent;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, StopReason, ToolSpec};
use crate::providers::{BoxStream, Capability, ContentPart, StreamEvent, ToolCall};
use crate::storage::SummaryRecord;

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
        let mut allowed_tools = self.allowed_tools(runtime);
        filter_turn_scoped_tools(&mut allowed_tools, session);
        // Convert capability_tool::ToolSpec → capability_chat::ToolSpec
        // (the LLM request type). Same fields, different module homes —
        // a unification candidate for a separate cleanup.
        let mut tool_specs: Vec<ToolSpec> = allowed_tools
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
        //
        // - No override → the Chat fallback wrapper (fans out across the
        //   configured chain on transient errors).
        // - Override (`/model`) → the raw per-model provider. Transient errors
        //   (502/timeout/SSE interruption) are retried in the LLM call loop
        //   below on the SAME chosen model. We intentionally do NOT fall back
        //   to a different model — the user picked this one.
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

        let permission_mode = turn_ctx.permission_mode;
        let mut empty_response_retries: usize = 0;
        const MAX_EMPTY_RETRIES: usize = 3;
        let mut overflow_retries: usize = 0;
        const MAX_OVERFLOW_RETRIES: usize = 3;
        // Media (images, audio, video) is lowered per concrete model inside the provider
        // layer (each chat provider is wrapped in `MediaLoweringProvider`): a
        // multimodal model gets the real file, while a model without the modality gets
        // a marker. The agent sends the canonical format — session-local files — and
        // never pre-renders them, so whichever model the fallback/override actually
        // serves with sees the right thing. It only owns the retrieval tools
        // (advertise + execute in the loop, since retrieval is a multi-round,
        // model-calling concern): `view_image`, `hear_audio`, and `view_video`.
        //
        // Advertise each tool when its aux model exists in the chain AND this turn
        // carries that media or history already references the tool (the latter
        // keeps it present across a /model switch so prior calls don't become
        // orphan tool calls the provider would reject).
        use crate::providers::capability::Modality;
        advertise_media_tool(
            &mut tool_specs,
            &mut allowed_tools,
            runtime.providers.as_ref(),
            Modality::Image,
            history_has_images(&session.history)
                || history_has_tool_calls(&session.history, "view_image"),
            || {
                Arc::new(crate::tools::ViewImageTool::new(Arc::clone(
                    &runtime.providers,
                )))
            },
        );
        advertise_media_tool(
            &mut tool_specs,
            &mut allowed_tools,
            runtime.providers.as_ref(),
            Modality::Audio,
            history_has_audio(&session.history)
                || history_has_tool_calls(&session.history, "hear_audio"),
            || {
                Arc::new(crate::tools::HearAudioTool::new(Arc::clone(
                    &runtime.providers,
                )))
            },
        );
        advertise_media_tool(
            &mut tool_specs,
            &mut allowed_tools,
            runtime.providers.as_ref(),
            Modality::Video,
            history_has_video(&session.history)
                || history_has_tool_calls(&session.history, "view_video"),
            || {
                Arc::new(crate::tools::ViewVideoTool::new(Arc::clone(
                    &runtime.providers,
                )))
            },
        );
        // If a media tool isn't declared this turn but history references it, fold
        // those calls to text so no orphan tool call reaches the provider.
        fold_absent_tool(&mut messages, &tool_specs, "view_image", "图片查看结果");
        fold_absent_tool(&mut messages, &tool_specs, "hear_audio", "语音内容");
        fold_absent_tool(&mut messages, &tool_specs, "view_video", "视频查看结果");
        fold_absent_tool(&mut messages, &tool_specs, "send_message", "消息发送结果");
        fold_absent_tool(&mut messages, &tool_specs, "send_media", "媒体发送结果");

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
            if let Some(token) = session.turn_stream.as_ref().and_then(|s| s.cancel_token()) {
                if token.is_cancelled() {
                    return Ok(TurnResult {
                        text: String::new(),
                        stop_reason: StopReason::EndTurn,
                        pending_retry: None,
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
            if let Some(compacted) = compact_until_fit(
                context,
                session,
                runtime,
                turn_ctx.system_prompt,
                &model_id,
                &tool_specs,
                false,
            )
            .await
            {
                messages = compacted;
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
                    match collect_stream(stream, &mut session.turn_stream).await {
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
                    compact_until_fit(
                        context,
                        session,
                        runtime,
                        turn_ctx.system_prompt,
                        &model_id,
                        &tool_specs,
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
                        });
                    }
                }
            }

            // No tool calls → final response. Persist + return.
            if response.tool_calls.is_empty() {
                // Emit Done event before persisting so the streaming UI gets
                // the final-text signal in the canonical order.
                push_or_drop(
                    &mut session.turn_stream,
                    TurnEvent::Done {
                        text: response.text.clone(),
                    },
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

            for (i, call) in response.tool_calls.iter().enumerate() {
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

                match loop_breaker.record_and_check(&call.name, &call.arguments, &result_content) {
                    LoopBreak::Detected(reason) => {
                        // Record-and-check triggered: append the result for
                        // this call so the pair is complete, then strip any
                        // remaining unexecuted tool_calls and abort.
                        let mut tool_msg = ChatMessage::text("tool", &result_content);
                        tool_msg.tool_call_id = Some(call.id.clone());
                        tool_msg.is_error = Some(is_error);
                        messages.push(tool_msg);
                        session.add_tool_result(call.id.clone(), result_content.clone(), is_error);

                        let remaining = response.tool_calls.len() - i - 1;
                        if remaining > 0 {
                            session.strip_trailing_tool_calls(remaining);
                        }
                        persist_last(session);
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

                // After executing this call, check if we've hit the hard
                // limit. If so, strip remaining unexecuted tool_calls and
                // abort — avoids orphan tool_calls in history.
                if loop_breaker.total_calls() >= loop_breaker.max_tool_calls() {
                    let remaining = response.tool_calls.len() - i - 1;
                    if remaining > 0 {
                        session.strip_trailing_tool_calls(remaining);
                        persist_last(session);
                    }
                    return Err(anyhow::anyhow!(
                        "tool call limit reached ({}), loop broken",
                        loop_breaker.max_tool_calls()
                    ));
                }
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

fn filter_turn_scoped_tools(
    allowed_tools: &mut Vec<Arc<dyn crate::providers::Tool>>,
    session: &Session,
) {
    allowed_tools.retain(|tool| match tool.name() {
        "send_message" => {
            let Some(channel) = session.channel.as_ref() else {
                return false;
            };
            let has_receiver = session.reply_target().is_some();
            let has_text_send = has_receiver;
            let has_file_send = channel.capabilities().supports_file_send;
            has_text_send || has_file_send
        }
        "send_media" => false,
        _ => true,
    });
}

/// True if any message in `history` carries an image part.
fn history_has_images(history: &[ChatMessage]) -> bool {
    history.iter().any(|m| {
        m.parts.iter().any(|p| match p {
            ContentPart::File {
                path, mime_type, ..
            } => {
                crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                    == crate::providers::media::FileModality::Image
            }
            _ => false,
        })
    })
}

/// True if any message in `history` carries an audio part.
fn history_has_audio(history: &[ChatMessage]) -> bool {
    history.iter().any(|m| {
        m.parts.iter().any(|p| match p {
            ContentPart::File {
                path, mime_type, ..
            } => {
                crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                    == crate::providers::media::FileModality::Audio
            }
            _ => false,
        })
    })
}

/// True if any message in `history` carries a video part.
fn history_has_video(history: &[ChatMessage]) -> bool {
    history.iter().any(|m| {
        m.parts.iter().any(|p| match p {
            ContentPart::File {
                path, mime_type, ..
            } => {
                crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                    == crate::providers::media::FileModality::Video
            }
            _ => false,
        })
    })
}

/// True if `history` contains a prior tool call named `name`. Used to keep a
/// media-retrieval tool present after a model switch so those calls don't become
/// orphan tool calls (declared in history but absent from the request's tools).
fn history_has_tool_calls(history: &[ChatMessage], name: &str) -> bool {
    history.iter().any(|m| {
        m.role == "assistant"
            && m.tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.name == name))
    })
}

/// Advertise (and make dispatchable) a media-retrieval tool when `want` (this
/// turn carries that media or history references the tool) AND its aux model
/// exists in the chain. The tool is only constructed when it will be offered.
fn advertise_media_tool(
    tool_specs: &mut Vec<ToolSpec>,
    allowed_tools: &mut Vec<Arc<dyn crate::providers::Tool>>,
    providers: &dyn crate::providers::ProviderRegistry,
    modality: crate::providers::capability::Modality,
    want: bool,
    make: impl FnOnce() -> Arc<dyn crate::providers::Tool>,
) {
    if !want || providers.find_chat_model_with_modality(modality).is_none() {
        return;
    }
    let tool = make();
    tool_specs.push(ToolSpec {
        name: tool.name().to_string(),
        description: Some(tool.description().to_string()),
        input_schema: tool.parameters_schema(),
    });
    allowed_tools.push(tool);
}

/// Backstop for a rare edge (e.g. config hot-reload drops the aux model
/// mid-session): if the request won't declare `tool_name` but history still
/// references it, fold each such call + its result into inline `[label]: …` text
/// on the calling assistant message and drop the tool-result message, so no
/// orphan tool call survives to be rejected by the provider. No-op when the tool
/// is declared. Operates on the cloned `messages` only.
fn fold_absent_tool(
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
async fn maybe_compact(
    context: &ContextEngine,
    session: &mut Session,
    runtime: &AgentRuntime,
    system_prompt: &str,
    model_id: &str,
    tool_specs: &[ToolSpec],
    force: bool,
    override_retain: Option<usize>,
) -> Option<(Vec<ChatMessage>, u64, u64)> {
    let cfg = runtime.providers.get_chat_model_config(model_id).ok()?;
    let window = cfg.context_window?;
    let estimate = estimate_history_tokens(system_prompt, &session.history);
    if !force && !context.should_compact(estimate, window) {
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
    let boundary = context.compaction_boundary_with_retain(
        &session.history,
        window,
        sys_prompt_tokens,
        tool_spec_tokens,
        override_retain,
    )?;

    // Snapshot history for the summarizer (which reads a slice); the live
    // session is passed through for the memory tools inside the summarizer.
    let history_snap: Vec<ChatMessage> = session.history.to_vec();
    match context
        .execute_compaction(
            &history_snap,
            system_prompt,
            tool_specs,
            boundary,
            model_id,
            session,
        )
        .await
    {
        Ok(result) => {
            let version = session.compact_version + 1;
            let summary_prefix = "[CONTEXT COMPACTION — REFERENCE ONLY] ";
            let summary_msg =
                ChatMessage::user_text(format!("{}{}", summary_prefix, result.summary));
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
            // Persist compaction to disk: save summary metadata to meta.json
            // and archive the old history segment (rotate_history writes surviving
            // messages to a fresh history.jsonl so the next restart loads the
            // compacted state instead of the full un-compacted history).
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
                // Line numbers restart at 1 after rotation; remap IDs.
                for (i, id) in session.message_ids.iter_mut().enumerate() {
                    *id = (i + 1) as i64;
                }
            }
            let mut messages: Vec<ChatMessage> =
                std::iter::once(ChatMessage::system_text(system_prompt))
                    .chain(session.history.iter().cloned())
                    .collect();
            crate::agents::session::sanitize_history(&mut messages);
            fold_absent_tool(&mut messages, tool_specs, "view_image", "图片查看结果");
            fold_absent_tool(&mut messages, tool_specs, "hear_audio", "语音内容");
            fold_absent_tool(&mut messages, tool_specs, "view_video", "视频查看结果");
            fold_absent_tool(&mut messages, tool_specs, "send_message", "消息发送结果");
            fold_absent_tool(&mut messages, tool_specs, "send_media", "媒体发送结果");
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
            tracing::warn!(err = %e, "compaction failed, continuing");
            None
        }
    }
}

/// Compact repeatedly until the history estimate drops below the compaction
/// threshold, or no further progress is possible.
///
/// A single `maybe_compact` pass folds only a bounded prefix (≤ the compaction
/// budget, ~window×threshold) into the rolling summary, so a history far over
/// the window — e.g. 934K against a 262K window — used to shrink by one chunk
/// per *user turn*, taking 6+ turns (and 6+ stalls) to converge. Looping the
/// passes within one turn drives it under threshold before we send, while
/// keeping each summary's input bounded (so per-summary fidelity is unchanged).
///
/// `force` applies only to the first pass (the provider-overflow backstop knows
/// the request overflowed even when our estimate sits under threshold); later
/// passes are gated by `should_compact`, so the loop stops the moment we fit.
/// Each `Some` pass strictly shrinks history (one fewer work unit), so the loop
/// terminates; `MAX_PASSES` is a defensive cap against pathological summary
/// growth.
#[allow(clippy::too_many_arguments)]
async fn compact_until_fit(
    context: &ContextEngine,
    session: &mut Session,
    runtime: &AgentRuntime,
    system_prompt: &str,
    model_id: &str,
    tool_specs: &[ToolSpec],
    force: bool,
) -> Option<Vec<ChatMessage>> {
    const MAX_PASSES: usize = 10;
    let configured_retain = context.retain_work_units();
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
        match maybe_compact(
            context,
            session,
            runtime,
            system_prompt,
            model_id,
            tool_specs,
            force_pass,
            override_retain,
        )
        .await
        {
            Some((messages, removed, summary)) => {
                // Stall detection: if net savings < 5% of the removed tokens
                // (or effectively zero), we're re-summarising the same old
                // summary. Lower retain_work_units to expand the compactable
                // range, down to a minimum of 1.
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
    latest
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
                push_or_drop(
                    turn_stream,
                    TurnEvent::Chunk {
                        delta: delta.clone(),
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

/// Whether an LLM error is transient and worth retrying on the same model.
/// Reuses the existing error classification from `error_class.rs`.
fn is_transient_llm_error(err: &anyhow::Error) -> bool {
    use crate::providers::{ClassifiedError, ErrorCategory, ProviderHttpError};
    // HTTP errors: classify via the existing pipeline
    if let Some(http_err) = err.downcast_ref::<ProviderHttpError>() {
        let classified = ClassifiedError::classify("agent", http_err.status, &http_err.message);
        return matches!(
            classified.category,
            ErrorCategory::ServerError | ErrorCategory::Overloaded | ErrorCategory::Timeout
        );
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
    use crate::agents::session::Session;
    use crate::config::sub_agent::SubAgentConfig;
    use crate::providers::{Tool, ToolResult};
    use async_trait::async_trait;

    fn img_msg(path: &str) -> ChatMessage {
        let mut m = ChatMessage::user_text("");
        m.parts = vec![
            ContentPart::Text {
                text: "看图".into(),
            },
            ContentPart::File {
                path: path.into(),
                mime_type: Some("image/png".into()),
                name: None,
                size_bytes: None,
            },
        ];
        m
    }

    #[test]
    fn history_has_images_detects_image_parts() {
        assert!(!history_has_images(&[ChatMessage::user_text("hi")]));
        assert!(history_has_images(&[img_msg("AAA")]));
    }

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

    #[test]
    fn history_has_tool_calls_detects_prior_tool_use() {
        let mut asst = ChatMessage::assistant_text("");
        asst.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "view_image".into(),
            arguments: "{}".into(),
        }]);
        assert!(history_has_tool_calls(&[asst], "view_image"));

        let mut other = ChatMessage::assistant_text("");
        other.tool_calls = Some(vec![ToolCall {
            id: "c2".into(),
            name: "calculator".into(),
            arguments: "{}".into(),
        }]);
        assert!(!history_has_tool_calls(&[other], "view_image"));
        assert!(!history_has_tool_calls(
            &[ChatMessage::user_text("hi")],
            "view_image"
        ));
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
        }
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
        session.record_inbound(crate::channels::ChannelMessage::new("u", "hi"));
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("send_message")),
            Arc::new(NamedTool("send_media")),
            Arc::new(NamedTool("calculator")),
        ];

        filter_turn_scoped_tools(&mut tools, &session);

        assert_eq!(tool_names(&tools), vec!["calculator"]);
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
        assert!(session.channel.is_none());
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
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
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
        let s = events_to_stream(vec![StreamEvent::Delta {
            text: "中方".into(),
        }]);
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
            StreamEvent::Delta {
                text: "partial".into(),
            },
            StreamEvent::Error("stream closed before completion".into()),
        ]);
        let mut turn_stream: Option<Box<dyn crate::channels::TurnStream>> = None;
        assert!(
            collect_stream(s, &mut turn_stream).await.is_err(),
            "provider Error must fail the turn"
        );
    }
}
