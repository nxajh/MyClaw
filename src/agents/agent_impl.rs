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
use std::path::PathBuf;

use parking_lot::RwLock;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::providers::Capability;
use crate::providers::{
    BoxStream, ChatMessage, ChatRequest, ChatUsage, ContentPart, StopReason, StreamEvent, ToolCall, ThinkingConfig,
};
use crate::providers::ServiceRegistry;
use crate::providers::capability_tool::ToolResult;
use crate::config::agent::{AutonomyLevel, ContextConfig};
use crate::agents::session_manager::SessionOverride;

use super::skills::SkillManager;
use super::tool_registry::ToolRegistry;
use super::TurnEvent;
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
use crate::agents::attachment::AttachmentManager;
use crate::config::sub_agent::SubAgentConfig;
use crate::tools::TaskDelegator;
use crate::storage::SummaryRecord;
use crate::str_utils;

// ── StreamMode ──────────────────────────────────────────────────────────────

/// Determines how the LLM stream is consumed inside `chat_loop`.
///
/// - `Collect`: silently collect into a `CollectedResponse` (existing `run()` behavior).
/// - `Streamed`: forward events via mpsc + support cancellation (for `run_streamed()`).
enum StreamMode {
    Collect,
    Streamed {
        event_tx: mpsc::Sender<TurnEvent>,
        cancel: CancellationToken,
    },
}

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

    /// Total context tokens (input + cached + output now in history + pending).
    pub fn total_tokens(&self) -> u64 {
        self.last_input_tokens
            .saturating_add(self.last_cached_tokens)
            .saturating_add(self.last_output_tokens)
            .saturating_add(self.pending_estimated_tokens)
    }

    /// Returns true if the tracker has never been updated (fresh session or recovery).
    pub fn is_fresh(&self) -> bool {
        self.last_input_tokens == 0
            && self.last_cached_tokens == 0
            && self.pending_estimated_tokens == 0
    }

    /// Last input tokens (new, non-cached).
    pub fn last_input(&self) -> u64 { self.last_input_tokens }

    /// Last cached input tokens.
    pub fn last_cached(&self) -> u64 { self.last_cached_tokens }

    /// Last output tokens.
    pub fn last_output(&self) -> u64 { self.last_output_tokens }

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
    /// Loop breaker exact-repeat threshold: N identical consecutive calls → break.
    pub loop_breaker_threshold: usize,
    /// Per-tool execution timeout in seconds (0 = no timeout).
    /// Does not apply to ask_user or delegate_task (those have their own timeouts).
    pub tool_timeout_secs: u64,
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
            loop_breaker_threshold: 3,
            tool_timeout_secs: 180,
        }
    }
}

/// Agent is the shared factory — call `.loop_for(session)` to get an AgentLoop.
#[derive(Clone)]
pub struct Agent {
    registry: Arc<dyn ServiceRegistry>,
    tools: Arc<ToolRegistry>,
    skills: Arc<RwLock<SkillManager>>,
    config: AgentConfig,
    system_prompt: String,
    /// Optional model override for sub-agents (e.g. summarizer uses a cheaper model).
    model_override: Option<String>,
    /// MCP server instructions: Vec<(server_name, instructions)>
    mcp_instructions: Vec<(String, String)>,
    /// Sub-agent configs (hot-reloadable).
    sub_agent_configs: Arc<RwLock<Vec<SubAgentConfig>>>,
    /// Workspace dirs for hot-reload scanning.
    skills_dir: PathBuf,
    agents_dir: PathBuf,
}

impl Agent {
    pub fn new(
        registry: Arc<dyn ServiceRegistry>,
        tools: Arc<ToolRegistry>,
        skills: Arc<RwLock<SkillManager>>,
        config: AgentConfig,
    ) -> Self {
        Self {
            registry,
            tools,
            skills,
            config,
            system_prompt: String::new(),
            model_override: None,
            mcp_instructions: Vec::new(),
            sub_agent_configs: Arc::new(RwLock::new(Vec::new())),
            skills_dir: PathBuf::new(),
            agents_dir: PathBuf::new(),
        }
    }

    /// Access the service registry (for slash commands).
    pub fn registry(&self) -> &Arc<dyn ServiceRegistry> {
        &self.registry
    }

    /// Access the tool registry (for slash commands).
    pub fn tools(&self) -> &Arc<super::tool_registry::ToolRegistry> {
        &self.tools
    }

    /// Access the skill manager (for slash commands).
    pub fn skills(&self) -> &Arc<RwLock<SkillManager>> {
        &self.skills
    }

    /// Access sub-agent configs (for slash commands and reload).
    pub fn sub_agent_configs(&self) -> &Arc<RwLock<Vec<SubAgentConfig>>> {
        &self.sub_agent_configs
    }

    /// Access workspace dirs.
    pub fn workspace_dir(&self) -> &str {
        &self.config.prompt_config.workspace_dir
    }

    /// Compact threshold ratio from agent config.
    pub fn compact_threshold(&self) -> f64 {
        self.config.context.compact_threshold
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

    /// Set MCP server instructions.
    pub fn with_mcp_instructions(mut self, instructions: Vec<(String, String)>) -> Self {
        self.mcp_instructions = instructions;
        self
    }

    /// Set sub-agent configs.
    pub fn with_sub_agent_configs(mut self, configs: Arc<RwLock<Vec<SubAgentConfig>>>) -> Self {
        self.sub_agent_configs = configs;
        self
    }

    /// Set workspace dirs for hot-reload scanning.
    pub fn with_workspace_dirs(mut self, skills_dir: PathBuf, agents_dir: PathBuf) -> Self {
        self.skills_dir = skills_dir;
        self.agents_dir = agents_dir;
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
        let ov = &session.session_override;

        // Apply autonomy override to prompt config if set.
        let prompt_config = if let Some(ref autonomy) = ov.autonomy {
            let mut pc = self.config.prompt_config.clone();
            pc.autonomy = match autonomy {
                AutonomyLevel::Full => crate::agents::prompt::AutonomyLevel::Full,
                AutonomyLevel::Default => crate::agents::prompt::AutonomyLevel::Default,
                AutonomyLevel::ReadOnly => crate::agents::prompt::AutonomyLevel::ReadOnly,
            };
            pc
        } else {
            self.config.prompt_config.clone()
        };

        let prompt = if !self.system_prompt.is_empty() {
            self.system_prompt.clone()
        } else {
            let skills = self.skills.read();
            let builder = SystemPromptBuilder::new(prompt_config.clone());
            builder.build(&skills)
        };

        // Apply context overrides from session override.
        let context = {
            let mut ctx = self.config.context.clone();
            if let Some(t) = ov.compact_threshold { ctx.compact_threshold = t; }
            if let Some(r) = ov.retain_work_units { ctx.retain_work_units = r; }
            ctx
        };

        // Apply max_tool_calls override.
        let max_tool_calls = ov.max_tool_calls.unwrap_or(self.config.max_tool_calls);

        // Resolve model override (session override takes priority over Agent-level override).
        let model_override = ov.model.clone().or_else(|| self.model_override.clone());

        // Resolve thinking override from session override.
        let thinking_override = match ov.thinking {
            Some(true) => Some(ThinkingConfig {
                enabled: true,
                effort: ov.effort.clone(),
            }),
            Some(false) => Some(ThinkingConfig { enabled: false, effort: None }),
            None => None,
        };

        let mut config = self.config.clone();
        config.prompt_config = prompt_config;
        config.context = context;
        config.max_tool_calls = max_tool_calls;

        AgentLoop {
            registry: Arc::clone(&self.registry),
            tools: Arc::clone(&self.tools),
            config,
            session,
            system_prompt: prompt,
            ask_user_handler: None,
            delegate_handler: None,
            loop_breaker: LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls,
                exact_repeat_threshold: self.config.loop_breaker_threshold,
                ..LoopBreakerConfig::default()
            }),
            pending_image_urls: None,
            pending_image_base64: None,
            token_tracker: TokenTracker::default(),
            persist_hook,
            sub_delegator: None,
            model_override,
            thinking_override,
            attachments: AttachmentManager::new(),
            mcp_instructions: self.mcp_instructions.clone(),
            skills: Arc::clone(&self.skills),
            sub_agent_configs: Arc::clone(&self.sub_agent_configs),
            skills_dir: self.skills_dir.clone(),
            agents_dir: self.agents_dir.clone(),
            change_rx: None,
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
    /// Optional thinking override from session override (None = use model config).
    thinking_override: Option<ThinkingConfig>,
    // ── Attachment (增量注入) ──
    /// Attachment manager — 追踪已通知 LLM 的 skills/agents/MCP 列表。
    attachments: AttachmentManager,
    /// MCP server instructions (startup 时一次性获取)。
    mcp_instructions: Vec<(String, String)>,
    /// Shared skill manager (RwLock for hot-reload).
    skills: Arc<RwLock<SkillManager>>,
    /// Sub-agent configs (RwLock for hot-reload).
    sub_agent_configs: Arc<RwLock<Vec<SubAgentConfig>>>,
    /// Workspace dirs for hot-reload scanning.
    skills_dir: PathBuf,
    agents_dir: PathBuf,
    /// File change receiver (None for sub-agents).
    change_rx: Option<watch::Receiver<super::watcher::ChangeSet>>,
}

impl AgentLoop {
    /// Set the ask_user handler (called by Orchestrator to wire the channel).
    pub fn with_ask_user_handler(mut self, handler: AskUserHandler) -> Self {
        self.ask_user_handler = Some(handler);
        self
    }

    /// Get a reference to the current session.
    pub fn session(&self) -> &super::session_manager::Session {
        &self.session
    }

    /// Get the current estimated total tokens.
    pub fn token_total(&self) -> u64 {
        self.token_tracker.total_tokens()
    }

    /// Get a breakdown of the last API call's token usage.
    pub fn last_usage(&self) -> (u64, u64, u64) {
        (
            self.token_tracker.last_input(),
            self.token_tracker.last_cached(),
            self.token_tracker.last_output(),
        )
    }

    /// Get the compact threshold ratio from config.
    pub fn compact_threshold(&self) -> f64 {
        self.config.context.compact_threshold
    }

    /// Get the current session override.
    pub fn session_override(&self) -> &SessionOverride {
        &self.session.session_override
    }

    /// Apply a new session override to this live agent loop.
    /// Updates the in-flight state so the override takes effect on the next
    /// message without waiting for the loop to be recreated.
    pub fn apply_session_override(&mut self, ov: SessionOverride) {
        // Update model override.
        self.model_override = ov.model.clone();

        // Update thinking override.
        self.thinking_override = match ov.thinking {
            Some(true) => Some(ThinkingConfig { enabled: true, effort: ov.effort.clone() }),
            Some(false) => Some(ThinkingConfig { enabled: false, effort: None }),
            None => None,
        };

        // Update max_tool_calls.
        if let Some(mtc) = ov.max_tool_calls {
            self.config.max_tool_calls = mtc;
            self.loop_breaker = LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls: mtc,
                exact_repeat_threshold: self.config.loop_breaker_threshold,
                ..LoopBreakerConfig::default()
            });
        }

        // Update context config.
        if let Some(t) = ov.compact_threshold { self.config.context.compact_threshold = t; }
        if let Some(r) = ov.retain_work_units { self.config.context.retain_work_units = r; }

        // Store override in session for next loop_for_with_persist call.
        self.session.session_override = ov;
    }

    /// Manually trigger compaction (used by /compact command).
    /// Skips the token threshold check — always attempts compression.
    pub async fn compact_now(&mut self, model_id: &str) -> anyhow::Result<()> {
        let context_window = self.registry.get_chat_model_config(model_id)?
            .context_window
            .ok_or_else(|| anyhow::anyhow!("模型未配置 context_window，无法压缩"))?;

        let history_len = self.session.history.len();
        if history_len <= 1 {
            anyhow::bail!("历史消息太少，无需压缩");
        }

        tracing::info!(
            total_tokens = self.token_tracker.total_tokens(),
            "starting manual compaction (/compact)"
        );

        self.compact_to_budget(model_id, context_window).await
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

    /// Set the file change receiver (for hot-reload).
    pub fn with_change_rx(mut self, rx: watch::Receiver<super::watcher::ChangeSet>) -> Self {
        self.change_rx = Some(rx);
        self
    }

    /// Access the attachment manager (for /reload command).
    pub fn attachments(&mut self) -> &mut AttachmentManager {
        &mut self.attachments
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

        // Compute initial diff (full listing of skills/agents/MCP) against history.
        // If history is empty (first turn or post-compaction), this sends a full listing.
        {
            tracing::info!("run: computing diff against history");
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
            // Agent listing: read from sub_delegator if available.
            if let Some(ref delegator) = self.sub_delegator {
                let agents = delegator.available_agents();
                tracing::info!(agent_count = agents.len(), "run: diff_agents");
                self.attachments.diff_agents(&agents, &history);
            } else {
                tracing::info!("run: no sub_delegator, skipping agent diff");
            }
            if !self.mcp_instructions.is_empty() {
                tracing::info!(mcp_count = self.mcp_instructions.len(), "run: diff_mcp");
                self.attachments.diff_mcp(&self.mcp_instructions, &history);
            }
            tracing::info!(
                pending_keys = ?self.attachments.pending_keys(),
                "run: diff complete"
            );
        }

        // 1. Account for the new user message before adding to history.
        let user_msg = ChatMessage::user_text(user_message.to_string());
        self.token_tracker.record_pending(estimate_message_tokens(&user_msg));
        self.session.add_user_text(user_message.to_string());

        // Persist user message via hook; capture the assigned DB id.
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

        // 2. Build the full message list for this turn.
        let messages = self.build_messages().await?;

        // 3. Run the chat loop (handles tool calls iteratively).
        let text = self.chat_loop(messages, stream_mode).await?;

        // 4. Persist assistant response.
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

    /// Build the message list: system prompt + attachment (delta) + history.
    async fn build_messages(&mut self) -> anyhow::Result<Vec<ChatMessage>> {
        let mut messages = Vec::with_capacity(self.session.history.len() + 8);

        // System prompt (static).
        if !self.system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(&self.system_prompt));
        }

        // Check for file changes (hot-reload).
        self.check_changes();

        // Date injection: check if date system-reminder is needed.
        {
            let tz = self.config.prompt_config.timezone_offset;
            let history = self.session.history.clone();
            self.attachments.diff_date(tz, &history);
        }

        // If there's a pending attachment delta, persist it into session history.
        // All system-reminders are kept in history — compaction naturally removes old ones.
        {
            let skills = self.skills.read();
            tracing::info!(
                pending_keys = ?self.attachments.pending_keys(),
                skill_count = skills.skill_count(),
                "build_messages: checking attachment delta"
            );
            if let Some(msg) = self.attachments.build_message(&skills) {
                tracing::info!(
                    msg_len = msg.text_content().len(),
                    "build_messages: persisting attachment to history"
                );
                // Append to history (previous system-reminders are kept)
                self.session.add_user_text(msg.text_content().to_string());

                // Persist the system-reminder so it survives session switches/restarts.
                // Without this, the message lives only in memory and is lost when the
                // session is evicted from the DashMap.
                if let Some(ref hook) = self.persist_hook {
                    if let Some(reminder_msg) = self.session.history.last() {
                        if let Some(id) = hook.persist_message(&self.session.id, reminder_msg) {
                            if let Some(last_id) = self.session.message_ids.last_mut() {
                                *last_id = id;
                            }
                        }
                    }
                }

                self.attachments.clear_pending();
            }
        }

        // History (includes the attachment if just persisted).
        messages.extend(self.session.history.iter().cloned());

        // Safety: remove orphan tool results before sending to provider.
        super::session_manager::sanitize_history(&mut messages);

        Ok(messages)
    }

    /// Check for file changes (hot-reload).
    fn check_changes(&mut self) {
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
                let new_defs = super::skill_loader::load_skills_from_dir(&self.skills_dir);
                let new_skills: Vec<super::skills::Skill> =
                    new_defs.iter().map(super::skills::Skill::from_definition).collect();
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
                let new_agents = super::agent_loader::load_agents_from_dir(&self.agents_dir);
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
                let memory_dir = std::path::Path::new(&self.config.prompt_config.workspace_dir)
                    .join("memory");
                let files = crate::memory::scan_memory_files(&memory_dir);
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
        let mut boosted_max_tokens = false;
        let mut first_iteration = true;

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
                first_iteration = false;
                self.build_messages().await?
            };

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
                        format!("{}...", str_utils::truncate_chars(&content, 97))
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
                    Err(e) if e.to_string().contains("stream chunk timeout") => {
                        // Server hung without sending data — count as failed attempt.
                        tool_calls_count += 1;
                        if tool_calls_count > 3 {
                            tracing::error!("stream timeout after 3 retries, giving up");
                            return Ok(String::new());
                        }
                        tracing::warn!(
                            attempt = tool_calls_count,
                            "stream chunk timeout, server may be hung, retrying..."
                        );
                        continue;
                    }
                    Err(e) => return Err(e),
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

    /// Like `collect_stream`, but also forwards text/thinking chunks as
    /// `TurnEvent`s via `event_tx` and respects `CancellationToken`.
    ///
    /// Cancellation behaviour:
    /// - Returns `Ok(CollectedResponse)` with whatever was collected so far
    ///   (the caller is responsible for sending `TurnEvent::Cancelled`).
    /// - If `event_tx.send()` fails (client disconnected), returns
    ///   `Err("Client disconnected")`.
    async fn collect_stream_with_events(
        &self,
        mut stream: BoxStream<StreamEvent>,
        event_tx: &mpsc::Sender<TurnEvent>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<CollectedResponse> {
        let mut text = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut tool_calls = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage: Option<ChatUsage> = None;

        let chunk_timeout = Duration::from_secs(self.config.stream_chunk_timeout_secs);
        let max_output_bytes = self.config.max_output_bytes;

        loop {
            // Cancellation checkpoint 1: between stream chunks.
            if cancel.is_cancelled() {
                return Ok(CollectedResponse {
                    text,
                    reasoning_content,
                    tool_calls,
                    stop_reason,
                    usage,
                });
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
                            // Forward chunk to client. If send fails, client is gone.
                            if event_tx.send(TurnEvent::Chunk { delta }).await.is_err() {
                                anyhow::bail!("Client disconnected during stream");
                            }
                        }
                        StreamEvent::Thinking { text: delta } => {
                            if !delta.is_empty() {
                                if let Some(rc) = &mut reasoning_content {
                                    rc.push_str(&delta);
                                } else {
                                    reasoning_content = Some(delta.clone());
                                }
                                if event_tx.send(TurnEvent::Thinking { delta }).await.is_err() {
                                    anyhow::bail!("Client disconnected during stream");
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
                    tracing::warn!("stream ended without Done event");
                    break;
                }
                Err(_) => {
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

                let answer = handler(self.session.id.clone(), question.to_string()).await?;

                // Record the user's answer in session history.
                self.session.add_user_text(answer.clone());

                return Ok(ToolResult {
                    success: true,
                    output: answer,
                    error: None,
                });
            }
        }

        // Special handling for delegate_task: async handler takes priority,
        // then sync delegation via sub_delegator (with parent session ID for persistence).
        if call.name == "delegate_task" {
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

            if let Some(ref handler) = self.delegate_handler {
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

            if let Some(ref delegator) = self.sub_delegator {
                let parent_id = self.session.id.clone();
                let result = delegator.delegate_with_parent(agent_name, task, &parent_id).await;
                return Ok(match result {
                    Ok(output) => ToolResult { success: true, output, error: None },
                    Err(e) => ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Sub-agent '{}' failed: {}", agent_name, e)),
                    },
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

        let result = if self.config.tool_timeout_secs > 0 {
            let timeout = Duration::from_secs(self.config.tool_timeout_secs);
            tokio::time::timeout(timeout, tool.execute(args))
                .await
                .unwrap_or_else(|_| {
                    Ok(crate::providers::capability_tool::ToolResult {
                        success: false,
                        output: format!(
                            "Tool '{}' timed out after {}s",
                            call.name, self.config.tool_timeout_secs
                        ),
                        error: Some("timeout".to_string()),
                    })
                })?
        } else {
            tool.execute(args).await?
        };

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

    /// Check whether the summary retains key information from the original dialogue.
    fn audit_summary_quality(
        &self,
        to_compact: &[ChatMessage],
        summary: &str,
    ) -> (bool, Vec<String>) {
        let mut reasons = Vec::new();

        // Check 1: length reasonable (no more than 2000 chars).
        if summary.chars().count() > 2000 {
            reasons.push(format!(
                "summary too long: {} chars (limit 2000)",
                summary.chars().count()
            ));
        }

        // Check 2: file paths preserved.
        let original_paths = Self::extract_file_paths(to_compact);
        if !original_paths.is_empty() {
            let preserved = original_paths.iter()
                .filter(|p| summary.contains(*p))
                .count();
            if preserved == 0 && original_paths.len() <= 5 {
                reasons.push(format!(
                    "no file paths preserved (original had {})",
                    original_paths.len()
                ));
            }
        }

        (reasons.is_empty(), reasons)
    }

    /// Extract likely file paths from messages (simplified).
    fn extract_file_paths(messages: &[ChatMessage]) -> Vec<String> {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"(?:/[\w/.-]+\.\w{1,5})|(?:src/[\w/.-]+)").unwrap());
        let mut paths = Vec::new();
        for msg in messages {
            for cap in re.captures_iter(&msg.text_content()) {
                if let Some(m) = cap.get(0) {
                    let p = m.as_str().to_string();
                    if !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
        }
        paths
    }

    /// Find the incremental compaction range and any existing summary to merge.
    fn find_incremental_range(&self, boundary: usize) -> (usize, usize, Option<String>) {
        let history = &self.session.history;
        let last_summary = history[..boundary].iter().rposition(|m| {
            m.role == "user" && m.text_content().starts_with("[Context Summary]")
        });
        match last_summary {
            Some(idx) => {
                let existing = history[idx].text_content();
                (idx, boundary, Some(existing))
            }
            None => (0, boundary, None),
        }
    }

    /// Inline summarizer — calls `do_inline_summarize` with a single attempt.
    async fn summarize_inline(
        &mut self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        model_id: &str,
    ) -> anyhow::Result<String> {
        match self.do_inline_summarize(to_compact, existing_summary, model_id).await {
            Ok(s) if !s.trim().is_empty() => Ok(s),
            Ok(_) => {
                tracing::warn!("summarize returned empty text");
                anyhow::bail!("summarize returned empty text")
            }
            Err(e) => {
                tracing::warn!(error = %e, "summarize failed");
                Err(e)
            }
        }
    }

    /// Inline summarize: same system prompt, tools, history as main request for
    /// maximum prefix cache hit. Runs a full mini chat_loop so the model can
    /// use tools if needed. 20K output budget (Claude Code: 20K).
    /// Final text output on EndTurn becomes the summary.
    ///
    /// `model_id` selects which model produces the summary.  For normal compaction this is
    /// the current primary model; for pre-fallback compaction this is the target (smaller)
    /// model so it both summarises and later handles the compacted context.
    async fn do_inline_summarize(
        &mut self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        model_id: &str,
    ) -> anyhow::Result<String> {
        // Prefer a direct provider for the requested model; fall back to the primary provider
        // (which may internally route through the fallback chain).
        let provider = match self.registry.get_chat_provider_by_model(model_id) {
            Some((p, _)) => p,
            None => {
                let (p, _) = self.registry.get_chat_provider(Capability::Chat)?;
                p
            }
        };

        // Build messages that mirror the main request for prefix cache hit.
        let mut messages: Vec<ChatMessage> = Vec::new();

        // 1. system prompt — same as main request.
        if !self.system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(&self.system_prompt));
        }

        // 2. history — same as main request (strip images to save tokens).
        for msg in to_compact {
            let mut cleaned = msg.clone();
            cleaned.parts = cleaned.parts.into_iter().map(|part| {
                match part {
                    ContentPart::ImageUrl { .. } => ContentPart::Text { text: "[image]".into() },
                    ContentPart::ImageB64 { .. } => ContentPart::Text { text: "[image]".into() },
                    other => other,
                }
            }).collect();
            messages.push(cleaned);
        }

        // 3. summarizer instruction — guide model to output summary directly,
        //    but if it calls tools, let it complete the round.
        let memory_prompt = "\n\
                 \n\
                 You also have a persistent memory system. The memory directory is `memory/` and\n\
                 its current index is in your system prompt above.\n\
                 \n\
                 Based on this conversation, decide if any memories should be saved, updated, or\n\
                 deleted. Use file_write to create/update memory files and file_edit to modify them.\n\
                 Use shell (rm) to delete memory files.\n\
                 \n\
                 Each memory file MUST have YAML frontmatter:\n\
                 ---\n\
                 name: short_snake_case_name\n\
                 description: one-line description (under 150 chars)\n\
                 type: user|feedback|project|reference\n\
                 created_at: YYYY-MM-DD\n\
                 ---\n\
                 \n\
                 Then the memory content in markdown.\n\
                 \n\
                 Rules:\n\
                 - ONLY save things NOT derivable from code/git (user preferences, decisions, corrections)\n\
                 - Check the existing memory index to avoid duplicates — update existing files instead of creating duplicates\n\
                 - If existing memories are outdated or contradicted, update or delete them\n\
                 - Keep name short, lowercase, underscores (becomes the filename: memory/{name}.md)\n\
                 - If no memory changes needed, skip this entirely and just output the summary\n\
                 \n\
                 You may use file_write, file_edit, and file_read tools for memory operations ONLY.\n\
                 Do not use other tools.";

        let prompt = match existing_summary {
            Some(base) => format!(
                "Below is a PREVIOUS SUMMARY followed by NEW conversation messages.\n\
                 \n\
                 === PREVIOUS SUMMARY ===\n{}\n\
                 === END PREVIOUS SUMMARY ===\n\
                 \n\
                 Merge the new messages into the previous summary. Produce a single \
                 updated summary that covers everything.\n\
                 \n\
                 Output the summary as plain text. If you also need to update memory files, \
                 use the file_write/file_edit tools first, then output the summary as your \
                 final response.\n\
                 \n\
                 Requirements:\n\
                 - Keep all user goals, tasks, and their current status\n\
                 - Keep all key decisions, conclusions, and reasoning\n\
                 - Keep all file paths, code locations, and variable names mentioned\n\
                 - Keep all errors encountered and how they were resolved\n\
                 - Omit raw tool output (large code blocks, logs, file contents)\n\
                 - Use the same language as the conversation (Chinese or English)\n\
                 - Be thorough but concise: every important detail should be preserved{}",
                base, memory_prompt
            ),
            None => format!(
                "Summarize the conversation history above. This summary will replace \
                 the full history, so it MUST preserve all information needed to continue \
                 the conversation seamlessly.\n\
                 \n\
                 Output the summary as plain text. If you also need to update memory files, \
                 use the file_write/file_edit tools first, then output the summary as your \
                 final response.\n\
                 \n\
                 Required sections:\n\
                 1. **User Goals**: What is the user trying to accomplish? Current status of each goal.\n\
                 2. **Key Decisions**: Important choices made and why.\n\
                 3. **Technical Context**: Files modified, code locations, APIs used, configurations changed.\n\
                 4. **Errors & Fixes**: Problems encountered and their solutions.\n\
                 5. **Pending Work**: What still needs to be done.\n\
                 \n\
                 Rules:\n\
                 - Omit raw tool output (large code blocks, logs, file dumps) — keep only key facts\n\
                 - Use the same language as the conversation\n\
                 - Be thorough: losing context means the user has to repeat themselves\n\
                 - This conversation has {} messages to summarize{}",
                to_compact.len(), memory_prompt
            ),
        };
        messages.push(ChatMessage::user_text(prompt));

        // 4. tools — same as main request for prefix cache.
        let tools = self.build_tool_specs();

        // 5. thinking config — same as main request for prefix cache.
        let thinking = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| {
                if cfg.reasoning {
                    Some(ThinkingConfig { enabled: true, effort: None })
                } else {
                    None
                }
            });

        // Mini chat_loop: keep calling the model until EndTurn.
        // Tool calls are executed using the main agent's tool executor,
        // but results are appended to the local messages Vec only —
        // NOT to self.session.history (which is the real conversation).
        let max_rounds = 10;
        let mut round = 0;

        let final_text = loop {
            round += 1;
            if round > max_rounds {
                tracing::warn!(rounds = round, "summarize loop exceeded max rounds");
                anyhow::bail!("summarize loop exceeded {} rounds", max_rounds);
            }

            let req = ChatRequest {
                model: model_id,
                messages: &messages,
                temperature: None,
                max_tokens: Some(20_000),
                thinking: thinking.clone(),
                stop: None,
                seed: None,
                tools: if tools.is_empty() { None } else { Some(&tools[..]) },
                stream: true,
            };

            let stream = provider.chat(req)?;
            let response = self.collect_stream(stream).await?;

            // Log cache hit for monitoring.
            if let Some(ref usage) = response.usage {
                if let Some(cached) = usage.cached_input_tokens {
                    tracing::info!(
                        round,
                        cached_tokens = cached,
                        total_input = usage.input_tokens.unwrap_or(0),
                        "summarizer cache hit"
                    );
                }
            }

            // No tool calls → final text, exit loop.
            if response.tool_calls.is_empty() {
                break response.text;
            }

            // Tool calls present → execute and append results to local messages.
            tracing::info!(
                round,
                tool_calls = response.tool_calls.len(),
                text_len = response.text.len(),
                "summarize: model requested tool calls"
            );

            // Append assistant message with tool_calls.
            let mut assistant_msg = ChatMessage::assistant_text(&response.text);
            assistant_msg.tool_calls = Some(response.tool_calls.clone());
            if let Some(ref thinking_text) = response.reasoning_content {
                assistant_msg.parts.insert(
                    0,
                    ContentPart::Thinking { thinking: thinking_text.clone() },
                );
            }
            messages.push(assistant_msg);

            // Execute each tool call.
            for call in &response.tool_calls {
                tracing::info!(tool = %call.name, id = %call.id, "summarize: executing tool");
                let result = self.execute_tool(call).await;
                let result_content = match &result {
                    Ok(r) => {
                        let mut out = r.output.clone();
                        if let Some(ref err) = r.error {
                            if out.is_empty() {
                                out = format!("error: {}", err);
                            }
                        }
                        out
                    }
                    Err(e) => format!("error: {}", e),
                };

                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(result.is_err());
                messages.push(tool_msg);
            }
        };

        Ok(final_text)
    }

    /// Check if compaction is needed and perform incremental LLM-based summarization.
    async fn maybe_compact(&mut self, model_id: &str) -> anyhow::Result<()> {
        let model_config = self.registry.get_chat_model_config(model_id)?;
        let context_window = match model_config.context_window {
            Some(cw) => cw,
            None => return Ok(()),
        };

        let threshold = (context_window as f64 * self.config.context.compact_threshold) as u64;
        let total = self.token_tracker.total_tokens();

        if total < threshold {
            return Ok(());
        }

        tracing::info!(
            total_tokens = total,
            threshold,
            context_window,
            "starting context compaction"
        );

        self.compact_to_budget(model_id, context_window).await
    }

    /// Core compaction implementation given a pre-computed split boundary.
    ///
    /// `history[..boundary]` is summarised; `history[boundary..]` is retained.
    /// Called by both regular threshold-based compaction and pre-fallback budget
    /// compaction, so the boundary-finding policy lives in the caller.
    async fn compact_with_boundary(
        &mut self,
        model_id: &str,
        boundary: usize,
    ) -> anyhow::Result<()> {
        let history_len = self.session.history.len();
        if history_len <= 1 {
            return Ok(());
        }

        // Defensive: ensure message_ids is in sync with history.
        let ids_len = self.session.message_ids.len();
        if ids_len < history_len {
            tracing::warn!(
                history_len,
                ids_len,
                "message_ids out of sync with history, padding with zeros"
            );
            self.session.message_ids.resize(history_len, 0);
        }

        if boundary >= history_len {
            tracing::info!("no compaction needed: conversation within retention");
            return Ok(());
        }
        if boundary == 0 {
            tracing::info!("no compaction needed: all history must be retained");
            return Ok(());
        }

        // 1. Find incremental range and extract old summary for merging.
        let (compact_start, compact_end, existing_summary) = self.find_incremental_range(boundary);
        let to_compact: Vec<ChatMessage> = self.session.history[compact_start..compact_end].to_vec();

        if to_compact.is_empty() {
            tracing::info!("no new content to compact");
            return Ok(());
        }

        let compacted_count = to_compact.len();
        let removed_tokens: u64 = to_compact.iter().map(estimate_message_tokens).sum();

        tracing::info!(
            compact_start,
            compact_end,
            boundary,
            has_existing_summary = existing_summary.is_some(),
            "compaction range determined"
        );

        // Resolve the DB id of the last message being compacted (needed for both
        // success and fallback paths so restart can skip pre-compaction messages).
        let last_compacted_id = self.session.message_ids
            .get(compact_end.saturating_sub(1))
            .copied()
            .unwrap_or(0);

        // 2. Generate summary (incrementally merge old summary).
        let summary = match self.summarize_inline(&to_compact, existing_summary.as_deref(), model_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "summarizer failed, dropping pre-boundary history");
                self.drop_pre_boundary_with_record(boundary, last_compacted_id);
                return Ok(());
            }
        };

        if summary.trim().is_empty() {
            tracing::warn!("summarizer returned empty, dropping pre-boundary history");
            self.drop_pre_boundary_with_record(boundary, last_compacted_id);
            return Ok(());
        }

        // Quality audit (non-blocking).
        let (ok, reasons) = self.audit_summary_quality(&to_compact, &summary);
        if !ok {
            tracing::warn!(reasons = ?reasons, "summary quality audit failed (non-blocking)");
        }

        // Refresh memory index (summarizer may have written memory files via tools).
        {
            let memory_dir = std::path::Path::new(&self.config.prompt_config.workspace_dir)
                .join("memory");
            let files = crate::memory::scan_memory_files(&memory_dir);
            let entries: Vec<crate::memory::IndexEntry> =
                files.iter().map(crate::memory::IndexEntry::from).collect();
            let history = self.session.history.clone();
            self.attachments.diff_memory(&entries, &history);
            tracing::info!(memory_count = entries.len(), "memory index refreshed after compaction");
        }

        // 3. Replace history.
        let version = self.session.compact_version + 1;
        let summary_msg = ChatMessage::user_text(
            format!("[Context Summary] {}", summary)
        );
        let summary_tokens = estimate_message_tokens(&summary_msg);

        self.session.history.drain(compact_start..compact_end);
        self.session.history.insert(compact_start, summary_msg);

        self.session.message_ids.drain(compact_start..compact_end);
        self.session.message_ids.insert(compact_start, 0);

        self.session.compact_version = version;
        self.session.summary_metadata = Some(super::session_manager::SummaryMetadata {
            version,
            token_estimate: summary_tokens,
            up_to_message: last_compacted_id,
        });

        // 4. Persist summary and rotate history file.
        if let Some(ref hook) = self.persist_hook {
            hook.save_compaction(&self.session.id, &SummaryRecord {
                id: 0,
                version,
                summary: summary.clone(),
                up_to_message: last_compacted_id,
                token_estimate: Some(summary_tokens),
                created_at: chrono::Utc::now(),
            });

            // Archive the pre-compaction segment; the summary message inserted
            // above is part of surviving, so it lands in the new file on disk.
            let surviving: Vec<(i64, ChatMessage)> = self.session.message_ids.iter()
                .copied()
                .zip(self.session.history.iter().cloned())
                .collect();
            hook.rotate_history(&self.session.id, &surviving);

            // Reassign message_ids to match the new file's 1-based line numbers.
            for (i, id) in self.session.message_ids.iter_mut().enumerate() {
                *id = (i + 1) as i64;
            }
        }

        // 5. Adjust token tracker.
        self.token_tracker.adjust_for_compaction(removed_tokens, summary_tokens);

        let new_total = self.token_tracker.total_tokens();
        tracing::info!(
            compacted_messages = compacted_count,
            summary_tokens,
            removed_tokens,
            new_total_tokens = new_total,
            version,
            "context compaction completed"
        );

        // 6. Safety net: if still over threshold, truncate retention zone.
        // After drain+insert, retention zone now starts at compact_start + 1.
        let new_boundary = compact_start + 1;
        let context_window = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| cfg.context_window)
            .unwrap_or(u64::MAX);
        let threshold = (context_window as f64 * self.config.context.compact_threshold) as u64;
        if new_total > threshold {
            self.truncate_retention_zone(new_boundary, model_id);
        }

        // 7. No need to reset attachment state — compaction removes old history
        //    entries, so the next diff will naturally rebuild from the remaining
        //    history (which may be empty → full re-listing).

        Ok(())
    }

    /// Compact history so that the compressible prefix fits within `target_window`.
    ///
    /// compress_budget = target_window * compact_threshold - system_prompt_tokens - tool_spec_tokens
    ///
    /// `compact_threshold` implicitly reserves `(1 - threshold)` for model output and headroom.
    /// Tool spec tokens are subtracted because the summarizer receives tool definitions too.
    /// `model_id` is the model that will summarize and later consume the compacted context.
    async fn compact_to_budget(
        &mut self,
        model_id: &str,
        target_window: u64,
    ) -> anyhow::Result<()> {
        // Estimate overhead sent alongside the history in every summarizer request.
        let system_prompt_tokens = estimate_tokens(&self.system_prompt);
        let tool_spec_tokens: u64 = self.build_tool_specs().iter().map(|spec| {
            let schema = spec.input_schema.to_string();
            estimate_tokens(&spec.name)
                + spec.description.as_deref().map_or(0, estimate_tokens)
                + estimate_tokens(&schema)
                + 8 // per-tool structural overhead
        }).sum();

        // Usable space up to the compaction threshold; the remaining (1-threshold) fraction
        // is implicitly reserved for the summary output and safety headroom.
        let threshold = self.config.context.compact_threshold;
        let compress_budget = ((target_window as f64 * threshold) as u64)
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(tool_spec_tokens);

        if compress_budget == 0 {
            anyhow::bail!(
                "context window ({}) too small to compact into (model '{}')",
                target_window, model_id
            );
        }

        let retain_count = self.config.context.retain_work_units.max(1);
        let boundary = match super::work_unit::find_compaction_boundary_for_budget(
            &self.session.history,
            compress_budget,
            retain_count,
        ) {
            Some(b) => b,
            None => {
                tracing::debug!(
                    compress_budget,
                    "no compaction boundary for budget (prefix too large or too few work units)"
                );
                return Ok(());
            }
        };

        let history_len = self.session.history.len();
        if boundary == 0 || boundary >= history_len {
            return Ok(());
        }

        tracing::info!(
            target_window,
            compress_budget,
            system_prompt_tokens,
            tool_spec_tokens,
            boundary,
            "compaction triggered"
        );

        self.compact_with_boundary(model_id, boundary).await
    }

    /// Check the routing fallback chain and proactively compact if the current context
    /// would overflow any fallback model's context window.
    ///
    /// Only the first (highest-priority) overflowing model triggers compaction;
    /// we compact once per turn at most.
    async fn maybe_compact_for_fallback(&mut self) -> anyhow::Result<()> {
        let routing_models = self.registry.get_chat_routing_models();
        if routing_models.len() <= 1 {
            return Ok(()); // no fallback chain
        }

        let conservative_total = (self.token_tracker.total_tokens() as f64 * 1.25) as u64;

        // Find the first fallback model whose window the current context would overflow.
        // Extract without holding a borrow before the mutable &mut self call below.
        let target: Option<(String, u64)> = routing_models
            .iter()
            .skip(1) // skip primary model
            .find_map(|model_id| {
                let cfg = self.registry.get_chat_model_config(model_id).ok()?;
                let window = cfg.context_window?;
                if conservative_total > window { Some((model_id.clone(), window)) } else { None }
            });

        if let Some((fallback_model, window)) = target {
            tracing::info!(
                fallback_model = %fallback_model,
                conservative_total,
                window,
                "pre-fallback: context would overflow fallback model, compacting"
            );
            if let Err(e) = self.compact_to_budget(&fallback_model, window).await {
                tracing::warn!(error = %e, "pre-fallback compaction failed");
            }
        }

        Ok(())
    }

    /// Drop all history before the boundary (no summary, no recovery).
    fn drop_pre_boundary_with_record(&mut self, boundary: usize, last_compacted_id: i64) {
        let removed_tokens: u64 = self.session.history[..boundary]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        self.session.history.drain(..boundary);
        self.session.message_ids.drain(..boundary);
        self.token_tracker.adjust_for_compaction(removed_tokens, 0);

        // Save a placeholder summary so restart skips pre-compaction messages.
        let version = self.session.compact_version + 1;
        self.session.compact_version = version;
        if let Some(ref hook) = self.persist_hook {
            hook.save_compaction(&self.session.id, &SummaryRecord {
                id: 0,
                version,
                summary: "[历史已截断]".to_string(),
                up_to_message: last_compacted_id,
                token_estimate: None,
                created_at: chrono::Utc::now(),
            });

            // Archive the pre-compaction segment; surviving messages go into a new file.
            let surviving: Vec<(i64, ChatMessage)> = self.session.message_ids.iter()
                .copied()
                .zip(self.session.history.iter().cloned())
                .collect();
            hook.rotate_history(&self.session.id, &surviving);
        }
    }

    /// Safety net: truncate oversized tool results in retention zone, or drop oldest unit.
    fn truncate_retention_zone(&mut self, boundary: usize, model_id: &str) {
        let safety_max_tokens = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| cfg.context_window)
            .map(|cw| (cw / 20) as usize)
            .unwrap_or(5_000);

        // 1. Truncate abnormally large tool results in retention zone.
        for i in boundary..self.session.history.len() {
            if self.session.history[i].role != "tool" {
                continue;
            }
            let text = self.session.history[i].text_content();
            let est = estimate_tokens(&text);
            if est > safety_max_tokens as u64 {
                let truncated = crate::tools::truncation::truncate_output(&text, safety_max_tokens);
                self.session.history[i].parts = vec![
                    ContentPart::Text { text: truncated }
                ];

                let old_est = est;
                let new_est = estimate_tokens(&self.session.history[i].text_content());
                self.token_tracker.adjust_for_compaction(old_est, new_est);

                tracing::warn!(
                    idx = i,
                    old_tokens = old_est,
                    new_tokens = new_est,
                    "safety-net truncated oversized tool result in retention zone"
                );
            }
        }

        // 2. Still over threshold? Drop oldest retained work unit.
        let threshold = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| cfg.context_window)
            .map(|cw| (cw as f64 * self.config.context.compact_threshold) as u64)
            .unwrap_or(u64::MAX);
        if self.token_tracker.total_tokens() > threshold {
            self.drop_oldest_retained_work_unit(boundary);
        }
    }

    /// Drop the oldest work unit in the retention zone (last resort).
    fn drop_oldest_retained_work_unit(&mut self, boundary: usize) {
        let retained = &self.session.history[boundary..];
        let units = super::work_unit::extract_work_units(retained);

        if units.len() <= 1 { return; }

        let unit = &units[0];
        let start = boundary + unit.user_start;
        let end = boundary + unit.end + 1;

        let to_remove = &self.session.history[start..end];
        let removed_tokens: u64 = to_remove.iter().map(estimate_message_tokens).sum();

        self.session.history.drain(start..end);
        self.session.message_ids.drain(start..end);
        self.token_tracker.adjust_for_compaction(removed_tokens, 0);

        tracing::warn!(
            dropped_start = start,
            dropped_end = end,
            removed_tokens,
            "dropped oldest retained work unit after truncation insufficient"
        );
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
