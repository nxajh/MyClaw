//! Agent — shared factory for AgentLoop instances.
//!
//! Agent holds shared resources (registry, skills, config) and creates
//! per-session AgentLoop handles.
//!
//! DDD: Agent depends on `dyn ServiceRegistry` (Domain trait), not on
//! `Registry` (Infrastructure concrete type). This keeps the Application
//! layer decoupled from Infrastructure.

use std::sync::Arc;

use myclaw_capability::capability::Capability;
use myclaw_capability::chat::{
    BoxStream, ChatMessage, ChatRequest, StopReason, StreamEvent, ToolCall, ToolSpec,
};
use myclaw_capability::service_registry::ServiceRegistry;
use myclaw_capability::tool::ToolResult;
use futures_util::StreamExt;

use super::session_manager::Session;
use super::skills::SkillsManager;
use crate::prompt::{SystemPromptBuilder, SystemPromptConfig};

/// AgentConfig controls loop breaker thresholds and tool call limits.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard cap on tool calls per turn. 0 = unlimited.
    pub max_tool_calls: usize,
    /// Maximum history messages to keep in memory. 0 = unlimited.
    pub max_history: usize,
    /// System prompt builder config.
    pub prompt_config: SystemPromptConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            max_history: 200,
            prompt_config: SystemPromptConfig::default(),
        }
    }
}

/// Agent is the shared factory — call `.loop_for(session)` to get an AgentLoop.
#[derive(Clone)]
pub struct Agent {
    registry: Arc<dyn ServiceRegistry>,
    skills: Arc<SkillsManager>,
    config: AgentConfig,
    system_prompt: String,
}

impl Agent {
    pub fn new(registry: Arc<dyn ServiceRegistry>, skills: SkillsManager, config: AgentConfig) -> Self {
        Self {
            registry,
            skills: Arc::new(skills),
            config,
            system_prompt: String::new(),
        }
    }

    /// Set the system prompt directly (overrides builder).
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    /// Create an AgentLoop for the given session.
    /// The system prompt is built from SystemPromptConfig + SkillsManager.
    pub fn loop_for(&self, session: Session) -> AgentLoop {
        let prompt = if !self.system_prompt.is_empty() {
            // Direct prompt set via with_system_prompt()
            self.system_prompt.clone()
        } else {
            // Build from config
            let builder = SystemPromptBuilder::new(self.config.prompt_config.clone());
            let tool_names: Vec<String> = self
                .skills
                .all_tools()
                .iter()
                .map(|t| t.name().to_string())
                .collect();
            builder.build(&self.skills, &tool_names)
        };

        AgentLoop {
            registry: Arc::clone(&self.registry),
            skills: Arc::clone(&self.skills),
            config: self.config.clone(),
            session,
            system_prompt: prompt,
        }
    }
}

/// Per-session agent loop handle. Execute `run(user_message)` to process a message.
pub struct AgentLoop {
    registry: Arc<dyn ServiceRegistry>,
    skills: Arc<SkillsManager>,
    config: AgentConfig,
    session: Session,
    /// Template for the system prompt.
    system_prompt: String,
}

impl AgentLoop {
    /// Process a user message and return the assistant's text response.
    ///
    /// This is the main entry point called by the orchestrator.
    pub async fn run(&mut self, user_message: &str) -> anyhow::Result<String> {
        tracing::info!(user_input = %user_message, "user message received");

        // 1. Add user message to session.
        self.session.add_user_text(user_message.to_string());

        // 2. Build the full message list for this turn.
        let messages = self.build_messages().await?;

        // 3. Run the chat loop (handles tool calls iteratively).
        let text = self.chat_loop(messages).await?;

        // 4. Persist assistant response.
        self.session.add_assistant_text(text.clone());

        Ok(text)
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
    async fn chat_loop(&mut self, mut messages: Vec<ChatMessage>) -> anyhow::Result<String> {
        let mut tool_calls_count = 0usize;

        loop {
            // 1. Get a chat provider via registry.
            let (provider, model_id) = self
                .registry
                .get_chat_provider(Capability::Chat)?;

            // 2. Build tool specs from skills manager.
            let tools = self.build_tool_specs();

            // 3. Build request.
            let req = ChatRequest {
                model: &model_id,
                messages: &messages,
                temperature: None,
                max_tokens: None,
                thinking: None,
                stop: None,
                seed: None,
                tools: if tools.is_empty() { None } else { Some(&tools) },
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
            Self::log_chat_io("request", &model_id, &messages, &tools, None);

            let response = self.collect_stream(stream).await?;
            tracing::info!(text_len = response.text.len(), tool_calls = response.tool_calls.len(), stop = ?response.stop_reason, "chat stream collected");

            // Log raw response.
            Self::log_chat_io("response", &model_id, &[], &[], Some(&response));

            // 5. No tool calls → return text.
            if response.tool_calls.is_empty() {
                if response.text.is_empty() {
                    tracing::warn!("chat response text is empty");
                }
                return Ok(response.text);
            }

            // 6. Tool calls present → execute them and append results.
            for call in &response.tool_calls {
                tracing::info!(tool = %call.name, id = %call.id, arguments = %call.arguments, "model requested tool call");
            }

            // Build the assistant's tool_calls message to append to conversation.
            let assistant_tool_calls: Vec<serde_json::Value> = response.tool_calls.iter().map(|call| {
                serde_json::json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    }
                })
            }).collect();
            let mut assistant_msg = ChatMessage::assistant_text(&response.text);
            assistant_msg.tool_calls = Some(assistant_tool_calls);
            messages.push(assistant_msg);

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
                let result_content = match &result {
                    Ok(r) => r.output.clone(),
                    Err(e) => format!("error: {}", e),
                };

                tracing::info!(tool = %call.name, success = result.is_ok(), "tool result:\n{}", result_content);

                // Append tool result with tool_call_id.
                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                messages.push(tool_msg);

                // Append to session history.
                self.session
                    .add_assistant_text(serde_json::to_string(&call).unwrap_or_default());
                self.session
                    .add_assistant_text(result_content);
            }
        }
    }

    /// Collect all events from a streaming chat response.
    async fn collect_stream(
        &self,
        mut stream: BoxStream<StreamEvent>,
    ) -> anyhow::Result<CollectedResponse> {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut stop_reason = StopReason::EndTurn;

        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Delta { text: delta } => text.push_str(&delta),
                StreamEvent::Thinking { .. } => {
                    // TODO: surface reasoning in response or log.
                }
                StreamEvent::ToolCallStart { id, name, initial_arguments } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: initial_arguments,
                    });
                }
                StreamEvent::ToolCallDelta { id, delta } => {
                    // OpenAI-compatible streaming: only the first chunk carries
                    // id + name; subsequent chunks may have empty id.
                    // Match by id if present, otherwise append to last tool call.
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
                        // No id — append arguments to the most recent tool call.
                        last.arguments.push_str(&delta);
                    }
                }
                StreamEvent::ToolCallEnd { id, name, arguments } => {
                    if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                        call.name = name;
                        call.arguments = arguments;
                    }
                }
                StreamEvent::Usage(_) => {
                    // TODO: record usage.
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

        Ok(CollectedResponse {
            text,
            tool_calls,
            stop_reason,
        })
    }

    /// Build tool specs from the skills manager.
    fn build_tool_specs(&self) -> Vec<ToolSpec> {
        self.skills
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
    async fn execute_tool(&self, call: &ToolCall) -> anyhow::Result<ToolResult> {
        let tool = self.skills.get(&call.name).ok_or_else(|| {
            anyhow::anyhow!("Unknown tool: '{}'", call.name)
        })?;

        let args: serde_json::Value = if call.arguments.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&call.arguments).unwrap_or_else(|_| {
                serde_json::json!({ "raw": &call.arguments })
            })
        };

        tool.execute(args).await
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
        tools: &[ToolSpec],
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
}

/// Response collected from a chat stream.
struct CollectedResponse {
    text: String,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    stop_reason: StopReason,
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
        }
    }
}
