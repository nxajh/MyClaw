//! `chat_loop` — iterative LLM → tool-call → LLM loop.
//!
//! Extracted from run.rs to keep that file focused on public entry-points
//! and stream-collection helpers.

use super::AgentLoop;
use super::types::{StreamMode, estimate_message_tokens};
use super::super::TurnEvent;
use super::super::loop_breaker::LoopBreak;
use crate::providers::{ChatMessage, ChatRequest, StopReason, ThinkingConfig};
use crate::providers::Capability;

/// Core chat loop: call LLM, handle tool calls, repeat until text response.
pub(super) async fn chat_loop(
    loop_: &mut AgentLoop,
    initial_messages: Vec<ChatMessage>,
    stream_mode: StreamMode,
) -> anyhow::Result<String> {
    let mut tool_calls_count = 0usize;
    let mut retry_count = 0usize;
    let mut empty_response_retries = 0usize;
    let mut boosted_max_tokens = false;
    let mut first_iteration = true;
    let mut images_attached = false;

    // Check if we have pending images that need a vision-capable model.
    let has_images = loop_.request_builder.has_images();

    // Pre-emptive compaction for fallback models: when the primary model is unavailable
    // (rate-limit or server error) the FallbackChatProvider routes to a smaller model
    // whose context window may be exceeded by the current history.
    // Only runs when no model_override is active (overrides bypass the fallback chain).
    if loop_.config.model_override.is_none() {
        if let Err(e) = loop_.maybe_compact_for_fallback().await {
            tracing::warn!(err = %e, "pre-fallback compaction check failed");
        }
    }

    loop {
        // Cancellation checkpoint 3: before next LLM call (top of loop).
        if let StreamMode::Streamed { cancel, .. } = &stream_mode {
            if cancel.is_cancelled() {
                tracing::debug!("turn cancelled before next LLM call");
                return Ok(String::new());
            }
        }

        // Hot switch checkpoint: before next LLM call.
        if crate::is_shutting_down() {
            tracing::debug!("shutdown flag set, exiting at LLM checkpoint");
            return Ok(String::new());
        }

        // 1. Get a chat provider via registry.
        // If model_override is set, use that model directly.
        // If images are pending, prefer a vision-capable model from the fallback chain.
        let (provider, model_id) = if let Some(ref model) = loop_.config.model_override {
            match loop_.registry.get_chat_provider_by_model(model) {
                Some((p, id)) => (p, id),
                None => {
                    tracing::warn!(model = %model, "model_override not found, falling back to default");
                    loop_.registry.get_chat_provider(Capability::Chat)?
                }
            }
        } else if has_images {
            loop_.select_vision_provider().await?
        } else {
            loop_.registry.get_chat_provider(Capability::Chat)?
        };

        // Pre-API compaction check: tool results from the previous round may have
        // pushed context over threshold. Compact before building messages to avoid
        // sending an oversized context. No-op on first iteration.
        if let Err(e) = loop_.maybe_compact(&model_id).await {
            tracing::warn!(err = %e, "pre-API compaction check failed");
        }

        // Use initial_messages on the first iteration (includes system-reminder
        // from AttachmentManager), rebuild on subsequent iterations (after tool
        // calls or compaction).
        let mut messages = if first_iteration {
            first_iteration = false;
            tracing::debug!(
                msg_count = initial_messages.len(),
                "chat_loop: first iteration using initial_messages"
            );
            for (i, m) in initial_messages.iter().enumerate() {
                let text = m.text_content();
                let has_reminder = text.contains("<system-reminder>");
                tracing::debug!(
                    idx = i,
                    role = %m.role,
                    len = text.len(),
                    has_reminder,
                    preview = %text.chars().take(80).collect::<String>(),
                    "chat_loop: initial_messages entry"
                );
            }
            initial_messages.clone()
        } else {
            loop_.request_builder.build(&loop_.session)
        };

        // Attach pending images to the last user message only on the first iteration.
        // Subsequent iterations (after tool calls) rebuild from history which already
        // has the text content; re-attaching would send images repeatedly.
        if !images_attached {
            loop_.attach_images_if_supported(&mut messages, &model_id);
            images_attached = true;
        }

        // 2. Build tool specs from skills manager.
        let tools = loop_.build_tool_specs();

        // 3. Build request.
        // Calculate max_tokens based on context window and current usage.
        // On retry after MaxTokens with empty text, boost the output budget.
        let max_tokens = if boosted_max_tokens {
            loop_.calculate_boosted_max_tokens(&model_id)
        } else {
            loop_.calculate_max_tokens(&model_id)
        };

        // Derive thinking config: session override takes priority over model config.
        let thinking = if let Some(ref t) = loop_.config.thinking_override {
            if t.enabled { Some(t.clone()) } else { None }
        } else {
            loop_.registry.get_chat_model_config(&model_id)
                .ok()
                .and_then(|cfg| {
                    if cfg.reasoning {
                        Some(ThinkingConfig { enabled: true, effort: None })
                    } else {
                        None
                    }
                })
        };

        let req = ChatRequest {
            model: &model_id,
            messages: &messages,
            temperature: None,
            max_tokens,
            thinking,
            stop: None,
            seed: None,
            tools: if tools.is_empty() { None } else { Some(tools.as_slice()) },
            stream: true,
        };

        tracing::debug!(msg_count = messages.len(), tool_count = tool_calls_count, "sending messages to model");

        // 4. Call chat and process stream.
        let stream = provider.chat(req)?;

        // Branch on StreamMode: Collect (existing) vs Streamed (forward events).
        let response = {
            let result = match &stream_mode {
                StreamMode::Collect => loop_.collect_stream(stream).await,
                StreamMode::Streamed { event_tx, cancel } => {
                    loop_.collect_stream_with_events(stream, event_tx, cancel).await
                }
            };
            match result {
                Ok(r) => r,
                Err(e) => {
                    let err_str = e.to_string();
                    // Fallback chain signals: do not restart from the outer loop in
                    // either case — the chain already did everything it could.
                    if err_str.contains(crate::providers::fallback::CHAIN_EXHAUSTED_TAG) {
                        tracing::warn!("fallback chain exhausted all providers, not retrying");
                        return Err(super::super::error::AgentError::ProviderChainExhausted.into());
                    }
                    if err_str.contains(crate::providers::fallback::CHAIN_ALL_COOLING_TAG) {
                        let wait_secs = err_str
                            .rsplit_once("retry in ")
                            .and_then(|(_, rest)| rest.trim_end_matches('s').parse::<u64>().ok())
                            .unwrap_or(0);
                        tracing::warn!(wait_secs, "fallback chain: all providers on cooldown");
                        return Err(super::super::error::AgentError::ProviderChainCooling { wait_secs }.into());
                    }
                    let classified = if let Some(http_err) =
                        e.downcast_ref::<crate::providers::ProviderHttpError>()
                    {
                        crate::providers::ClassifiedError::classify("", http_err.status, &http_err.message)
                    } else {
                        crate::providers::ClassifiedError::from_message(&err_str)
                    };
                    if classified.retryable {
                        match classified.reason {
                            crate::providers::FailoverReason::Timeout => {
                                tracing::error!("stream timeout, giving up");
                                return Err(super::super::error::AgentError::StreamTimeout {
                                    secs: crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT.as_secs(),
                                }.into());
                            }
                            _ => {
                                retry_count += 1;
                                if retry_count > 3 {
                                    tracing::error!(reason = ?classified.reason, "retryable error after 3 attempts, giving up");
                                    return Err(super::super::error::AgentError::RetryExhausted {
                                        attempts: retry_count,
                                        source: e,
                                    }.into());
                                }
                                tracing::warn!(
                                    attempt = retry_count,
                                    reason = ?classified.reason,
                                    "retryable error, retrying"
                                );
                                continue;
                            }
                        }
                    }
                    return Err(e);
                }
            }
        };

        // Cancellation checkpoint 4: after stream collected, before tool loop.
        if let StreamMode::Streamed { cancel, .. } = &stream_mode {
            if cancel.is_cancelled() {
                tracing::debug!("turn cancelled after stream collection");
                return Ok(response.text);
            }
        }

        tracing::debug!(text_len = response.text.len(), tool_calls = response.tool_calls.len(), stop = ?response.stop_reason, "chat stream collected");

        // Record token usage from API response.
        // Real context = input_tokens (new) + cached_input_tokens + output_tokens.
        if let Some(ref usage) = response.usage {
            let cached = usage.cached_input_tokens.unwrap_or(0);
            loop_.context.update_usage(
                usage.input_tokens.unwrap_or(0),
                usage.output_tokens.unwrap_or(0),
                cached,
            );
            tracing::debug!(
                input_tokens = usage.input_tokens.unwrap_or(0),
                cached_tokens = cached,
                output_tokens = usage.output_tokens.unwrap_or(0),
                total_tracked = loop_.context.token_total(),
                "token usage recorded"
            );

            // Persist the precise total so it survives restarts.
            if let Some(ref hook) = loop_.persist_hook {
                hook.save_token_count(&loop_.session.id, loop_.context.token_total());
            }
        }

        // Check compaction using the precise token counts just reported by the API.
        // This eliminates the one-turn delay that results from checking before the
        // API call: we now always have accurate data when deciding to compact.
        if let Err(e) = loop_.maybe_compact(&model_id).await {
            tracing::warn!(err = %e, "compaction failed");
        }

        // 5. No tool calls → return text.
        if response.tool_calls.is_empty() {
            if response.text.is_empty() {
                empty_response_retries += 1;
                if empty_response_retries > 3 {
                    tracing::error!("empty response after 3 retries, giving up");
                    return Ok(String::new());
                }

                match response.stop_reason {
                    StopReason::MaxTokens => {
                        // Output budget exhausted — boost and retry (context-related, not provider failure).
                        tracing::warn!(attempt = empty_response_retries, "output hit max_tokens with no text, boosting output budget for retry");
                        boosted_max_tokens = true;
                    }
                    StopReason::StopSequence | StopReason::EndTurn => {
                        // Model stopped naturally but produced no text — may be a transient issue.
                        tracing::warn!(attempt = empty_response_retries, stop = ?response.stop_reason, "empty response with natural stop, retrying");
                    }
                    _ => {
                        tracing::warn!(attempt = empty_response_retries, stop = ?response.stop_reason, "chat response text is empty, retrying");
                    }
                }
                continue;
            }
            // Streamed mode: send Done event before returning.
            if let StreamMode::Streamed { event_tx, .. } = &stream_mode {
                let _ = event_tx.send(TurnEvent::Done { text: response.text.clone() }).await;
            }
            return Ok(response.text);
        }

        // 6. Tool calls present → execute them and append results.
        for call in &response.tool_calls {
            tracing::debug!(tool = %call.name, id = %call.id, arguments = %call.arguments, "model requested tool call");
        }

        // Build the assistant's tool_calls message to append to conversation.
        // Store in canonical ToolCall format — each provider's build_body()
        // translates to its own wire format.
        let mut assistant_msg = ChatMessage::assistant_text(&response.text);
        assistant_msg.tool_calls = Some(response.tool_calls.clone());

        // If the model emitted thinking content, add it as a Thinking part
        // so it is re-sent to the model on subsequent turns.
        if let Some(ref thinking) = response.reasoning_content {
            use crate::providers::ContentPart;
            assistant_msg.parts.insert(
                0,
                ContentPart::Thinking {
                    thinking: thinking.clone(),
                    signature: response.thinking_signature.clone(),
                },
            );
        }

        messages.push(assistant_msg);

        // Persist assistant message with tool_calls to session history.
        loop_.session.add_assistant_with_tools(
            response.text.clone(),
            response.tool_calls.clone(),
            response.reasoning_content.clone(),
            response.thinking_signature.clone(),
        );

        // Persist assistant tool-call message via hook; capture DB id.
        if let Some(ref hook) = loop_.persist_hook {
            if let Some(msg) = loop_.session.history.last() {
                if let Some(id) = hook.persist_message(&loop_.session.id, msg) {
                    if let Some(last_id) = loop_.session.message_ids.last_mut() {
                        *last_id = id;
                    }
                }
            }
        }

        for call in &response.tool_calls {
            tool_calls_count += 1;

            // Cancellation checkpoint 2: before each tool execution.
            if let StreamMode::Streamed { cancel, event_tx } = &stream_mode {
                if cancel.is_cancelled() {
                    tracing::debug!(tool = %call.name, "turn cancelled before tool execution");
                    let _ = event_tx.send(TurnEvent::Cancelled { partial: response.text.clone() }).await;
                    return Ok(response.text.clone());
                }
                // Send ToolCall event to client.
                let args: serde_json::Value = serde_json::from_str(&call.arguments)
                    .unwrap_or(serde_json::Value::Null);
                let _ = event_tx.send(TurnEvent::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args,
                }).await;
            }

            // Hot switch checkpoint: before tool execution.
            if crate::is_shutting_down() {
                tracing::debug!(tool = %call.name, "shutdown flag set, exiting before tool execution");
                return Ok(String::new());
            }

            // Hard limit check.
            if loop_.config.max_tool_calls > 0
                && tool_calls_count > loop_.config.max_tool_calls
            {
                anyhow::bail!(
                    "Tool call limit reached ({}), loop broken",
                    loop_.config.max_tool_calls
                );
            }

            let result = loop_.execute_tool(call).await;
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

            tracing::debug!(tool = %call.name, success = !is_error, "tool result:\n{}", result_content);

            // Streamed mode: send ToolResult event to client.
            if let StreamMode::Streamed { event_tx, .. } = &stream_mode {
                let _ = event_tx.send(TurnEvent::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    output: result_content.clone(),
                }).await;
            }

            // Loop breaker check.
            match loop_.loop_breaker.record_and_check(&call.name, &call.arguments, &result_content) {
                LoopBreak::Detected(reason) => {
                    tracing::warn!(reason = ?reason, "loop breaker triggered, aborting turn");
                    return Err(crate::agents::error::AgentError::LoopBreak {
                        reason: format!("{:?}", reason),
                    }.into());
                }
                LoopBreak::None => {}
            }

            // Append tool result with tool_call_id and is_error.
            let mut tool_msg = ChatMessage::text("tool", &result_content);
            tool_msg.tool_call_id = Some(call.id.clone());
            tool_msg.is_error = Some(is_error);
            messages.push(tool_msg);

            // Record estimated tokens for the tool result message.
            loop_.context.record_pending(
                estimate_message_tokens(messages.last().unwrap())
            );

            // Persist tool result to session history.
            loop_.session.add_tool_result(call.id.clone(), result_content, is_error);

            // Persist tool result via hook; capture DB id.
            if let Some(ref hook) = loop_.persist_hook {
                if let Some(msg) = loop_.session.history.last() {
                    if let Some(id) = hook.persist_message(&loop_.session.id, msg) {
                        if let Some(last_id) = loop_.session.message_ids.last_mut() {
                            *last_id = id;
                        }
                    }
                }
            }

            // Hot switch checkpoint: after tool execution.
            // SIGUSR1 may arrive during or immediately after `myclaw restart`
            // executes. Check here so we exit before the next tool call in
            // the same batch (e.g. kill/pkill) can run.
            if crate::is_shutting_down() {
                tracing::debug!(
                    tool = %call.name,
                    "shutdown flag set after tool execution, exiting before next tool"
                );
                return Ok(String::new());
            }
        }
    }
}
