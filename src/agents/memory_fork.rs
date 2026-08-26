//! Turn-end memory extraction fork.
//!
//! Spawns a background mini-agent at the end of each turn (when the model
//! produces a final response with no tool calls). The fork shares the main
//! conversation's prompt cache prefix and uses a restricted tool set to
//! persist durable memories. Fire-and-forget — the main turn does not wait.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::{Mutex, Semaphore};


use crate::agents::tool_registry::ToolRegistry;
use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest, ToolSpec};
use crate::providers::capability_tool::ToolResult;
use crate::providers::{
    BoxStream, ChatUsage, ProviderRegistry, StreamEvent, ThinkingConfig, ToolCall,
};

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
    /// Real session ID for provenance annotation (e.g. `myclaw/s/019fe564-...`).
    pub session_id: String,
    /// Memory root directory (memory files live here).
    pub memory_root: String,
    /// Provider registry for resolving model config (reasoning flag).
    pub registry: Arc<dyn ProviderRegistry>,
}

/// Tools the fork is permitted to call. Mirrors `MemoryToolExecutor::ALLOWED`.
const ALLOWED: &[&str] = &[
    "memory_list",
    "memory_view",
    "memory_search",
    "memory_manage",
    "session_query",
];

const MAX_ROUNDS: usize = 3;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(90);
const SESSION_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// Cap on preserved user messages. User messages carry the durable facts /
/// profile details — unlike assistant replies they are never dropped below
/// this bound (profile fidelity, RFC P1).
const MAX_FORK_USER_MESSAGES: usize = 40;
const CONTEXT_TAIL_MESSAGES: usize = 12;

static MEMORY_FORK_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
static SESSION_COOLDOWNS: OnceLock<Mutex<std::collections::HashMap<String, std::time::Instant>>> =
    OnceLock::new();

/// Run the memory extraction fork. Best-effort — logs errors, never panics.
pub async fn run_memory_fork(input: ForkInput) {
    let session_key = input.session_owner.clone();
    if should_skip_for_cooldown(&session_key).await {
        tracing::debug!(session_owner = %session_key, "memory_fork: skipped by session cooldown");
        return;
    }

    let semaphore = MEMORY_FORK_SEMAPHORE.get_or_init(|| Semaphore::new(1));
    let permit = match semaphore.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::debug!(session_owner = %session_key, "memory_fork: skipped because another fork is running");
            return;
        }
    };

    tracing::info!(
        model = %input.model_id,
        msg_count = input.messages.len(),
        "memory_fork: starting"
    );

    let result = tokio::time::timeout(OVERALL_TIMEOUT, run_memory_fork_inner(input)).await;
    drop(permit);

    match result {
        Ok(Ok(written)) => {
            mark_session_cooldown(&session_key).await;
            if written > 0 {
                tracing::info!(files_written = written, "memory_fork: memories saved");
            } else {
                tracing::debug!("memory_fork: no memories saved");
            }
        }
        Ok(Err(e)) => {
            mark_session_cooldown(&session_key).await;
            tracing::warn!(err = %e, "memory_fork: failed");
        }
        Err(_) => {
            mark_session_cooldown(&session_key).await;
            tracing::warn!(
                timeout_secs = OVERALL_TIMEOUT.as_secs(),
                "memory_fork: timed out"
            );
        }
    }
}

async fn should_skip_for_cooldown(session_key: &str) -> bool {
    let cooldowns = SESSION_COOLDOWNS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let cooldowns = cooldowns.lock().await;
    cooldowns
        .get(session_key)
        .is_some_and(|last| last.elapsed() < SESSION_COOLDOWN)
}

async fn mark_session_cooldown(session_key: &str) {
    let cooldowns = SESSION_COOLDOWNS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut cooldowns = cooldowns.lock().await;
    cooldowns.insert(session_key.to_string(), std::time::Instant::now());
    cooldowns.retain(|_, last| last.elapsed() < SESSION_COOLDOWN * 2);
}

/// Compact fork context while preserving user-authored detail.
///
/// Profile-fidelity strategy (RFC P1): user messages are the primary carrier
/// of durable facts (preferences, habits, relationships, lifestyle). Older
/// logic kept only the last 12 messages, silently dropping every early user
/// statement. Now:
/// - all system messages are kept (prompt prefix, cache-friendly),
/// - every user message is kept, bounded by `MAX_FORK_USER_MESSAGES`,
/// - only the most recent assistant replies are kept,
/// - an explanatory note describes what was omitted.
fn compact_fork_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut system_msgs: Vec<ChatMessage> = Vec::new();
    let mut user_msgs: Vec<ChatMessage> = Vec::new();
    let mut assistant_msgs: Vec<ChatMessage> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => system_msgs.push(msg),
            "user" => user_msgs.push(msg),
            _ => assistant_msgs.push(msg),
        }
    }

    let omitted_assistant = assistant_msgs.len().saturating_sub(CONTEXT_TAIL_MESSAGES);
    let omitted_user = user_msgs.len().saturating_sub(MAX_FORK_USER_MESSAGES);

    let mut compacted = system_msgs;
    if omitted_user > 0 {
        compacted.push(ChatMessage::system_text(format!(
            "Memory fork context was trimmed: {omitted_assistant} assistant and {omitted_user} older user messages omitted. Extract only durable memories evidenced by the messages below; do not infer task progress from omitted history."
        )));
    } else if omitted_assistant > 0 {
        compacted.push(ChatMessage::system_text(format!(
            "Memory fork context was trimmed: {omitted_assistant} older assistant messages omitted; all user messages are preserved below. Extract only durable memories evidenced by the messages below; do not infer task progress from omitted history."
        )));
    }

    let user_start = user_msgs.len().saturating_sub(MAX_FORK_USER_MESSAGES);
    compacted.extend(user_msgs.into_iter().skip(user_start));
    let assistant_start = assistant_msgs.len().saturating_sub(CONTEXT_TAIL_MESSAGES);
    compacted.extend(assistant_msgs.into_iter().skip(assistant_start));

    sanitize_fork_context(compacted)
}

fn sanitize_fork_context(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    messages
        .into_iter()
        .filter_map(|mut msg| {
            if msg.role == "tool" {
                return None;
            }
            msg.tool_calls = None;
            msg.tool_call_id = None;
            msg.is_error = None;
            if msg.role == "assistant" && msg.text_content().trim().is_empty() {
                return None;
            }
            Some(msg)
        })
        .collect()
}

async fn run_memory_fork_inner(input: ForkInput) -> Result<usize> {
    // Build a minimal ToolContext for tool execution — memory tools need `owner`;
    // session_query needs the real `id` for provenance lookup.
    let session_shell = crate::api::tool::ToolContext {
        owner: input.session_owner.clone(),
        session_id: input.session_id.clone(),
        agent_name: "main".to_string(),
        ..Default::default()
    };

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

    // Assemble messages: compacted recent context + extraction prompt.
    let mut messages = compact_fork_messages(input.messages);
    let extraction_prompt = build_extraction_prompt(&input.memory_root, &input.session_id);
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
                    format!(
                        "tool '{}' not available during memory extraction",
                        call.name
                    ),
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
            let mut args = args;
            if call.name == "memory_manage" {
                if let Some(obj) = args.as_object_mut() {
                    obj.entry("model".to_string())
                        .or_insert_with(|| serde_json::Value::String(input.model_id.clone()));
                    // Fork memory always lands in the user's private layer. The
                    // agent layer is populated only by the idle distillation pass.
                    obj.insert(
                        "scope".to_string(),
                        serde_json::Value::String("user".to_string()),
                    );
                }
            }
            if call.name == "memory_manage" && args["action"].as_str() == Some("remove") {
                tracing::warn!("memory_fork: remove action blocked");
                let mut tool_msg = ChatMessage::text(
                    "tool",
                    "memory_manage(action='remove') is not available during background memory extraction",
                );
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(true);
                messages.push(tool_msg);
                continue;
            }

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
/// `CompactionEngine::collect_summary_stream` (no UI streaming).
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
                StreamEvent::ModelUsed { .. } => {} // informational; fork keeps its model_id
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
/// Independent from `compaction_engine::build_memory_prompt` — tuned for
/// turn-end extraction (not compaction), with its own taxonomy and rules.
fn build_extraction_prompt(memory_root: &str, session_id: &str) -> String {
    // Build existing memory index so the model avoids duplicates.
    let existing_index = if !memory_root.is_empty() {
        let memory_dir = std::path::Path::new(memory_root);
        let files = crate::memory::scan_memory_files(memory_dir);
        if files.is_empty() {
            String::from("(empty — no memories yet)")
        } else {
            let entries: Vec<crate::memory::IndexEntry> =
                files.iter().map(crate::memory::IndexEntry::from).collect();
            crate::memory::format_full_memory_index(&entries)
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
         Check this list before writing — use memory_search for likely duplicates and update an existing file rather than creating a duplicate.\n\
         \n\
         ## Durability gate (must pass before writing)\n\
         Write only if the fact is likely to remain useful after 7 days and across future sessions.\n\
         Before any add/replace, internally classify the candidate:\n\
         - durability: high|medium|low\n\
         - staleness_risk: low|medium|high\n\
         - action: add|replace|skip\n\
         Only call memory_manage when durability is high or medium AND staleness_risk is low or medium.\n\
         Skip task progress, transient runtime status, temporary TODOs, current commit/PID/log state, and one-off observations.\n\
         \n\
         ## User profile fidelity (CRITICAL)\n\
         User identity, preferences, habits, relationships, family, lifestyle, and explicitly stated stable facts are ALWAYS\n\
         high durability / low staleness — never gate them away, and never classify them as one-off observations.\n\
         \"one-off observations\" means transient events (a single bug fixed today, a one-time request) — NOT stable personal facts.\n\
         Record profile facts with FULL detail: keep names, times, places, numbers, exact wording, and specific context.\n\
         Do NOT abbreviate, summarize, generalize, or drop specifics when saving user-level memories.\n\
         \n\
         ## Replace without data loss\n\
         Before replacing an existing memory, call memory_view to read its full current content.\n\
         Write back ALL still-valid old details PLUS the new facts. Never drop old details unless the\n\
         user explicitly stated that the fact changed. A replace that only contains the new fact while\n\
         silently discarding prior details is data loss.\n\
         \n\
         ## Replace-first rule\n\
         Before creating a new memory, call memory_search with the key terms.\n\
         If an existing memory covers the same topic, use `memory_manage` action `replace` to merge the new durable fact.\n\
         Use `add` only when no existing memory covers the subject. Include a concise `reason` in memory_manage calls so audit logs explain why the memory changed.\n\
         Never call `remove` unless the user explicitly confirmed deletion in the main conversation.\n\
         \n\
         ## What to save\n\
         - User preferences, habits, and communication style\n\
         - Project decisions and their rationale\n\
         - Behavior corrections (things you did wrong that the user pointed out)\n\
         - Stable facts not derivable from code/git\n\
         \n\
         ## Attribution & Viewpoints (CRITICAL)\n\
         - NEVER attribute the Assistant's ideas, proposals, or analyses to the User.\n\
         - Only save a viewpoint or preference as the User's if the User EXPLICITLY stated it in their message.\n\
         - If the Assistant proposed a solution and the User merely agreed (e.g. \"ok\", \"looks good\"), save it as a project decision, NOT as a user preference or user viewpoint.\n\
         - Be extremely careful with pronoun and role mix-ups. \"I\" in a user message means the User; \"I\" in an assistant message means the Assistant.\n\
         \n\
         ## What NOT to save\n\
         - Task progress, completed-work logs, temporary TODO state\n\
         - Facts that will be stale in a week\n\
         - Code snippets, file contents, git diffs\n\
         \n\
         ## How to save\n\
         Use the `memory_manage` tool with action `add` (new) or `replace` (update). Do not use `remove` from this background fork.\n\
         All memory_manage calls are forced to `scope='user'` — extracted memories live in THIS user's\n\
         private layer. Do not judge whether a fact is cross-user generalizable; just record the durable\n\
         facts of this conversation. The shared agent layer is maintained by a separate distillation\n\
         process, not by you.\n\
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
         See Also (cross-links):\n\
         At the END of the content, add a `## See Also` section with markdown links to related memories.\n\
         Canonical format (always use this — do not omit the .md suffix on the href):\n\
         ## See Also\n\
         - [Related: other_memory_name](other_memory_name.md)\n\
         \n\
         Rules:\n\
         - Memory logical name has no .md (e.g. other_memory_name); the href MUST be that name plus .md\n\
         - Label: `[Related: <name>]` or a short description; target: `(<name>.md)` only — no bare name, no path prefix\n\
         - Link to 1-3 closely related memories from the existing index so the knowledge graph stays connected\n\
         \n\
         Other rules:\n\
         - Write memories as declarative facts, not instructions\n\
         - Only memories with `inject: 'always'` are injected into every conversation — regardless of type. Use `project`/`reference` with `inject: 'search'` for on-demand context\n\
         - If no memory changes are needed, respond with exactly: no memory changes needed\n\
         \n\
         You have a limited turn budget. Be efficient: decide what to write, then write it.\n\
         Available tools: memory_list, memory_view, memory_search, memory_manage, session_query.\n\
         \n\
         ## Provenance annotation\n\
         After writing each fact in memory content, add a provenance marker on its own line:\n\
         > 📌 provenance:`<session_id>#<msg_id>` <YYYY-MM-DD>\n\
         - `<session_id>`: the current session ID is `{session_id}`. For facts from earlier sessions, call session_query(action=\"list\") to find the ID.\n\
         - `<msg_id>`: global message ID. Call session_query(action=\"messages\", session_id=\"...\") to see message IDs. Can be a range like `42-58`.\n\
         - Date: today's date (YYYY-MM-DD).\n\
         Omit the marker for self-derived knowledge (e.g. your own analysis of tool output).",
        existing_index = existing_index,
        session_id = session_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_prompt_forces_user_scope() {
        let prompt = build_extraction_prompt("memory", "myclaw/s/test123");
        assert!(
            prompt.contains("scope='user'"),
            "fork prompt must force the user scope (agent layer is distillation-only)"
        );
        assert!(
            prompt.contains("private layer"),
            "fork prompt must describe the user layer as private"
        );
    }

    #[test]
    fn extraction_prompt_forces_profile_fidelity() {
        let prompt = build_extraction_prompt("memory", "myclaw/s/test123");
        assert!(
            prompt.contains("User profile fidelity"),
            "prompt must mandate profile fidelity for user-level memories"
        );
        assert!(
            prompt.contains("FULL detail"),
            "prompt must demand full detail (no summarizing away specifics)"
        );
        assert!(
            prompt.contains("Replace without data loss"),
            "prompt must forbid lossy replaces"
        );
        assert!(
            prompt.contains("memory_view"),
            "prompt must require reading the old content before replacing"
        );
        assert!(
            prompt.contains("high durability / low staleness"),
            "profile facts must be exempt from the durability gate"
        );
    }

    #[test]
    fn compact_fork_messages_preserves_all_user_messages() {
        let mut msgs: Vec<ChatMessage> = vec![ChatMessage::system_text("system prompt")];
        for i in 0..14 {
            msgs.push(ChatMessage::user_text(format!("user fact {}", i)));
            msgs.push(ChatMessage::assistant_text(format!("reply {}", i)));
        }

        let compacted = compact_fork_messages(msgs);
        let joined: String = compacted
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");

        // Every user message survives — early profile details must not be
        // dropped by context trimming (RFC P1).
        for i in 0..14 {
            assert!(
                joined.contains(&format!("user fact {}", i)),
                "user fact {} must be preserved",
                i
            );
        }
        assert!(joined.contains("system prompt"), "system messages must be preserved");
        assert!(joined.contains("reply 13"), "final assistant reply must be preserved");
        assert!(
            !joined.contains("reply 0"),
            "old assistant replies should be trimmed to bound tokens"
        );
        assert!(
            joined.contains("Memory fork context was trimmed"),
            "a trim note must explain what was omitted"
        );
    }

    #[test]
    fn compact_fork_messages_bounds_user_messages() {
        let mut msgs: Vec<ChatMessage> = vec![ChatMessage::system_text("system prompt")];
        for i in 0..60 {
            msgs.push(ChatMessage::user_text(format!("user fact {}", i)));
        }

        let compacted = compact_fork_messages(msgs);
        let user_count = compacted.iter().filter(|m| m.role == "user").count();
        assert!(
            user_count <= MAX_FORK_USER_MESSAGES,
            "user messages must be bounded (got {})",
            user_count
        );

        let joined: String = compacted
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("user fact 59"), "newest user messages must be kept");
        assert!(
            !joined.contains("user fact 0"),
            "oldest user messages beyond the bound may be dropped"
        );
        assert!(joined.contains("Memory fork context was trimmed"));
    }
}
