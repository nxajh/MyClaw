use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use futures_util::StreamExt;

use crate::providers::{
    BoxStream, ChatMessage, ChatUsage, StopReason, StreamEvent,
};
use super::AgentLoop;
use super::types::{StreamMode, CollectedResponse};
use super::super::TurnEvent;
use super::{turn, chat_loop};

impl AgentLoop {
    /// Apply a new session override to this live agent loop.
    /// Updates the in-flight state so the override takes effect on the next
    /// message without waiting for the loop to be recreated.
    pub fn apply_session_override(&mut self, ov: crate::agents::session::SessionOverride) {
        // Autonomy change: inject a system-reminder so the model learns the new policy
        // on the next turn. The actual hard enforcement is in execute_tool regardless.
        if let Some(ref permission_mode) = ov.permission_mode {
            self.request_builder.diff_autonomy(permission_mode);
        }

        // Apply all config fields via the shared helper (also sets model_override and thinking_override).
        let new_config = self.config.with_override(&ov);
        let new_max = new_config.max_tool_calls;
        self.config = new_config;

        // Rebuild loop breaker when max_tool_calls changed.
        if ov.max_tool_calls.is_some() {
            self.loop_breaker = super::super::loop_breaker::LoopBreaker::new(
                super::super::loop_breaker::LoopBreakerConfig {
                    max_tool_calls: new_max,
                    exact_repeat_threshold: self.config.loop_breaker_threshold,
                    ..super::super::loop_breaker::LoopBreakerConfig::default()
                },
            );
        }

        // Store override in session for next loop_for_with_persist call.
        self.session.session_override = ov;
    }

    /// Process a user message and return the assistant's text response.
    ///
    /// This is the main entry point called by the orchestrator.
    /// Process a user message and return the assistant's text response.
    ///
    /// This is the main entry point used by all existing channels (Telegram, QQ Bot, etc.).
    /// Internally delegates to `run_turn_core` with `StreamMode::Collect`.
    #[tracing::instrument(skip(self, image_urls, image_base64), fields(session = %self.session.id))]
    pub async fn run(&mut self, user_message: &str, image_urls: Option<Vec<String>>, image_base64: Option<Vec<String>>) -> anyhow::Result<String> {
        turn::run_turn_core(self, user_message, image_urls, image_base64, StreamMode::Collect).await
    }

    /// Process a user message with streaming events sent to `event_tx`.
    ///
    /// Used by ClientChannel: the WebSocket handler forwards TurnEvent chunks
    /// to the connected client in real-time. Supports cancellation via `CancellationToken`.
    pub async fn run_streamed(
        &mut self,
        user_message: &str,
        image_urls: Option<Vec<String>>,
        image_base64: Option<Vec<String>>,
        event_tx: mpsc::Sender<TurnEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<String> {
        turn::run_turn_core(
            self,
            user_message,
            image_urls,
            image_base64,
            StreamMode::Streamed { event_tx, cancel },
        ).await
    }

    /// Detect and recover an interrupted turn from session history.
    ///
    /// Two cases are handled:
    ///
    /// **Case A — missing tool results:** The session ends with an assistant
    /// tool_calls message whose tool results were never persisted (process was
    /// killed during tool execution).  We re-execute the missing tools and call
    /// `chat_loop` so the model continues.
    ///
    /// **Case B — missing LLM continuation:** The session ends with complete
    /// tool results but no final assistant response (process was killed after
    /// tool execution finished but before the next LLM call).  We call
    /// `chat_loop` directly so the model generates the final response.
    pub(crate) async fn recover_incomplete_turn(&mut self, stream_mode: &StreamMode) -> anyhow::Result<Option<String>> {
        let history = &self.session.history;
        if history.is_empty() {
            return Ok(None);
        }

        // Walk backwards: collect tool_call_ids that have results,
        // then find tool_calls without results.
        let mut completed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut pending_calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut has_trailing_tool_results = false;

        for msg in history.iter().rev() {
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
                // Stop scanning — we only care about the trailing segment.
                break;
            } else {
                break; // User/system message — no incomplete turn.
            }
        }

        // Case A: assistant tool_calls with missing results → re-execute.
        if !pending_calls.is_empty() {
            tracing::info!(
                missing_count = pending_calls.len(),
                "detected incomplete turn (missing tool results), resuming"
            );

            let mut messages = self.request_builder.build(&self.session);

            for call in &pending_calls {
                tracing::info!(tool = %call.name, id = %call.id, "re-executing interrupted tool call");
                let result = self.execute_tool(call).await;
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

                tracing::info!(tool = %call.name, success = !is_error, "re-executed tool result");

                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(is_error);
                messages.push(tool_msg);

                self.session.add_tool_result(call.id.clone(), result_content, is_error);
                if let Some(ref hook) = self.persist_hook {
                    if let Some(msg) = self.session.history.last() {
                        if let Some(id) = hook.persist_message(&self.session.id, msg) {
                            if let Some(last_id) = self.session.message_ids.last_mut() {
                                *last_id = id;
                            }
                        }
                    }
                }
            }

            let text = chat_loop::chat_loop(self, messages, stream_mode.clone()).await?;
            // Persist the recovered assistant response so the turn is no longer incomplete.
            if !text.is_empty() {
                self.session.add_assistant_text(text.clone());
                if let Some(ref hook) = self.persist_hook {
                    if let Some(msg) = self.session.history.last() {
                        if let Some(id) = hook.persist_message(&self.session.id, msg) {
                            if let Some(last_id) = self.session.message_ids.last_mut() {
                                *last_id = id;
                            }
                        }
                    }
                }
            }
            tracing::info!("interrupted turn resumed (case A: re-executed tools + LLM)");
            return Ok(Some(text));
        }

        // Case B: all tool results present but no final assistant response → call LLM.
        if has_trailing_tool_results && pending_calls.is_empty() {
            tracing::info!("detected incomplete turn (missing LLM continuation), resuming");
            let messages = self.request_builder.build(&self.session);
            let text = chat_loop::chat_loop(self, messages, stream_mode.clone()).await?;
            // Persist the recovered assistant response so the turn is no longer incomplete.
            if !text.is_empty() {
                self.session.add_assistant_text(text.clone());
                if let Some(ref hook) = self.persist_hook {
                    if let Some(msg) = self.session.history.last() {
                        if let Some(id) = hook.persist_message(&self.session.id, msg) {
                            if let Some(last_id) = self.session.message_ids.last_mut() {
                                *last_id = id;
                            }
                        }
                    }
                }
            }
            tracing::info!("interrupted turn resumed (case B: LLM continuation)");
            return Ok(Some(text));
        }

        // Case C: last message is user — daemon was killed before model responded.
        if history.last().is_some_and(|m| m.role == "user") {
            tracing::info!("detected incomplete turn (user message with no assistant response), resuming");
            let messages = self.request_builder.build(&self.session);
            let text = chat_loop::chat_loop(self, messages, stream_mode.clone()).await?;
            if !text.is_empty() {
                self.session.add_assistant_text(text.clone());
                if let Some(ref hook) = self.persist_hook {
                    if let Some(msg) = self.session.history.last() {
                        if let Some(id) = hook.persist_message(&self.session.id, msg) {
                            if let Some(last_id) = self.session.message_ids.last_mut() {
                                *last_id = id;
                            }
                        }
                    }
                }
            }
            tracing::info!("interrupted turn resumed (case C: user→assistant recovery)");
            return Ok(Some(text));
        }

        Ok(None)
    }

    /// Collect all events from a streaming chat response.
    pub(crate) async fn collect_stream(
        &self,
        stream: BoxStream<StreamEvent>,
    ) -> anyhow::Result<CollectedResponse> {
        self.collect_stream_inner(stream, None, None).await
    }

    /// Like `collect_stream`, but also forwards text/thinking chunks as
    /// `TurnEvent`s via `event_tx` and respects `CancellationToken`.
    pub(crate) async fn collect_stream_with_events(
        &self,
        stream: BoxStream<StreamEvent>,
        event_tx: &mpsc::Sender<TurnEvent>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<CollectedResponse> {
        self.collect_stream_inner(stream, Some(event_tx), Some(cancel)).await
    }

    /// Unified stream collector. `event_tx` and `cancel` are both `Some` for the
    /// streaming path, both `None` for the collect-only path.
    async fn collect_stream_inner(
        &self,
        mut stream: BoxStream<StreamEvent>,
        event_tx: Option<&mpsc::Sender<TurnEvent>>,
        cancel: Option<&CancellationToken>,
    ) -> anyhow::Result<CollectedResponse> {
        let mut text = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut thinking_signature: Option<String> = None;
        let mut tool_calls = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage: Option<ChatUsage> = None;

        let first_chunk_timeout = Duration::from_secs(self.config.stream_first_chunk_timeout_secs);
        let max_output_bytes = self.config.max_output_bytes;
        let mut received_first_chunk = false;

        loop {
            // Cancellation checkpoint (streaming path only).
            if let Some(cancel) = cancel {
                if cancel.is_cancelled() {
                    return Ok(CollectedResponse { text, reasoning_content, thinking_signature, tool_calls, stop_reason, usage });
                }
            }

            // Check output length limit
            if text.len() > max_output_bytes {
                tracing::warn!(
                    output_bytes = text.len(),
                    max_bytes = max_output_bytes,
                    "stream output exceeded max size, forcing stop"
                );
                stop_reason = StopReason::MaxTokens;
                break;
            }

            // First chunk: enforce timeout (API can be slow to start on large contexts).
            // Subsequent chunks: no application-level timeout — TCP keepalive detects dead
            // connections at the OS level; a mid-stream gap only happens on network failure,
            // which surfaces as a StreamEvent::Error rather than silence.
            let event_opt = if !received_first_chunk {
                match tokio::time::timeout(first_chunk_timeout, stream.next()).await {
                    Ok(ev) => ev,
                    Err(_) => {
                        anyhow::bail!(
                            "stream chunk timeout after {}s, no data received",
                            first_chunk_timeout.as_secs()
                        );
                    }
                }
            } else {
                stream.next().await
            };

            match event_opt {
                Some(event) => {
                    received_first_chunk = true;
                    match event {
                        StreamEvent::Delta { text: delta } => {
                            text.push_str(&delta);
                            if let Some(tx) = event_tx {
                                if tx.send(TurnEvent::Chunk { delta }).await.is_err() {
                                    anyhow::bail!("Client disconnected during stream");
                                }
                            }
                        }
                        StreamEvent::Thinking { text: delta } => {
                            if !delta.is_empty() {
                                if let Some(rc) = &mut reasoning_content {
                                    rc.push_str(&delta);
                                } else {
                                    reasoning_content = Some(delta.clone());
                                }
                                if let Some(tx) = event_tx {
                                    if tx.send(TurnEvent::Thinking { delta }).await.is_err() {
                                        anyhow::bail!("Client disconnected during stream");
                                    }
                                }
                            }
                        }
                        StreamEvent::ThinkingSignature { signature } => {
                            thinking_signature = Some(signature);
                        }
                        StreamEvent::ToolCallStart { id, name, initial_arguments } => {
                            tool_calls.push(crate::providers::ToolCall {
                                id,
                                name,
                                arguments: initial_arguments,
                            });
                        }
                        StreamEvent::ToolCallDelta { id, delta } => {
                            if !id.is_empty() {
                                if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                                    call.arguments.push_str(&delta);
                                } else {
                                    tool_calls.push(crate::providers::ToolCall {
                                        id: id.clone(),
                                        name: String::new(),
                                        arguments: delta,
                                    });
                                    tracing::debug!(tool_call_id = %id, "auto-created tool call from delta");
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
                            // Merge rather than overwrite: Anthropic sends two Usage events
                            // (message_start with input_tokens, message_delta with output_tokens).
                            if let Some(ref mut existing) = usage {
                                if u.input_tokens.is_some() { existing.input_tokens = u.input_tokens; }
                                if u.output_tokens.is_some() { existing.output_tokens = u.output_tokens; }
                                if u.cached_input_tokens.is_some() { existing.cached_input_tokens = u.cached_input_tokens; }
                                if u.reasoning_tokens.is_some() { existing.reasoning_tokens = u.reasoning_tokens; }
                                if u.cache_write_tokens.is_some() { existing.cache_write_tokens = u.cache_write_tokens; }
                            } else {
                                usage = Some(u);
                            }
                        }
                        StreamEvent::Done { reason } => {
                            stop_reason = reason;
                            break;
                        }
                        StreamEvent::HttpError { status, message } => {
                            return Err(crate::providers::ProviderHttpError { status, message }.into());
                        }
                        StreamEvent::Error(e) => {
                            anyhow::bail!("Stream error: {}", e);
                        }
                    }
                }
                None => {
                    // Stream ended without Done event
                    tracing::warn!("stream ended without Done event");
                    break;
                }
            }
        }

        Ok(CollectedResponse {
            text,
            reasoning_content,
            thinking_signature,
            tool_calls,
            stop_reason,
            usage,
        })
    }

    /// Calculate max_tokens for the current request based on context window.
    pub(super) fn calculate_max_tokens(&self, model_id: &str) -> Option<u32> {
        let model_config = self.registry.get_chat_model_config(model_id).ok()?;
        let context_window = model_config.context_window?;
        let max_output = model_config.max_output_tokens.unwrap_or(4096) as u64;

        let total_tokens = self.policy.token_total();
        let available = context_window.saturating_sub(total_tokens);
        let max = max_output.min(available).min(u32::MAX as u64);

        if max < 256 {
            tracing::warn!(
                model = %model_id,
                context_window,
                total_tokens,
                available,
                "very little context space remaining"
            );
        }

        Some(max.max(256) as u32)
    }

    /// Calculate boosted max_tokens for retry after MaxTokens exhaustion.
    /// Doubles the output budget (up to context window limit).
    pub(super) fn calculate_boosted_max_tokens(&self, model_id: &str) -> Option<u32> {
        let model_config = self.registry.get_chat_model_config(model_id).ok()?;
        let context_window = model_config.context_window?;
        let default_max = model_config.max_output_tokens.unwrap_or(4096) as u64;
        // Double the output budget.
        let boosted = (default_max * 2).min(context_window);

        let total_tokens = self.policy.token_total();
        let available = context_window.saturating_sub(total_tokens);
        let max = boosted.min(available).min(u32::MAX as u64);

        tracing::debug!(
            boosted_max = max,
            available,
            "boosted max_tokens for retry"
        );

        Some(max.max(256) as u32)
    }
}
