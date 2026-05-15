use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;

use crate::providers::{
    BoxStream, ChatMessage, ChatRequest, ChatUsage, ContentPart, ServiceRegistry,
    StreamEvent, ThinkingConfig, ToolCall,
};
use crate::providers::capability_chat::ToolSpec;
use crate::providers::Capability;
use crate::agents::resource_provider::ResourceProvider;
use crate::agents::tool_executor::MemoryToolExecutor;
use crate::agents::tool_registry::ToolRegistry;
use crate::agents::agent_impl::types::estimate_message_tokens;

/// Result returned by CompactionExecutor::execute.
/// AgentLoop is responsible for applying it to session (drain/insert history, update metadata).
pub(crate) struct CompactionResult {
    pub compact_start: usize,
    pub compact_end: usize,
    pub summary: String,
    pub summary_tokens: u64,
    pub removed_tokens: u64,
    pub compacted_count: usize,
}

/// Generates a compaction summary from a read-only history slice.
///
/// Does not mutate session — the caller (AgentLoop.compact_with_boundary) applies
/// the result. MemoryToolExecutor restricts the summarizer to file tools only,
/// preventing accidental session mutation or sub-agent spawning.
pub(crate) struct CompactionExecutor {
    registry: Arc<dyn ServiceRegistry>,
    resources: Arc<ResourceProvider>,
    memory_executor: MemoryToolExecutor,
    max_rounds: usize,
    stream_chunk_timeout_secs: u64,
}

impl CompactionExecutor {
    pub(crate) fn new(
        registry: Arc<dyn ServiceRegistry>,
        resources: Arc<ResourceProvider>,
        tools: Arc<ToolRegistry>,
        stream_chunk_timeout_secs: u64,
    ) -> Self {
        Self {
            registry,
            resources,
            memory_executor: MemoryToolExecutor::new(tools),
            max_rounds: 10,
            stream_chunk_timeout_secs,
        }
    }

    /// Generate a compaction summary for `history[0..boundary]`.
    ///
    /// `tool_specs` must be the same spec list used for the main LLM request so
    /// the provider's prefix cache key (model + system_prompt + tool_definitions)
    /// matches and the summarizer call hits the cache.
    pub(crate) async fn execute(
        &self,
        history: &[ChatMessage],
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        boundary: usize,
        model_id: &str,
    ) -> anyhow::Result<CompactionResult> {
        let (compact_start, compact_end, existing_summary) =
            find_incremental_range(history, boundary);

        let to_compact: Vec<ChatMessage> = history[compact_start..compact_end].to_vec();
        if to_compact.is_empty() {
            anyhow::bail!("no content to compact");
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

        let summary = self
            .summarize(&to_compact, existing_summary.as_deref(), system_prompt, tool_specs, model_id)
            .await?;

        let (ok, reasons) = audit_summary_quality(&to_compact, &summary);
        if !ok {
            tracing::warn!(reasons = ?reasons, "summary quality audit failed (non-blocking)");
        }

        let summary_tokens = estimate_message_tokens(&ChatMessage::user_text(summary.clone()));

        Ok(CompactionResult {
            compact_start,
            compact_end,
            summary,
            summary_tokens,
            removed_tokens,
            compacted_count,
        })
    }

    async fn summarize(
        &self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        model_id: &str,
    ) -> anyhow::Result<String> {
        match self.do_summarize(to_compact, existing_summary, system_prompt, tool_specs, model_id).await {
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

    async fn do_summarize(
        &self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        model_id: &str,
    ) -> anyhow::Result<String> {
        let provider = match self.registry.get_chat_provider_by_model(model_id) {
            Some((p, _)) => p,
            None => {
                let (p, _) = self.registry.get_chat_provider(Capability::Chat)?;
                p
            }
        };

        let mut messages: Vec<ChatMessage> = Vec::new();

        if !system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(system_prompt));
        }

        for msg in to_compact {
            messages.push(strip_images(msg));
        }

        let memory_prompt = build_memory_prompt(&self.resources.knowledge_dir);
        let prompt = build_summarizer_prompt(to_compact.len(), existing_summary, &memory_prompt);
        messages.push(ChatMessage::user_text(prompt));

        let thinking = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| {
                if cfg.reasoning {
                    Some(ThinkingConfig { enabled: true, effort: None })
                } else {
                    None
                }
            });

        let mut round = 0;
        let final_text = loop {
            round += 1;
            if round > self.max_rounds {
                anyhow::bail!("summarize loop exceeded {} rounds", self.max_rounds);
            }

            let req = ChatRequest {
                model: model_id,
                messages: &messages,
                temperature: None,
                max_tokens: Some(20_000),
                thinking: thinking.clone(),
                stop: None,
                seed: None,
                tools: if tool_specs.is_empty() { None } else { Some(&tool_specs[..]) },
                stream: true,
            };

            let stream = provider.chat(req)?;
            let response = self.collect_summary_stream(stream).await?;

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

            if response.tool_calls.is_empty() {
                break response.text;
            }

            tracing::info!(
                round,
                tool_calls = response.tool_calls.len(),
                "summarize: model requested tool calls"
            );

            let mut assistant_msg = ChatMessage::assistant_text(&response.text);
            assistant_msg.tool_calls = Some(response.tool_calls.clone());
            if let Some(ref thinking_text) = response.reasoning_content {
                assistant_msg.parts.insert(
                    0,
                    ContentPart::Thinking { thinking: thinking_text.clone() },
                );
            }
            messages.push(assistant_msg);

            for call in &response.tool_calls {
                tracing::info!(tool = %call.name, id = %call.id, "summarize: executing tool");
                let result = self.memory_executor.execute(call).await;
                let (result_content, is_error) = match &result {
                    Ok(r) => {
                        let mut out = r.output.clone();
                        if let Some(ref err) = r.error {
                            if out.is_empty() { out = format!("error: {}", err); }
                        }
                        (out, !r.success)
                    }
                    Err(e) => (format!("error: {}", e), true),
                };
                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(is_error);
                messages.push(tool_msg);
            }
        };

        Ok(final_text)
    }

    async fn collect_summary_stream(&self, mut stream: BoxStream<StreamEvent>) -> anyhow::Result<SummaryResponse> {
        let mut text = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage: Option<ChatUsage> = None;
        let chunk_timeout = Duration::from_secs(self.stream_chunk_timeout_secs);

        loop {
            match tokio::time::timeout(chunk_timeout, stream.next()).await {
                Ok(Some(event)) => match event {
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
                        tool_calls.push(ToolCall { id, name, arguments: initial_arguments });
                    }
                    StreamEvent::ToolCallDelta { id, delta } => {
                        if !id.is_empty() {
                            if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                                call.arguments.push_str(&delta);
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
                        } else {
                            usage = Some(u);
                        }
                    }
                    StreamEvent::Done { .. } => break,
                    StreamEvent::HttpError { message, .. } => anyhow::bail!("summarizer stream error: {}", message),
                    StreamEvent::Error(e) => anyhow::bail!("summarizer stream error: {}", e),
                },
                Ok(None) => {
                    tracing::warn!("summarizer stream ended without Done event");
                    break;
                }
                Err(_) => anyhow::bail!(
                    "summarizer stream chunk timeout after {}s",
                    self.stream_chunk_timeout_secs
                ),
            }
        }

        Ok(SummaryResponse { text, reasoning_content, tool_calls, usage })
    }
}

struct SummaryResponse {
    text: String,
    reasoning_content: Option<String>,
    tool_calls: Vec<ToolCall>,
    usage: Option<ChatUsage>,
}

fn find_incremental_range(history: &[ChatMessage], boundary: usize) -> (usize, usize, Option<String>) {
    let last_summary = history[..boundary].iter().rposition(|m| {
        m.role == "user" && m.text_content().starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
    });
    match last_summary {
        Some(idx) => {
            let existing = history[idx].text_content();
            (idx, boundary, Some(existing))
        }
        None => (0, boundary, None),
    }
}

fn strip_images(msg: &ChatMessage) -> ChatMessage {
    let mut cleaned = msg.clone();
    cleaned.parts = cleaned.parts.into_iter().map(|part| match part {
        ContentPart::ImageUrl { .. } => ContentPart::Text { text: "[image]".into() },
        ContentPart::ImageB64 { .. } => ContentPart::Text { text: "[image]".into() },
        other => other,
    }).collect();
    cleaned
}

fn build_memory_prompt(knowledge_dir: &str) -> String {
    format!(
        "\n\
         \n\
         You also have a persistent memory system. The memory directory is `{knowledge_dir}/` and\n\
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
         description quality rules:\n\
         - DO NOT repeat the filename — description must add information beyond what the name already says\n\
         - MUST include key terms that help decide when to read this file (tool names, feature names, bug symptoms, decision outcomes)\n\
         - BAD: \"MyClaw memory system design decisions\" (name already says this)\n\
         - GOOD: \"记忆索引从system prompt迁到system-reminder注入；diff_memory始终检查history不依赖内存旧文本\"\n\
         - If updating an existing file, update its description to reflect the latest content\n\
         \n\
         Other rules:\n\
         - ONLY save things NOT derivable from code/git (user preferences, decisions, corrections)\n\
         - Check the existing memory index to avoid duplicates — update existing files instead of creating duplicates\n\
         - If existing memories are outdated or contradicted, update or delete them\n\
         - Keep name short, lowercase, underscores (becomes the filename: {knowledge_dir}/{{name}}.md)\n\
         - If no memory changes needed, skip this entirely and just output the summary\n\
         \n\
         You may use file_write, file_edit, and file_read tools for memory operations ONLY.\n\
         Do not use other tools."
    )
}

fn build_summarizer_prompt(msg_count: usize, existing_summary: Option<&str>, memory_prompt: &str) -> String {
    match existing_summary {
        Some(base) => format!(
            "Below is a PREVIOUS SUMMARY followed by NEW conversation messages.\n\
             \n\
             === PREVIOUS SUMMARY ===\n{base}\n\
             === END PREVIOUS SUMMARY ===\n\
             \n\
             Merge the new messages into the previous summary. Produce a single \n\
             updated summary that covers everything.\n\
             \n\
             Output the summary as plain text with the following REQUIRED sections. \n\
             Mark items as Resolved or Pending so the model knows what is active:\n\
             \n\
             ## Active Task\n\
             What the user is currently doing and its status.\n\
             \n\
             ## Key Decisions\n\
             Important choices made and why.\n\
             \n\
             ## Technical Context\n\
             Files modified, code locations, APIs used, configurations changed.\n\
             \n\
             ## Resolved\n\
             Tasks/questions that were completed or answered.\n\
             \n\
             ## Pending\n\
             Tasks/questions still open or deferred.\n\
             \n\
             ## Errors & Fixes\n\
             Problems encountered and their solutions.\n\
             \n\
             Rules:\n\
             - Mark resolved items clearly (prefix with [Resolved])\n\
             - Mark pending items clearly (prefix with [Pending])\n\
             - Omit raw tool output (large code blocks, logs, file contents)\n\
             - Use the same language as the conversation\n\
             - Be thorough but concise: every important detail should be preserved{memory_prompt}"
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
             - This conversation has {msg_count} messages to summarize{memory_prompt}"
        ),
    }
}

fn audit_summary_quality(to_compact: &[ChatMessage], summary: &str) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();

    if summary.chars().count() < 100 {
        reasons.push(format!(
            "summary too short: {} chars (minimum 100)",
            summary.chars().count()
        ));
    }

    let original_paths = extract_file_paths(to_compact);
    if !original_paths.is_empty() {
        let preserved = original_paths.iter().filter(|p| summary.contains(*p)).count();
        if preserved == 0 && original_paths.len() <= 5 {
            reasons.push(format!(
                "no file paths preserved (original had {})",
                original_paths.len()
            ));
        }
    }

    (reasons.is_empty(), reasons)
}

fn extract_file_paths(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?:/[\w/.-]+\.\w{1,5})|(?:src/[\w/.-]+)").unwrap()
    });
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(0) {
                let p = m.as_str().to_string();
                if seen.insert(p.clone()) {
                    paths.push(p);
                }
            }
        }
    }
    paths
}
