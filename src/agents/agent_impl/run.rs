use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use futures_util::StreamExt;

use crate::providers::{
    BoxStream, ChatMessage, ChatRequest, ChatUsage, StopReason, StreamEvent, ThinkingConfig,
};
use crate::providers::Capability;
use crate::tools::TaskDelegator;

use super::AgentLoop;
use super::types::{StreamMode, CollectedResponse, estimate_tokens, estimate_message_tokens};
use super::super::TurnEvent;
use super::super::loop_breaker::LoopBreak;

impl AgentLoop {
    /// Apply a new session override to this live agent loop.
    /// Updates the in-flight state so the override takes effect on the next
    /// message without waiting for the loop to be recreated.
    pub fn apply_session_override(&mut self, ov: crate::agents::session_manager::SessionOverride) {
        // Autonomy change: inject a system-reminder so the model learns the new policy
        // on the next turn. The actual hard enforcement is in execute_tool regardless.
        if let Some(ref autonomy) = ov.autonomy {
            self.attachments.diff_autonomy(autonomy);
        }

        self.model_override = ov.model.clone();
        self.thinking_override = ov.to_thinking_config();

        // Apply all config fields via the shared helper.
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
    pub async fn run(&mut self, user_message: &str, image_urls: Option<Vec<String>>, image_base64: Option<Vec<String>>) -> anyhow::Result<String> {
        self.run_turn_core(user_message, image_urls, image_base64, StreamMode::Collect).await
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
        self.run_turn_core(
            user_message,
            image_urls,
            image_base64,
            StreamMode::Streamed { event_tx, cancel },
        ).await
    }

    /// Shared implementation for `run()` and `run_streamed()`.
    ///
    /// Preamble (reset, diff, persist user message) → chat_loop → postamble (persist assistant).
    async fn run_turn_core(
        &mut self,
        user_message: &str,
        image_urls: Option<Vec<String>>,
        image_base64: Option<Vec<String>>,
        stream_mode: StreamMode,
    ) -> anyhow::Result<String> {
        tracing::info!(user_input = %user_message, "user message received");

        // ── Breakpoint recovery: auto-resume interrupted turn ─────────────
        // If the session ends with assistant tool_calls that have no matching
        // tool results (process was killed mid-turn), re-execute the missing
        // tools and let chat_loop continue from there.
        self.recover_incomplete_turn(&stream_mode).await?;

        // Reset loop breaker for new turn.
        self.loop_breaker.reset();

        // Initialize token tracker for fresh session / recovery.
        if self.token_tracker.is_fresh() {
            if let Some(stored) = self.session.last_total_tokens {
                // Precise value persisted from last API response — use directly.
                self.token_tracker.update_from_usage(stored, 0, 0);
            } else {
                // No stored value (brand-new session): estimate from history.
                if !self.system_prompt.is_empty() {
                    self.token_tracker.record_pending(
                        estimate_tokens(&self.system_prompt) + 4
                    );
                }
                for msg in &self.session.history {
                    self.token_tracker.record_pending(estimate_message_tokens(msg));
                }
            }
        }

        // 1. Hot-reload (may update skills/agents) — runs before any diff so that
        //    subsequent diffs see the freshest state.
        self.check_changes();

        // 2. Compute all attachment deltas against current history (before adding
        //    the new user message, so rebuild_from_history sees only prior turns).
        {
            let history = self.session.history.clone();
            {
                let skills = self.skills.read();
                tracing::info!(
                    history_len = history.len(),
                    skill_count = skills.skill_count(),
                    "run: diff_skills"
                );
                self.attachments.diff_skills(&skills, &history);
            }
            if let Some(ref delegator) = self.sub_delegator {
                let agents = delegator.available_agents();
                tracing::info!(agent_count = agents.len(), "run: diff_agents");
                self.attachments.diff_agents(&agents, &history);
            }
            if !self.mcp_instructions.is_empty() {
                tracing::info!(mcp_count = self.mcp_instructions.len(), "run: diff_mcp");
                self.attachments.diff_mcp(&self.mcp_instructions, &history);
            }
            // Memory index — 每 turn 都检查，首次或 compaction 后自动全量注入
            {
                let memory_dir = std::path::Path::new(&self.config.prompt_config.knowledge_dir);
                let files = crate::memory::scan_memory_files(memory_dir);
                let entries: Vec<crate::memory::IndexEntry> =
                    files.iter().map(crate::memory::IndexEntry::from).collect();
                tracing::info!(
                    memory_count = entries.len(),
                    "run: diff_memory"
                );
                self.attachments.diff_memory(&entries, &history);
            }
            let tz = self.config.prompt_config.timezone_offset;
            self.attachments.diff_date(tz, &history);
            tracing::info!(
                pending_keys = ?self.attachments.pending_keys(),
                "run: diff complete"
            );
        }

        // 3. Build attachment text and merge it into the user message.
        //    The <system-reminder> is prepended so the model sees context before input.
        //    Both are persisted as a single history entry — no consecutive user messages.
        let combined_user = {
            let skills = self.skills.read();
            match self.attachments.build_text(&skills) {
                Some(reminder) => {
                    tracing::info!(reminder_len = reminder.len(), "merging attachment into user message");
                    format!("{}\n\n{}", reminder, user_message)
                }
                None => user_message.to_string(),
            }
        };
        self.attachments.clear_pending();

        // 4. Add combined user message to history and persist.
        let user_msg = ChatMessage::user_text(combined_user.clone());
        self.token_tracker.record_pending(estimate_message_tokens(&user_msg));
        // ★ Record snapshot length BEFORE adding user message, so rollback can
        //   undo everything added during this turn (user + assistant/tool_calls/tool_results).
        let turn_snapshot_len = self.session.history.len();
        self.session.add_user_text(combined_user.clone());

        if let Some(ref hook) = self.persist_hook {
            if let Some(msg) = self.session.history.last() {
                if let Some(id) = hook.persist_message(&self.session.id, msg) {
                    if let Some(last_id) = self.session.message_ids.last_mut() {
                        *last_id = id;
                    }
                }
            }
        }

        self.pending_image_urls = image_urls;
        self.pending_image_base64 = image_base64;

        // 5. Build the full message list for this turn (pure: no side effects).
        let messages = self.build_messages().await?;

        // Save a flag for whether we're in streaming mode, so we can send
        // TurnEvent::EmptyResponse after chat_loop takes ownership of stream_mode.
        let is_streamed = matches!(&stream_mode, StreamMode::Streamed { .. });

        // 3. Run the chat loop (handles tool calls iteratively).
        let text = match self.chat_loop(messages, stream_mode).await {
            Ok(text) => text,
            Err(e) => {
                // Roll back turn for ALL errors so the user can retry cleanly.
                tracing::warn!(
                    turn_snapshot_len,
                    current_len = self.session.history.len(),
                    err = %e,
                    "chat_loop failed, rolling back turn"
                );

                // Roll back in-memory history to pre-turn state.
                self.session.rollback_to(turn_snapshot_len);

                // Roll back persisted history.
                if let Some(ref hook) = self.persist_hook {
                    hook.truncate_messages(&self.session.id, turn_snapshot_len);
                }

                // Check if this is a LoopBreak error — re-raise with specific type
                // so the orchestrator can show a tailored retry prompt.
                if let Some(crate::agents::error::AgentError::LoopBreak { reason }) =
                    e.downcast_ref::<crate::agents::error::AgentError>()
                {
                    return Err(crate::agents::error::AgentError::LoopBreak {
                        reason: reason.clone(),
                    }.into());
                }

                // Propagate as-is (already rolled back).
                return Err(e);
            }
        };

        // 5. Handle empty response: rollback turn and return error.
        //    chat_loop retries internally (stream timeout × 3, empty response × 3).
        //    If it still returns empty, the turn is irrecoverable.
        //
        //    BUT: if the empty response is due to a checkpoint exit (SIGUSR1),
        //    skip persistence and return cleanly — let the session stay at the
        //    last tool_result so a new process can resume from the breakpoint.
        if text.is_empty() && crate::is_shutting_down() {
            tracing::info!("checkpoint exit with empty response, skipping persistence");
            return Ok(text);
        }

        if text.is_empty() {
            tracing::warn!(
                turn_snapshot_len,
                current_len = self.session.history.len(),
                "empty response after retries, rolling back turn"
            );

            // For streaming path: notify the client before rollback so the
            // frontend can show retry UI. Note: stream_mode was moved into
            // chat_loop, so we check the pre-saved flag. The TurnEvent is
            // sent by chat_loop internally when it detects cancellation,
            // but for empty response we handle it here via a different path:
            // chat_loop sends TurnEvent::Done only on success, so the client
            // will detect the stream ended without Done and can show retry UI.
            // We also set the pending_retry_message so the orchestrator can
            // offer the retry button.
            if is_streamed {
                tracing::info!("streaming turn had empty response, client will detect via missing Done event");
            }

            // Roll back in-memory history to pre-turn state.
            self.session.rollback_to(turn_snapshot_len);

            // Roll back persisted history.
            if let Some(ref hook) = self.persist_hook {
                hook.truncate_messages(&self.session.id, turn_snapshot_len);
            }

            return Err(crate::agents::error::AgentError::EmptyResponse {
                user_message: combined_user,
            }.into());
        }

        // 5. Persist assistant response.
        self.session.add_assistant_text(text.clone());

        // Persist assistant message via hook; capture the assigned DB id.
        if let Some(ref hook) = self.persist_hook {
            if let Some(msg) = self.session.history.last() {
                if let Some(id) = hook.persist_message(&self.session.id, msg) {
                    if let Some(last_id) = self.session.message_ids.last_mut() {
                        *last_id = id;
                    }
                }
            }
        }

        Ok(text)
    }

    /// Detect and recover an interrupted turn from session history.
    ///
    /// If the session ends with assistant tool_calls that have no matching tool
    /// results (process was killed mid-turn), re-execute the missing tools and
    /// call `chat_loop` so the model continues from where it left off.
    async fn recover_incomplete_turn(&mut self, stream_mode: &StreamMode) -> anyhow::Result<()> {
        // Scan session history from the end to find missing tool results.
        let history = &self.session.history;
        if history.is_empty() {
            return Ok(());
        }

        // Walk backwards: collect all tool_call_ids that have results,
        // then find tool_calls without results.
        let mut completed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut pending_calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut found_incomplete = false;

        // Scan from end backwards until we hit a non-assistant message.
        for msg in history.iter().rev() {
            match msg.role {
                crate::providers::Role::Tool => {
                    if let Some(ref id) = msg.tool_call_id {
                        completed_ids.insert(id.clone());
                    }
                }
                crate::providers::Role::Assistant => {
                    if let Some(ref calls) = msg.tool_calls {
                        for call in calls {
                            if !completed_ids.contains(&call.id) {
                                pending_calls.push(call.clone());
                                found_incomplete = true;
                            }
                        }
                    }
                    // Stop scanning — we only care about the trailing assistant tool_calls.
                    break;
                }
                _ => break, // User/system message — no incomplete turn.
            }
        }

        if !found_incomplete || pending_calls.is_empty() {
            return Ok(());
        }

        tracing::info!(
            missing_count = pending_calls.len(),
            "detected incomplete turn from previous run, resuming tool execution"
        );

        // Re-execute missing tool results and append to session history.
        let mut messages = self.build_messages().await?;

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

            // Append to messages for chat_loop.
            let mut tool_msg = ChatMessage::text("tool", &result_content);
            tool_msg.tool_call_id = Some(call.id.clone());
            tool_msg.is_error = Some(is_error);
            messages.push(tool_msg);

            // Persist to session history.
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

        // Call chat_loop so the model sees the completed tool results and continues.
        let _text = self.chat_loop(messages, stream_mode.clone()).await?;
        tracing::info!("interrupted turn resumed and completed");

        Ok(())
    }

    /// Build the message list: system prompt + history.
    ///
    /// Pure function — no side effects on session state.
    /// All attachment merging and hot-reload happens in `run_turn_core` before this is called.
    pub(crate) async fn build_messages(&mut self) -> anyhow::Result<Vec<ChatMessage>> {
        let mut messages = Vec::with_capacity(self.session.history.len() + 2);

        if !self.system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(&self.system_prompt));
        }

        messages.extend(self.session.history.iter().cloned());

        // Remove orphan tool results before sending to provider.
        super::super::session_manager::sanitize_history(&mut messages);

        Ok(messages)
    }

    /// Check for file changes (hot-reload).
    pub(crate) fn check_changes(&mut self) {
        let rx = match self.change_rx.as_mut() {
            Some(rx) => rx,
            None => {
                tracing::debug!("check_changes: change_rx is None, skipping");
                return;
            }
        };

        let changed = rx.has_changed();
        tracing::info!(
            has_changed = ?changed,
            "check_changes: polled watcher"
        );

        while rx.has_changed().unwrap_or(false) {
            let changes = rx.borrow_and_update().clone();
            tracing::info!(?changes, "check_changes: processing change");

            if changes.skills_changed {
                let new_defs = super::super::skill_loader::load_skills_from_dir(&self.skills_dir);
                let new_skills: Vec<super::super::skills::Skill> =
                    new_defs.iter().map(super::super::skills::Skill::from_definition).collect();
                {
                    let mut skills = self.skills.write();
                    skills.reload(new_skills);
                }
                let skills = self.skills.read();
                let history = self.session.history.clone();
                tracing::debug!(
                    history_len = history.len(),
                    current_count = skills.skill_count(),
                    "check_changes: calling diff_skills"
                );
                self.attachments.diff_skills(&skills, &history);
                tracing::info!(skill_count = skills.skill_count(), "skills hot-reloaded");
            }

            if changes.agents_changed {
                let new_agents = super::super::agent_loader::load_agents_from_dir(&self.agents_dir);
                let agent_list: Vec<(String, String)> = new_agents
                    .iter()
                    .map(|a| (a.name.clone(), a.description.clone().unwrap_or_default()))
                    .collect();
                {
                    let mut configs = self.sub_agent_configs.write();
                    *configs = new_agents;
                }
                let history = self.session.history.clone();
                self.attachments.diff_agents(&agent_list, &history);
                tracing::info!(agent_count = agent_list.len(), "agents hot-reloaded");
            }

            if changes.memory_changed {
                let memory_dir = std::path::Path::new(&self.config.prompt_config.knowledge_dir);
                let files = crate::memory::scan_memory_files(memory_dir);
                let entries: Vec<crate::memory::IndexEntry> =
                    files.iter().map(crate::memory::IndexEntry::from).collect();
                let history = self.session.history.clone();
                self.attachments.diff_memory(&entries, &history);
                tracing::info!(memory_count = entries.len(), "memory hot-reloaded");
            }
        }
    }

    /// Core chat loop: call LLM, handle tool calls, repeat until text response.
    async fn chat_loop(&mut self, initial_messages: Vec<ChatMessage>, stream_mode: StreamMode) -> anyhow::Result<String> {
        let mut tool_calls_count = 0usize;
        let mut stream_timeout_retries = 0usize;
        let mut retry_count = 0usize;
        let mut empty_response_retries = 0usize;
        let mut boosted_max_tokens = false;
        let mut first_iteration = true;
        let mut images_attached = false;

        // Check if we have pending images that need a vision-capable model.
        let has_images = self.pending_image_urls.as_ref().is_some_and(|v| !v.is_empty())
            || self.pending_image_base64.as_ref().is_some_and(|v| !v.is_empty());

        // Pre-emptive compaction for fallback models: when the primary model is unavailable
        // (rate-limit or server error) the FallbackChatProvider routes to a smaller model
        // whose context window may be exceeded by the current history.
        // Only runs when no model_override is active (overrides bypass the fallback chain).
        if self.model_override.is_none() {
            if let Err(e) = self.maybe_compact_for_fallback().await {
                tracing::warn!(error = %e, "pre-fallback compaction check failed, continuing");
            }
        }

        loop {
            // Cancellation checkpoint 3: before next LLM call (top of loop).
            if let StreamMode::Streamed { cancel, .. } = &stream_mode {
                if cancel.is_cancelled() {
                    tracing::info!("turn cancelled before next LLM call");
                    return Ok(String::new());
                }
            }

            // Hot switch checkpoint: before next LLM call.
            if crate::is_shutting_down() {
                tracing::info!("shutdown flag set, exiting at LLM checkpoint");
                return Ok(String::new());
            }

            // 1. Get a chat provider via registry.
            // If model_override is set, use that model directly.
            // If images are pending, prefer a vision-capable model from the fallback chain.
            let (provider, model_id) = if let Some(ref model) = self.model_override {
                match self.registry.get_chat_provider_by_model(model) {
                    Some((p, id)) => (p, id),
                    None => {
                        tracing::warn!(model = %model, "model_override not found, falling back to default");
                        self.registry.get_chat_provider(Capability::Chat)?
                    }
                }
            } else if has_images {
                self.select_vision_provider().await?
            } else {
                self.registry.get_chat_provider(Capability::Chat)?
            };

            // Pre-API compaction check: tool results from the previous round may have
            // pushed context over threshold. Compact before building messages to avoid
            // sending an oversized context. No-op on first iteration.
            if let Err(e) = self.maybe_compact(&model_id).await {
                tracing::warn!(error = %e, "pre-API compaction check failed, continuing");
            }

            // Use initial_messages on the first iteration (includes system-reminder
            // from AttachmentManager), rebuild on subsequent iterations (after tool
            // calls or compaction).
            let mut messages = if first_iteration {
                first_iteration = false;
                tracing::info!(
                    msg_count = initial_messages.len(),
                    "chat_loop: first iteration using initial_messages"
                );
                for (i, m) in initial_messages.iter().enumerate() {
                    let text = m.text_content();
                    let has_reminder = text.contains("<system-reminder>");
                    tracing::info!(
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
                self.build_messages().await?
            };

            // Attach pending images to the last user message only on the first iteration.
            // Subsequent iterations (after tool calls) rebuild from history which already
            // has the text content; re-attaching would send images repeatedly.
            if !images_attached {
                self.attach_images_if_supported(&mut messages, &model_id);
                images_attached = true;
            }

            // 2. Build tool specs from skills manager.
            let tools = self.build_tool_specs();

            // 3. Build request.
            // Calculate max_tokens based on context window and current usage.
            // On retry after MaxTokens with empty text, boost the output budget.
            let max_tokens = if boosted_max_tokens {
                self.calculate_boosted_max_tokens(&model_id)
            } else {
                self.calculate_max_tokens(&model_id)
            };

            // Derive thinking config: session override takes priority over model config.
            let thinking = if let Some(ref t) = self.thinking_override {
                if t.enabled { Some(t.clone()) } else { None }
            } else {
                self.registry.get_chat_model_config(&model_id)
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
                tools: if tools.is_empty() { None } else { Some(&tools[..]) },
                stream: true,
            };

            // Log the message sequence being sent to the model.
            tracing::info!(
                msg_count = messages.len(),
                tool_count = tool_calls_count,
                "sending messages to model: {:?}",
                messages.iter().map(|m| {
                    let content = m.text_content();
                    let truncated = if content.len() > 100 {
                        format!("{}...", crate::str_utils::truncate_chars(&content, 97))
                    } else { content.to_string() };
                    format!("{}: {}", m.role, truncated)
                }).collect::<Vec<_>>()
            );

            // 4. Call chat and process stream.
            let stream = provider.chat(req)?;
            tracing::info!("chat stream started, collecting...");

            // Branch on StreamMode: Collect (existing) vs Streamed (forward events).
            let response = {
                let result = match &stream_mode {
                    StreamMode::Collect => self.collect_stream(stream).await,
                    StreamMode::Streamed { event_tx, cancel } => {
                        self.collect_stream_with_events(stream, event_tx, cancel).await
                    }
                };
                match result {
                    Ok(r) => r,
                    Err(e) => {
                        let classified = crate::providers::ClassifiedError::from_message(&e.to_string());
                        if classified.retryable {
                            match classified.reason {
                                crate::providers::FailoverReason::Timeout => {
                                    stream_timeout_retries += 1;
                                    if stream_timeout_retries > 3 {
                                        tracing::error!("stream timeout after 3 retries, giving up");
                                        return Ok(String::new());
                                    }
                                    tracing::warn!(
                                        attempt = stream_timeout_retries,
                                        "stream chunk timeout, retrying..."
                                    );
                                    continue;
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
                                        "retryable error, retrying..."
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
                    tracing::info!("turn cancelled after stream collection");
                    return Ok(response.text);
                }
            }

            tracing::info!(text_len = response.text.len(), tool_calls = response.tool_calls.len(), stop = ?response.stop_reason, "chat stream collected");

            // Record token usage from API response.
            // Real context = input_tokens (new) + cached_input_tokens + output_tokens.
            if let Some(ref usage) = response.usage {
                let cached = usage.cached_input_tokens.unwrap_or(0);
                self.token_tracker.update_from_usage(
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0),
                    cached,
                );
                tracing::debug!(
                    input_tokens = usage.input_tokens.unwrap_or(0),
                    cached_tokens = cached,
                    output_tokens = usage.output_tokens.unwrap_or(0),
                    total_tracked = self.token_tracker.total_tokens(),
                    "token usage recorded"
                );

                // Persist the precise total so it survives restarts.
                if let Some(ref hook) = self.persist_hook {
                    hook.save_token_count(&self.session.id, self.token_tracker.total_tokens());
                }
            }

            // Check compaction using the precise token counts just reported by the API.
            // This eliminates the one-turn delay that results from checking before the
            // API call: we now always have accurate data when deciding to compact.
            if let Err(e) = self.maybe_compact(&model_id).await {
                tracing::warn!(error = %e, "compaction failed, continuing");
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
                            tracing::warn!(attempt = empty_response_retries, "output hit max_tokens with no text, boosting output budget for retry...");
                            boosted_max_tokens = true;
                        }
                        StopReason::StopSequence | StopReason::EndTurn => {
                            // Model stopped naturally but produced no text — may be a transient issue.
                            tracing::warn!(attempt = empty_response_retries, stop = ?response.stop_reason, "empty response with natural stop, retrying...");
                        }
                        _ => {
                            tracing::warn!(attempt = empty_response_retries, stop = ?response.stop_reason, "chat response text is empty, retrying...");
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
                tracing::info!(tool = %call.name, id = %call.id, arguments = %call.arguments, "model requested tool call");
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
                    ContentPart::Thinking { thinking: thinking.clone() },
                );
            }

            messages.push(assistant_msg);

            // Persist assistant message with tool_calls to session history.
            self.session.add_assistant_with_tools(
                response.text.clone(),
                response.tool_calls.clone(),
                response.reasoning_content.clone(),
            );

            // Persist assistant tool-call message via hook; capture DB id.
            if let Some(ref hook) = self.persist_hook {
                if let Some(msg) = self.session.history.last() {
                    if let Some(id) = hook.persist_message(&self.session.id, msg) {
                        if let Some(last_id) = self.session.message_ids.last_mut() {
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
                        tracing::info!(tool = %call.name, "turn cancelled before tool execution");
                        let _ = event_tx.send(TurnEvent::Cancelled { partial: response.text.clone() }).await;
                        return Ok(response.text.clone());
                    }
                    // Send ToolCall event to client.
                    let args: serde_json::Value = serde_json::from_str(&call.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    let _ = event_tx.send(TurnEvent::ToolCall {
                        name: call.name.clone(),
                        args,
                    }).await;
                }

                // Hot switch checkpoint: before tool execution.
                if crate::is_shutting_down() {
                    tracing::info!(tool = %call.name, "shutdown flag set, exiting before tool execution");
                    return Ok(String::new());
                }

                // Hard limit check.
                if self.config.max_tool_calls > 0
                    && tool_calls_count > self.config.max_tool_calls
                {
                    anyhow::bail!(
                        "Tool call limit reached ({}), loop broken",
                        self.config.max_tool_calls
                    );
                }

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

                tracing::info!(tool = %call.name, success = !is_error, "tool result:\n{}", result_content);

                // Streamed mode: send ToolResult event to client.
                if let StreamMode::Streamed { event_tx, .. } = &stream_mode {
                    let _ = event_tx.send(TurnEvent::ToolResult {
                        name: call.name.clone(),
                        output: result_content.clone(),
                    }).await;
                }

                // Loop breaker check.
                match self.loop_breaker.record_and_check(&call.name, &call.arguments, &result_content) {
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
                self.token_tracker.record_pending(
                    estimate_message_tokens(messages.last().unwrap())
                );

                // Persist tool result to session history.
                self.session.add_tool_result(call.id.clone(), result_content, is_error);

                // Persist tool result via hook; capture DB id.
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
        }
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
    async fn collect_stream_with_events(
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
        let mut tool_calls = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage: Option<ChatUsage> = None;

        let chunk_timeout = Duration::from_secs(self.config.stream_chunk_timeout_secs);
        let max_output_bytes = self.config.max_output_bytes;

        loop {
            // Cancellation checkpoint (streaming path only).
            if let Some(cancel) = cancel {
                if cancel.is_cancelled() {
                    return Ok(CollectedResponse { text, reasoning_content, tool_calls, stop_reason, usage });
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

            // Wait for next chunk with timeout
            match tokio::time::timeout(chunk_timeout, stream.next()).await {
                Ok(Some(event)) => {
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
                        StreamEvent::HttpError { message, .. } => {
                            anyhow::bail!("Stream error: {}", message);
                        }
                        StreamEvent::Error(e) => {
                            anyhow::bail!("Stream error: {}", e);
                        }
                    }
                }
                Ok(None) => {
                    // Stream ended without Done event
                    tracing::warn!("stream ended without Done event");
                    break;
                }
                Err(_) => {
                    // Chunk timeout — treat as server-side failure so the
                    // caller (chat_loop) can distinguish this from a genuine
                    // MaxTokens condition and take appropriate action.
                    anyhow::bail!(
                        "stream chunk timeout after {}s, no data received",
                        chunk_timeout.as_secs()
                    );
                }
            }
        }

        Ok(CollectedResponse {
            text,
            reasoning_content,
            tool_calls,
            stop_reason,
            usage,
        })
    }

    /// Calculate max_tokens for the current request based on context window.
    fn calculate_max_tokens(&self, model_id: &str) -> Option<u32> {
        let model_config = self.registry.get_chat_model_config(model_id).ok()?;
        let context_window = model_config.context_window?;
        let max_output = model_config.max_output_tokens.unwrap_or(4096) as u64;

        let total_tokens = self.token_tracker.total_tokens();
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
    fn calculate_boosted_max_tokens(&self, model_id: &str) -> Option<u32> {
        let model_config = self.registry.get_chat_model_config(model_id).ok()?;
        let context_window = model_config.context_window?;
        let default_max = model_config.max_output_tokens.unwrap_or(4096) as u64;
        // Double the output budget.
        let boosted = (default_max * 2).min(context_window);

        let total_tokens = self.token_tracker.total_tokens();
        let available = context_window.saturating_sub(total_tokens);
        let max = boosted.min(available).min(u32::MAX as u64);

        tracing::info!(
            boosted_max = max,
            available,
            "boosted max_tokens for retry"
        );

        Some(max.max(256) as u32)
    }
}
