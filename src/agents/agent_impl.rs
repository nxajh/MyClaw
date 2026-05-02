//! Agent — shared factory for AgentLoop instances.
//!
//! Agent holds shared resources (registry, skills, config) and creates
//! per-session AgentLoop handles.
//!
//! DDD: Agent depends on `dyn ServiceRegistry` (Domain trait), not on
//! `Registry` (Infrastructure concrete type). This keeps the Application
//! layer decoupled from Infrastructure.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::providers::Capability;
use crate::providers::{
    BoxStream, ChatMessage, ChatRequest, ChatUsage, StopReason, StreamEvent, ToolCall,
};
use crate::providers::ServiceRegistry;
use crate::providers::capability_tool::ToolResult;
use crate::config::agent::ContextConfig;
use crate::tools::TaskDelegator;
use super::skills::SkillManager;
use super::tool_registry::ToolRegistry;
use futures_util::StreamExt;

/// Callback for ask_user tool: (session_key, question) → user_answer.
///
/// The handler sends the question through the channel and waits for the
/// user's next message, which is delivered via a oneshot channel managed
/// by the Orchestrator.
pub type AskUserHandler = Arc<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>
        + Send
        + Sync,
>;

/// Callback for async delegation: (agent_name, task) → task_id.
///
/// The handler spawns the sub-agent in a background tokio task and returns
/// the task_id immediately. When the sub-agent completes, the Orchestrator
/// receives a DelegationEvent and wakes the main agent.
pub type DelegateHandler = Arc<
    dyn Fn(String, String) -> anyhow::Result<String> + Send + Sync,
>;

use super::loop_breaker::{LoopBreak, LoopBreaker, LoopBreakerConfig};
use super::session_manager::{Session, PersistHook};
use crate::agents::prompt::{SystemPromptBuilder, SystemPromptConfig};
use crate::storage::SummaryRecord;

/// Estimate token count from text length (~4 bytes per token).
pub(crate) fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Estimate token count for a ChatMessage.
pub(crate) fn estimate_message_tokens(msg: &crate::providers::ChatMessage) -> u64 {
    use crate::providers::ContentPart;
    let mut tokens = 4u64; // metadata overhead
    for part in &msg.parts {
        tokens += match part {
            ContentPart::Text { text } => estimate_tokens(text),
            ContentPart::ImageUrl { .. } => 800,
            ContentPart::ImageB64 { .. } => 800,
            ContentPart::Thinking { thinking } => estimate_tokens(thinking),
        };
    }
    // Estimate tool_calls overhead (id + name + arguments).
    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            tokens += estimate_tokens(&tc.id) + estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments) + 8;
        }
    }
    // tool_call_id on tool result messages.
    if let Some(ref tcid) = msg.tool_call_id {
        tokens += estimate_tokens(tcid) + 4;
    }
    tokens
}

/// Token usage tracker — combines precise API-reported usage with estimated pending tokens.
#[derive(Debug, Clone, Default)]
pub(crate) struct TokenTracker {
    /// Last API response's input_tokens (new, non-cached).
    last_input_tokens: u64,
    /// Last API response's cached_input_tokens.
    last_cached_tokens: u64,
    /// Last API response's output_tokens.
    last_output_tokens: u64,
    /// Estimated tokens of items added to history after the last API response.
    pending_estimated_tokens: u64,
}

impl TokenTracker {
    /// Update with precise usage from API response. Resets pending estimates.
    /// `input_tokens` = new (non-cached) tokens, `cached_tokens` = cache-hit tokens.
    pub fn update_from_usage(&mut self, input_tokens: u64, output_tokens: u64, cached_tokens: u64) {
        self.last_input_tokens = input_tokens;
        self.last_output_tokens = output_tokens;
        self.last_cached_tokens = cached_tokens;
        self.pending_estimated_tokens = 0;
    }

    /// Record estimated tokens for a new item added to history.
    pub fn record_pending(&mut self, tokens: u64) {
        self.pending_estimated_tokens += tokens;
    }

    /// Total input context tokens (what the model sees as input).
    /// Does NOT include output_tokens — those are the model's generation, not input context.
    pub fn total_tokens(&self) -> u64 {
        self.last_input_tokens
            .saturating_add(self.last_cached_tokens)
            .saturating_add(self.pending_estimated_tokens)
    }

    /// Adjust tracker after compaction: deduct removed tokens, add summary tokens.
    /// Preserves output_tokens and only touches input/pending estimates.
    pub fn adjust_for_compaction(&mut self, removed_tokens: u64, added_tokens: u64) {
        let net_reduction = removed_tokens.saturating_sub(added_tokens);
        // Deduct from pending first, then from input.
        let from_pending = net_reduction.min(self.pending_estimated_tokens);
        self.pending_estimated_tokens -= from_pending;
        self.last_input_tokens = self.last_input_tokens
            .saturating_sub(net_reduction - from_pending);
    }

    /// Reset for a new conversation.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// AgentConfig controls loop breaker thresholds and tool call limits.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard cap on tool calls per turn. 0 = unlimited.
    pub max_tool_calls: usize,
    /// Maximum history messages to keep in memory. 0 = unlimited.
    pub max_history: usize,
    /// System prompt builder config.
    pub prompt_config: SystemPromptConfig,
    /// Context window management settings.
    pub context: ContextConfig,
    /// Stream chunk timeout in seconds — max time to wait for next chunk.
    pub stream_chunk_timeout_secs: u64,
    /// Max output bytes before forcing stream stop (derived from max_output_tokens).
    pub max_output_bytes: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            max_history: 200,
            prompt_config: SystemPromptConfig::default(),
            context: ContextConfig::default(),
            stream_chunk_timeout_secs: 90,
            max_output_bytes: 100 * 1024, // 100KB default
        }
    }
}

/// Agent is the shared factory — call `.loop_for(session)` to get an AgentLoop.
#[derive(Clone)]
pub struct Agent {
    registry: Arc<dyn ServiceRegistry>,
    tools: Arc<ToolRegistry>,
    skills: Arc<SkillManager>,
    config: AgentConfig,
    system_prompt: String,
    /// Optional model override for sub-agents (e.g. summarizer uses a cheaper model).
    model_override: Option<String>,
}

impl Agent {
    pub fn new(
        registry: Arc<dyn ServiceRegistry>,
        tools: Arc<ToolRegistry>,
        skills: Arc<SkillManager>,
        config: AgentConfig,
    ) -> Self {
        Self {
            registry,
            tools,
            skills,
            config,
            system_prompt: String::new(),
            model_override: None,
        }
    }

    /// Set the system prompt directly (overrides builder).
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    /// Set a model override (used by sub-agents to route to specific models).
    pub fn with_model(mut self, model: String) -> Self {
        self.model_override = Some(model);
        self
    }

    /// Create an AgentLoop for the given session.
    /// The system prompt is built from SystemPromptConfig + SkillManager.
    pub fn loop_for(&self, session: Session) -> AgentLoop {
        self.loop_for_with_persist(session, None)
    }

    /// Create an AgentLoop for the given session with an optional persist hook.
    pub fn loop_for_with_persist(
        &self,
        session: Session,
        persist_hook: Option<Arc<dyn PersistHook>>,
    ) -> AgentLoop {
        let prompt = if !self.system_prompt.is_empty() {
            // Direct prompt set via with_system_prompt()
            self.system_prompt.clone()
        } else {
            // Build from config
            let builder = SystemPromptBuilder::new(self.config.prompt_config.clone());
            let tool_names = self.tools.tool_names_sorted();
            builder.build(&self.skills, &tool_names)
        };

        AgentLoop {
            registry: Arc::clone(&self.registry),
            tools: Arc::clone(&self.tools),
            config: self.config.clone(),
            session,
            system_prompt: prompt,
            ask_user_handler: None,
            delegate_handler: None,
            loop_breaker: LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls: self.config.max_tool_calls,
                ..LoopBreakerConfig::default()
            }),
            pending_image_urls: None,
            pending_image_base64: None,
            token_tracker: TokenTracker::default(),
            persist_hook,
            sub_delegator: None,
            model_override: self.model_override.clone(),
        }
    }
}

/// Per-session agent loop handle. Execute `run(user_message)` to process a message.
pub struct AgentLoop {
    registry: Arc<dyn ServiceRegistry>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    session: Session,
    /// Template for the system prompt.
    system_prompt: String,
    /// Optional callback for ask_user tool.
    ask_user_handler: Option<AskUserHandler>,
    /// Optional callback for async delegate_task.
    delegate_handler: Option<DelegateHandler>,
    /// Loop breaker — detects repetitive tool-call patterns.
    loop_breaker: LoopBreaker,
    /// Pending image URLs from the current user message (attached per-model in chat_loop).
    pending_image_urls: Option<Vec<String>>,
    /// Pending base64 image data from the current user message.
    pending_image_base64: Option<Vec<String>>,
    /// Token usage tracker for context window management.
    token_tracker: TokenTracker,
    /// Optional hook for persisting messages to the backend.
    persist_hook: Option<Arc<dyn PersistHook>>,
    /// Optional sub-agent delegator for compaction (shared with Orchestrator).
    sub_delegator: Option<Arc<super::sub_agent::SubAgentDelegator>>,
    /// Optional model override — forces a specific model instead of registry default.
    model_override: Option<String>,
}

impl AgentLoop {
    /// Set the ask_user handler (called by Orchestrator to wire the channel).
    pub fn with_ask_user_handler(mut self, handler: AskUserHandler) -> Self {
        self.ask_user_handler = Some(handler);
        self
    }

    /// Set the delegate handler (called by Orchestrator to wire async delegation).
    pub fn with_delegate_handler(mut self, handler: DelegateHandler) -> Self {
        self.delegate_handler = Some(handler);
        self
    }

    /// Set the sub-agent delegator (used for compaction summarization).
    pub fn with_sub_delegator(mut self, delegator: Arc<super::sub_agent::SubAgentDelegator>) -> Self {
        self.sub_delegator = Some(delegator);
        self
    }

    /// Process a user message and return the assistant's text response.
    ///
    /// This is the main entry point called by the orchestrator.
    pub async fn run(&mut self, user_message: &str, image_urls: Option<Vec<String>>, image_base64: Option<Vec<String>>) -> anyhow::Result<String> {
        tracing::info!(user_input = %user_message, "user message received");

        // Reset loop breaker for new turn.
        self.loop_breaker.reset();

        // Reset token tracker for new turn, then estimate entire history.
        self.token_tracker.reset();
        for msg in &self.session.history {
            self.token_tracker.record_pending(estimate_message_tokens(msg));
        }

        // 1. Add user message to session (text only; images attached per-model in chat_loop).
        self.session.add_user_text(user_message.to_string());

        // Persist user message via hook.
        if let Some(ref hook) = self.persist_hook {
            if let Some(msg) = self.session.history.last() {
                hook.persist_message(&self.session.key, msg);
            }
        }

        self.pending_image_urls = image_urls;
        self.pending_image_base64 = image_base64;

        // 2. Build the full message list for this turn.
        let messages = self.build_messages().await?;

        // 3. Run the chat loop (handles tool calls iteratively).
        let text = self.chat_loop(messages).await?;

        // 4. Persist assistant response.
        self.session.add_assistant_text(text.clone());

        // Persist assistant message via hook.
        if let Some(ref hook) = self.persist_hook {
            if let Some(msg) = self.session.history.last() {
                hook.persist_message(&self.session.key, msg);
            }
        }

        Ok(text)
    }

    /// Attach pending image URLs and base64 data to the last user message if model supports it.
    fn attach_images_if_supported(&self, messages: &mut [ChatMessage], model_id: &str) {
        let has_urls = self.pending_image_urls.as_ref().is_some_and(|v| !v.is_empty());
        let has_b64 = self.pending_image_base64.as_ref().is_some_and(|v| !v.is_empty());

        if !has_urls && !has_b64 {
            return;
        }

        let supports_image = self
            .registry
            .get_chat_model_config(model_id)
            .map(|cfg| cfg.supports_image_input())
            .unwrap_or(false);

        if !supports_image {
            tracing::debug!(
                model = %model_id,
                "model does not support image input, ignoring images"
            );
            return;
        }

        if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
            if let Some(urls) = self.pending_image_urls.as_ref() {
                for url in urls {
                    last_user.parts.push(crate::providers::ContentPart::ImageUrl {
                        url: url.clone(),
                        detail: crate::providers::ImageDetail::Auto,
                    });
                }
            }
            if let Some(b64s) = self.pending_image_base64.as_ref() {
                for b64 in b64s {
                    last_user.parts.push(crate::providers::ContentPart::ImageB64 {
                        b64_json: b64.clone(),
                        detail: crate::providers::ImageDetail::Auto,
                    });
                }
            }
            let total = self.pending_image_urls.as_ref().map_or(0, |v| v.len())
                + self.pending_image_base64.as_ref().map_or(0, |v| v.len());
            tracing::info!("attached {} image(s) to user message", total);
        }
    }

    /// Select a vision-capable model from the fallback chain.
    /// Falls back to the default chat provider if no vision model is found.
    async fn select_vision_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::ChatProvider>, String)> {
        // Walk the routing list directly (not the fallback chain, which collapses
        // all providers into a single FallbackChatProvider entry).
        // We need the original model list to find vision-capable models.
        let routing_models = self.registry.get_chat_routing_models();
        for model_id in &routing_models {
            if let Ok(cfg) = self.registry.get_chat_model_config(model_id) {
                if cfg.supports_image_input() {
                    // Try direct model provider first (bypasses fallback chain).
                    if let Some((provider, id)) = self.registry.get_chat_provider_by_model(model_id) {
                        tracing::info!(model = %id, "selected vision-capable model for image input (direct)");
                        return Ok((provider, id));
                    }
                    // Fall back to the primary provider (FallbackChatProvider).
                    tracing::info!(model = %model_id, "selected vision-capable model for image input (fallback)");
                    let (provider, _) = self.registry.get_chat_provider(Capability::Chat)?;
                    return Ok((provider, model_id.clone()));
                }
            }
        }

        // No vision model found — warn and fall back to default.
        tracing::warn!("no vision-capable model found, images may be ignored");
        self.registry.get_chat_provider(Capability::Chat)
    }

    /// Build the message list: system prompt + history.
    async fn build_messages(&self) -> anyhow::Result<Vec<ChatMessage>> {
        let mut messages = Vec::with_capacity(self.session.history.len() + 4);

        // System prompt.
        if !self.system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(&self.system_prompt));
        }

        // History.
        messages.extend(self.session.history.iter().cloned());

        Ok(messages)
    }

    /// Core chat loop: call LLM, handle tool calls, repeat until text response.
    async fn chat_loop(&mut self, _initial_messages: Vec<ChatMessage>) -> anyhow::Result<String> {
        let mut tool_calls_count = 0usize;
        let mut boosted_max_tokens = false;

        // Check if we have pending images that need a vision-capable model.
        let has_images = self.pending_image_urls.as_ref().is_some_and(|v| !v.is_empty())
            || self.pending_image_base64.as_ref().is_some_and(|v| !v.is_empty());

        loop {
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

            // Check if context compaction is needed.
            if let Err(e) = self.maybe_compact(&model_id).await {
                tracing::warn!(error = %e, "compaction failed, continuing");
            }

            // Rebuild messages after compaction may have modified session.history.
            let mut messages = self.build_messages().await?;

            // Attach pending images to the last user message if model supports it.
            self.attach_images_if_supported(&mut messages, &model_id);

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

            let req = ChatRequest {
                model: &model_id,
                messages: &messages,
                temperature: None,
                max_tokens,
                thinking: None,
                stop: None,
                seed: None,
                tools: if tools.is_empty() { None } else { Some(&tools[..]) },
                stream: true,
            };

            // Log the message sequence being sent to the model.
            tracing::debug!(
                msg_count = messages.len(),
                tool_count = tool_calls_count,
                "sending messages to model: {:?}",
                messages.iter().map(|m| {
                    let content = m.text_content();
                    let truncated = if content.len() > 100 {
                        let end = content.char_indices().take_while(|(i, _)| *i < 100).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(100);
                        format!("{}...", &content[..end])
                    } else { content };
                    format!("{}: {}", m.role, truncated)
                }).collect::<Vec<_>>()
            );

            // 4. Call chat and process stream.
            let stream = provider.chat(req)?;
            tracing::info!("chat stream started, collecting...");

            // Log raw request.
            Self::log_chat_io("request", &model_id, &messages, &tools[..], None);

            let response = self.collect_stream(stream).await?;
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
            }

            // Log raw response.
            Self::log_chat_io("response", &model_id, &[], &[], Some(&response));

            // 5. No tool calls → return text.
            if response.tool_calls.is_empty() {
                if response.text.is_empty() {
                    tool_calls_count += 1;
                    if tool_calls_count > 3 {
                        tracing::error!("empty response after 3 retries, giving up");
                        return Ok(String::new());
                    }

                    match response.stop_reason {
                        StopReason::MaxTokens => {
                            // Output budget exhausted by thinking.
                            // Boost max_tokens on retry so thinking + text both fit.
                            tracing::warn!(attempt = tool_calls_count, "output hit max_tokens with no text, boosting output budget for retry...");
                            boosted_max_tokens = true;
                        }
                        _ => {
                            // Model returned end_turn/content_filter but no text (thinking-only).
                            // Just retry — the model may produce text on the next attempt.
                            tracing::warn!(attempt = tool_calls_count, stop = ?response.stop_reason, "chat response text is empty (thinking-only), retrying...");
                        }
                    }
                    continue;
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

            // Persist assistant tool-call message via hook.
            if let Some(ref hook) = self.persist_hook {
                if let Some(msg) = self.session.history.last() {
                    hook.persist_message(&self.session.key, msg);
                }
            }

            for call in &response.tool_calls {
                tool_calls_count += 1;

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

                // Loop breaker check.
                match self.loop_breaker.record_and_check(&call.name, &call.arguments, &result_content) {
                    LoopBreak::Detected(reason) => {
                        tracing::warn!(reason = ?reason, "loop breaker triggered, aborting turn");
                        anyhow::bail!("Loop breaker triggered: {:?}", reason);
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

                // Persist tool result via hook.
                if let Some(ref hook) = self.persist_hook {
                    if let Some(msg) = self.session.history.last() {
                        hook.persist_message(&self.session.key, msg);
                    }
                }
            }
        }
    }

    /// Collect all events from a streaming chat response.
    async fn collect_stream(
        &self,
        mut stream: BoxStream<StreamEvent>,
    ) -> anyhow::Result<CollectedResponse> {
        let mut text = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut tool_calls = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage: Option<ChatUsage> = None;

        let chunk_timeout = Duration::from_secs(self.config.stream_chunk_timeout_secs);
        let max_output_bytes = self.config.max_output_bytes;

        loop {
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
                        StreamEvent::Delta { text: delta } => text.push_str(&delta),
                        StreamEvent::Thinking { text: delta } => {
                            if !delta.is_empty() {
                                if let Some(rc) = &mut reasoning_content {
                                    rc.push_str(&delta);
                                } else {
                                    reasoning_content = Some(delta);
                                }
                            }
                        }
                        StreamEvent::ToolCallStart { id, name, initial_arguments } => {
                            tool_calls.push(ToolCall {
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
                                    tool_calls.push(ToolCall {
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
                            usage = Some(u);
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
                    // Chunk timeout
                    tracing::warn!(
                        chunk_timeout_secs = self.config.stream_chunk_timeout_secs,
                        "stream chunk timeout, no data received"
                    );
                    stop_reason = StopReason::MaxTokens;
                    break;
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

    /// Build tool specs from the skills manager.
    fn build_tool_specs(&self) -> Vec<crate::providers::capability_chat::ToolSpec> {
        use crate::providers::capability_chat::ToolSpec;
        self.tools
            .all_tools()
            .iter()
            .map(|t| {
                let spec = t.spec();
                ToolSpec {
                    name: spec.name,
                    description: Some(spec.description),
                    input_schema: spec.parameters,
                }
            })
            .collect()
    }

    /// Execute a single tool call.
    /// Special-cases `ask_user` and `delegate_task` to use handlers when available.
    /// Applies framework-level truncation based on the tool's `max_output_tokens()`.
    async fn execute_tool(&mut self, call: &ToolCall) -> anyhow::Result<ToolResult> {
        // Special handling for ask_user tool.
        if call.name == "ask_user" {
            if let Some(ref handler) = self.ask_user_handler {
                let args: serde_json::Value = if call.arguments.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&call.arguments).unwrap_or_else(|_| {
                        serde_json::json!({ "raw": &call.arguments })
                    })
                };
                let question = args["question"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

                // Record the assistant's question in session history.
                self.session.add_assistant_text(question.to_string());

                let answer = handler(self.session.key.clone(), question.to_string()).await?;

                // Record the user's answer in session history.
                self.session.add_user_text(answer.clone());

                return Ok(ToolResult {
                    success: true,
                    output: answer,
                    error: None,
                });
            }
        }

        // Special handling for delegate_task tool — async delegation via handler.
        if call.name == "delegate_task" {
            if let Some(ref handler) = self.delegate_handler {
                let args: serde_json::Value = if call.arguments.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&call.arguments).unwrap_or_else(|_| {
                        serde_json::json!({ "raw": &call.arguments })
                    })
                };
                let agent_name = args["agent"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("'agent' is required"))?;
                let task = args["task"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("'task' is required"))?;

                let task_id = handler(agent_name.to_string(), task.to_string())?;

                return Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Task delegated to sub-agent '{}' (task_id: {}). \
                         The sub-agent is now running in the background. \
                         You will be notified when it completes.",
                        agent_name, task_id
                    ),
                    error: None,
                });
            }
        }

        let tool = self.tools.get(&call.name).ok_or_else(|| {
            anyhow::anyhow!("Unknown tool: '{}'", call.name)
        })?;

        let args: serde_json::Value = if call.arguments.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&call.arguments).unwrap_or_else(|_| {
                serde_json::json!({ "raw": &call.arguments })
            })
        };

        let result = tool.execute(args).await?;

        // Framework-level truncation based on tool's declared limit.
        let max_tokens = tool.max_output_tokens();
        let truncated_output = crate::tools::truncation::truncate_tool_result(
            &result.output,
            max_tokens,
        );
        if truncated_output.len() != result.output.len() {
            tracing::debug!(
                tool = %call.name,
                original_len = result.output.len(),
                truncated_len = truncated_output.len(),
                max_tokens,
                "tool output truncated by framework"
            );
        }

        Ok(ToolResult {
            success: result.success,
            output: truncated_output,
            error: result.error,
        })
    }

    /// Append raw chat I/O to a debug log file for post-mortem analysis.
    ///
    /// Each entry is a JSON line with timestamp, direction, model, and payload.
    /// The log file is `chat_debug.jsonl` in the current working directory.
    /// Writes are best-effort — errors are silently ignored.
    fn log_chat_io(
        direction: &str,
        model_id: &str,
        messages: &[ChatMessage],
        tools: &[crate::providers::capability_chat::ToolSpec],
        response: Option<&CollectedResponse>,
    ) {
        use std::io::Write;

        let entry = if direction == "request" {
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "dir": "request",
                "model": model_id,
                "messages": messages.iter().map(|m| {
                    let mut obj = serde_json::json!({
                        "role": m.role,
                        "content": m.text_content(),
                    });
                    if let Some(tcs) = &m.tool_calls {
                        obj["tool_calls"] = serde_json::json!(tcs);
                    }
                    if let Some(tc_id) = &m.tool_call_id {
                        obj["tool_call_id"] = serde_json::json!(tc_id);
                    }
                    obj
                }).collect::<Vec<_>>(),
                "tools": tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
            })
        } else {
            let resp = response.unwrap();
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "dir": "response",
                "model": model_id,
                "text": resp.text,
                "text_len": resp.text.len(),
                "tool_calls": resp.tool_calls.iter().map(|tc| {
                    serde_json::json!({
                        "id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                    })
                }).collect::<Vec<_>>(),
                "tool_calls_count": resp.tool_calls.len(),
                "stop_reason": format!("{:?}", resp.stop_reason),
            })
        };

        // Best-effort write; don't propagate errors.
        let path = "chat_debug.jsonl";
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{}", entry);
        }
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

    /// Check if compaction is needed and perform LLM-based summarization.
    async fn maybe_compact(&mut self, model_id: &str) -> anyhow::Result<()> {
        let model_config = self.registry.get_chat_model_config(model_id)?;
        let context_window = match model_config.context_window {
            Some(cw) => cw,
            None => return Ok(()),
        };

        let threshold = (context_window as f64 * self.config.context.compact_threshold) as u64;
        let total = self.token_tracker.total_tokens();

        if total <= threshold {
            return Ok(());
        }

        tracing::info!(
            total_tokens = total,
            threshold,
            context_window,
            "starting context compaction"
        );

        // Determine how many messages to compact.
        let history_len = self.session.history.len();
        let ids_len = self.session.message_ids.len();
        if history_len <= 1 {
            return Ok(()); // nothing to compact
        }

        // Defensive: ensure message_ids is in sync with history.
        if ids_len < history_len {
            tracing::warn!(
                history_len,
                ids_len,
                "message_ids out of sync with history, padding with zeros"
            );
            self.session.message_ids.resize(history_len, 0);
        }

        let compact_ratio = self.config.context.compact_ratio;
        let compact_count = ((history_len as f64) * compact_ratio).ceil() as usize;
        // Ensure we keep at least the last message.
        let compact_count = compact_count.min(history_len - 1).max(1);

        // Take the oldest messages for compaction.
        let to_compact: Vec<ChatMessage> = self.session.history[..compact_count].to_vec();

        // Don't compact messages that are already summaries.
        if to_compact.iter().any(|m| {
            m.role == "system" && m.text_content().starts_with("[summary]")
        }) {
            // Already has a summary at the start, just trim oldest non-summary messages.
            self.trim_oldest(threshold);
            return Ok(());
        }

        // Build plain text content for the summarizer sub-agent.
        let mut text_for_summary = String::new();
        for msg in &to_compact {
            let text = msg.text_content();
            if !text.is_empty() {
                text_for_summary.push_str(&format!("[{}] {}\n\n", msg.role, text));
            }
        }

        // Call summarizer sub-agent or fall back to trim_oldest.
        let summary = match &self.sub_delegator {
            Some(delegator) => {
                match delegator.delegate("summarizer", &text_for_summary).await {
                    Ok(s) => {
                        if s.trim().is_empty() {
                            tracing::warn!("summarizer returned empty text, falling back to trim");
                            self.trim_oldest(threshold);
                            return Ok(());
                        }
                        s
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "summarizer sub-agent failed, falling back to trim");
                        self.trim_oldest(threshold);
                        return Ok(());
                    }
                }
            }
            None => {
                tracing::warn!("no sub_delegator configured, falling back to trim_oldest");
                self.trim_oldest(threshold);
                return Ok(());
            }
        };

        // Replace compacted messages with the summary.
        let summary_msg = ChatMessage::system_text(
            format!("[summary] {}", summary)
        );
        let summary_tokens = estimate_message_tokens(&summary_msg);

        // Track the last message id that was compacted.
        let last_compacted_id = self.session.message_ids
            .get(compact_count.saturating_sub(1))
            .copied()
            .unwrap_or(0);

        // Remove compacted messages and insert summary at the front.
        self.session.history.drain(..compact_count);
        self.session.history.insert(0, summary_msg);
        self.session.message_ids.drain(..compact_count);
        self.session.message_ids.insert(0, 0); // summary placeholder

        // Persist the compaction summary.
        if let Some(ref hook) = self.persist_hook {
            hook.save_compaction(&self.session.key, &SummaryRecord {
                id: 0,
                summary: summary.clone(),
                up_to_message: last_compacted_id,
                token_estimate: Some(summary_tokens),
                created_at: chrono::Utc::now(),
            });
        }

        // Adjust token tracker without overwriting output/cached data.
        let removed_tokens: u64 = to_compact.iter()
            .map(estimate_message_tokens)
            .sum();
        self.token_tracker.adjust_for_compaction(removed_tokens, summary_tokens);

        let new_total = self.token_tracker.total_tokens();
        tracing::info!(
            compacted_messages = compact_count,
            summary_tokens,
            removed_tokens,
            new_total_tokens = new_total,
            "context compaction completed"
        );

        // If still over threshold after compaction, trim oldest.
        if new_total > threshold {
            self.trim_oldest(threshold);
        }

        Ok(())
    }

    /// Trim oldest messages until total tokens are below threshold.
    /// Preserves system messages and the last user message.
    fn trim_oldest(&mut self, threshold: u64) {
        loop {
            let total = self.token_tracker.total_tokens();
            if total <= threshold || self.session.history.len() <= 2 {
                break;
            }
            // Skip system messages at the front (but not summaries).
            if self.session.history[0].role == "system"
                && !self.session.history[0].text_content().starts_with("[summary]")
            {
                break;
            }
            let removed = self.session.history.remove(0);
            let removed_tokens = estimate_message_tokens(&removed);
            self.token_tracker.adjust_for_compaction(removed_tokens, 0);

            // Drain the corresponding message_id.
            if !self.session.message_ids.is_empty() {
                self.session.message_ids.remove(0);
            }
        }
    }
}

/// Response collected from a chat stream.
struct CollectedResponse {
    text: String,
    reasoning_content: Option<String>,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    stop_reason: StopReason,
    usage: Option<ChatUsage>,
}

// ── Extension trait for ChatMessage ──────────────────────────────────────────

/// Extension methods for ChatMessage.
#[allow(dead_code)]
trait ChatMessageExt {
    fn with_name(self, name: String) -> ChatMessage;
}

impl ChatMessageExt for ChatMessage {
    fn with_name(self, name: String) -> ChatMessage {
        ChatMessage {
            role: self.role,
            parts: self.parts,
            name: Some(name),
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }
    }
}
