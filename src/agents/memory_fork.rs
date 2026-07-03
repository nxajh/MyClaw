//! Turn-end memory extraction fork.
//!
//! Spawns a background mini-agent at the end of each turn (when the model
//! produces a final response with no tool calls). The fork shares the main
//! conversation's prompt cache prefix and uses a restricted tool set to
//! persist durable memories. Fire-and-forget — the main turn does not wait.

use std::sync::Arc;

use anyhow::Result;
use futures_util::StreamExt;

use crate::agents::session::Session;
use crate::agents::tool_registry::ToolRegistry;
use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest, ToolSpec};
use crate::providers::{BoxStream, ChatUsage, ProviderRegistry, StreamEvent, ThinkingConfig, ToolCall};
use crate::providers::capability_tool::ToolResult;

/// Input bundle for a memory extraction fork.
///
/// Cloned from `Agent::run` at turn end. All fields are `Arc` or owned so the
/// struct is `'static` and can move into `tokio::spawn`.
pub struct ForkInput {
    /// System prompt + full history (clone of `messages` in `Agent::run`).
    pub messages: Vec<ChatMessage>,
    /// Model ID (same as the main turn — maximises cache hit).
    pub model_id: String,
    /// Chat provider (Arc clone, cheap).
    pub provider: Arc<dyn ChatProvider>,
    /// Full tool spec list (for prefix-cache key matching). Execution is
    /// gated by the allow-list below.
    pub tool_specs: Vec<ToolSpec>,
    /// Tool registry for actual execution.
    pub tool_registry: Arc<ToolRegistry>,
    /// Session owner (routing key) for memory scoping.
    pub session_owner: String,
    /// Knowledge directory (memory files live here).
    pub knowledge_dir: String,
    /// Provider registry for resolving model config (reasoning flag).
    pub registry: Arc<dyn ProviderRegistry>,
}

/// Tools the fork is permitted to call. Mirrors `MemoryToolExecutor::ALLOWED`.
const ALLOWED: &[&str] = &[
    "file_read",
    "file_write",
    "file_edit",
    "shell",
    "memory_manage",
];

const MAX_ROUNDS: usize = 5;

/// Run the memory extraction fork. Best-effort — logs errors, never panics.
pub async fn run_memory_fork(input: ForkInput) {
    tracing::info!(
        model = %input.model_id,
        msg_count = input.messages.len(),
        "memory_fork: starting"
    );

    match run_memory_fork_inner(input).await {
        Ok(written) => {
            if written > 0 {
                tracing::info!(files_written = written, "memory_fork: memories saved");
            } else {
                tracing::debug!("memory_fork: no memories saved");
            }
        }
        Err(e) => {
            tracing::warn!(err = %e, "memory_fork: failed");
        }
    }
}

async fn run_memory_fork_inner(input: ForkInput) -> Result<usize> {
    // Build a minimal Session shell — memory tools only need `owner`.
    let mut session_shell = Session::new("memory_fork".to_string());
    session_shell.owner = input.session_owner.clone();

    // Resolve thinking config from model config (same as do_summarize).
    let thinking = input
        .registry
        .get_chat_model_config(&input.model_id)
        .ok()
        .and_then(|cfg| {
            if cfg.reasoning {
                Some(ThinkingConfig {
                    enabled: true,
                    effort: None,
                })
            } else {
                None
            }
        });

    // Assemble messages: original context + extraction prompt.
    let mut messages = input.messages;
    let extraction_prompt = build_extraction_prompt(&input.knowledge_dir);
    messages.push(ChatMessage::user_text(extraction_prompt));

    let provider = input.provider;
    let model_id = &input.model_id;
    let tool_specs = &input.tool_specs;
    let tool_registry = &input.tool_registry;
    let mut files_written = 0usize;

    for round in 1..=MAX_ROUNDS {
        let req = ChatRequest {
            model: model_id,
            messages: &messages,
            temperature: None,
            max_tokens: None,
            thinking: thinking.clone(),
            stop: None,
            seed: None,
            tools: if tool_specs.is_empty() {
                None
            } else {
                Some(tool_specs)
            },
            stream: true,
        };

        let stream = provider.chat(req)?;
        let response = collect_fork_stream(stream).await?;

        if response.tool_calls.is_empty() {
            break;
        }

        tracing::info!(
            round,
            tool_calls = response.tool_calls.len(),
            "memory_fork: executing tool calls"
        );

        // Append assistant message with tool calls (preserves thinking).
        let mut assistant_msg = ChatMessage::assistant_text(&response.text);
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        if let Some(ref rc) = response.reasoning_content {
            assistant_msg.parts.insert(
                0,
                crate::providers::ContentPart::Thinking {
                    thinking: rc.clone(),
                    signature: response.thinking_signature.clone(),
                },
            );
        }
        messages.push(assistant_msg);

        for call in &response.tool_calls {
            if !ALLOWED.contains(&call.name.as_str()) {
                tracing::warn!(tool = %call.name, "memory_fork: tool not allowed, blocking");
                let mut tool_msg = ChatMessage::text(
                    "tool",
                    format!("tool '{}' not available during memory extraction", call.name),
                );
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(true);
                messages.push(tool_msg);
                continue;
            }

            let tool = match tool_registry.get(&call.name) {
                Some(t) => t,
                None => {
                    let mut tool_msg = ChatMessage::text(
                        "tool",
                        format!("tool '{}' not found in registry", call.name),
                    );
                    tool_msg.tool_call_id = Some(call.id.clone());
                    tool_msg.is_error = Some(true);
                    messages.push(tool_msg);
                    continue;
                }
            };

            let args = if call.arguments.is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": &call.arguments }))
            };

            let raw = tool.execute(args, &session_shell).await;
            let result: anyhow::Result<ToolResult> = raw;
            let (result_content, is_error) = match &result {
                Ok(r) => {
                    if r.success && call.name == "memory_manage" {
                        files_written += 1;
                    }
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

            let mut tool_msg = ChatMessage::text("tool", &result_content);
            tool_msg.tool_call_id = Some(call.id.clone());
            tool_msg.is_error = Some(is_error);
            messages.push(tool_msg);
        }
    }

    Ok(files_written)
}

/// Stream collector for the fork — simplified version of
/// `ContextEngine::collect_summary_stream` (no UI streaming).
struct ForkResponse {
    text: String,
    reasoning_content: Option<String>,
    thinking_signature: Option<String>,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    usage: Option<ChatUsage>,
}

async fn collect_fork_stream(mut stream: BoxStream<StreamEvent>) -> Result<ForkResponse> {
    let mut text = String::new();
    let mut reasoning_content: Option<String> = None;
    let mut thinking_signature: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage: Option<ChatUsage> = None;
    let chunk_timeout = crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT;

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
                    name,
                    delta,
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
                StreamEvent::Done { .. } => break,
                StreamEvent::ThinkingSignature { signature } => {
                    thinking_signature = Some(signature);
                }
                StreamEvent::HttpError { message, .. } => {
                    anyhow::bail!("memory_fork stream error: {}", message)
                }
                StreamEvent::Error(e) => anyhow::bail!("memory_fork stream error: {}", e),
            },
            Ok(None) => {
                tracing::warn!("memory_fork stream ended without Done event");
                break;
            }
            Err(_) => anyhow::bail!(
                "memory_fork stream chunk timeout after {}s",
                chunk_timeout.as_secs()
            ),
        }
    }

    Ok(ForkResponse {
        text,
        reasoning_content,
        thinking_signature,
        tool_calls,
        usage,
    })
}

/// Build the extraction prompt for the fork.
///
/// Independent from `context_engine::build_memory_prompt` — tuned for
/// turn-end extraction (not compaction), with its own taxonomy and rules.
fn build_extraction_prompt(knowledge_dir: &str) -> String {
    // Build existing memory index so the model avoids duplicates.
    let existing_index = if !knowledge_dir.is_empty() {
        let memory_dir = std::path::Path::new(knowledge_dir);
        let files = crate::memory::scan_memory_files(memory_dir);
        if files.is_empty() {
            String::from("(empty — no memories yet)")
        } else {
            let entries: Vec<crate::memory::IndexEntry> =
                files.iter().map(crate::memory::IndexEntry::from).collect();
            crate::memory::format_wiki_index(&entries)
        }
    } else {
        String::from("(memory directory not configured)")
    };

    format!(
        "\n\n---\n\
         You are now acting as the memory extraction subagent. Analyze the conversation above \
         and decide if any durable facts should be saved, updated, or removed from persistent memory.\n\
         \n\
         ## Existing memory index\n\
         {existing_index}\n\
         \n\
         Check this list before writing — update an existing file rather than creating a duplicate.\n\
         \n\
         ## What to save\n\
         - User preferences, habits, and communication style\n\
         - Project decisions and their rationale\n\
         - Behavior corrections (things you did wrong that the user pointed out)\n\
         - Stable facts not derivable from code/git\n\
         \n\
         ## What NOT to save\n\
         - Task progress, completed-work logs, temporary TODO state\n\
         - Facts that will be stale in a week\n\
         - Code snippets, file contents, git diffs\n\
         \n\
         ## How to save\n\
         Use the `memory_manage` tool with action `add` (new), `replace` (update), or `remove` (delete).\n\
         \n\
         The tool auto-generates YAML frontmatter for you. Pass metadata as tool parameters:\n\
         - name: short_snake_case_name\n\
         - description: one-line description (under 150 chars)\n\
         - type: user|feedback|project|reference\n\
         - tags: [optional, for, searchability]\n\
         \n\
         The `content` parameter is the memory BODY ONLY — plain markdown text, NO frontmatter, NO `---` blocks.\n\
         The tool will prepend the frontmatter automatically.\n\
         \n\
         description quality rules:\n\
         - DO NOT repeat the filename — description must add information beyond what the name says\n\
         - MUST include key terms that help decide when to read this file\n\
         - If updating an existing file, update its description to reflect the latest content\n\
         \n\
         Other rules:\n\
         - Write memories as declarative facts, not instructions\n\
         - Only `user` and `feedback` types are injected into every conversation — use `project`/`reference` for on-demand context\n\
         - If no memory changes are needed, respond with exactly: no memory changes needed\n\
         \n\
         You have a limited turn budget. Be efficient: decide what to write, then write it.\n\
         Available tools: file_read, file_write, file_edit, shell (read-only recommended), memory_manage.",
        existing_index = existing_index,
    )
}
