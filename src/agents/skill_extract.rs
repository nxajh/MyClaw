//! Turn-end skill extraction fork.
//!
//! Spawns a background mini-agent at the end of a substantive turn (>= 5 tool
//! calls, no errors). Reviews the conversation for reusable procedural
//! knowledge — multi-step workflows with concrete commands, pitfalls
//! discovered, non-obvious parameters. Extracted skills are written with
//! `status: draft` and filtered out of normal loading until reviewed.
//!
//! Fire-and-forget — the main turn does not wait.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::{Mutex, Semaphore};


use crate::agents::tool_registry::ToolRegistry;
use crate::channels::{Channel, ChannelOutboundMessage};
use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest, ToolSpec};
use crate::providers::capability_tool::ToolResult;
use crate::providers::{BoxStream, ChatUsage, StreamEvent, ToolCall};

/// Input bundle for skill extraction. All fields are owned/Arc so the struct
/// is `'static` and can move into `tokio::spawn`.
pub struct SkillExtractInput {
    /// System prompt + full history (clone of `messages` in `Agent::run`).
    pub messages: Vec<ChatMessage>,
    /// Model ID (same as the main turn — maximises cache hit).
    pub model_id: String,
    /// Chat provider (Arc clone, cheap).
    pub provider: Arc<dyn ChatProvider>,
    /// Full tool spec list (for prefix-cache key matching).
    pub tool_specs: Vec<ToolSpec>,
    /// Tool registry for actual execution.
    pub tool_registry: Arc<ToolRegistry>,
    /// Real session ID for provenance.
    pub session_id: String,
    /// Data dir root — skills actually live in `{base_dir}/skills/` (=
    /// `AppConfig::skills_root()`, where `skill_manage` writes), not under
    /// the agent workspace (issue #102: this field used to hold
    /// `workspace_dir`, so the dedup index below was always built from a
    /// directory nothing writes to).
    pub base_dir: String,
    /// Channel to notify on the session that just hosted this turn, so a
    /// newly-written draft doesn't accumulate silently (issue #89). `None`
    /// for headless/cron sessions or when no channel is wired.
    pub channel: Option<Arc<dyn Channel>>,
    /// Routing target for the notification (same as `Session::reply_target()`).
    pub reply_target: Option<String>,
}

/// Tools the skill extraction fork is permitted to call.
const ALLOWED: &[&str] = &["skills_list", "skill_manage"];

const MAX_ROUNDS: usize = 3;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(90);

static SKILL_EXTRACT_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
static EXTRACTED_SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Run one skill extraction pass. Fire-and-forget — caller spawns and
/// discards. Each session can extract at most once (one-shot guard).
pub async fn run_skill_extract(input: SkillExtractInput) {
    // One-shot per session.
    let extracted = EXTRACTED_SESSIONS.get_or_init(|| Mutex::new(HashSet::new()));
    {
        let mut guard = extracted.lock().await;
        if guard.contains(&input.session_id) {
            tracing::debug!(
                session_id = %input.session_id,
                "skill_extract: already ran for this session"
            );
            return;
        }
        guard.insert(input.session_id.clone());
        // Bound the set to prevent unbounded growth.
        if guard.len() > 200 {
            let to_remove: Vec<String> = guard.iter().take(50).cloned().collect();
            for id in to_remove {
                guard.remove(&id);
            }
        }
    }

    let semaphore = SKILL_EXTRACT_SEMAPHORE.get_or_init(|| Semaphore::new(1));
    let permit = match semaphore.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::debug!("skill_extract: skipped because another extraction is running");
            return;
        }
    };

    tracing::info!(
        model = %input.model_id,
        msg_count = input.messages.len(),
        "skill_extract: starting"
    );

    // Captured before `input` moves into `run_skill_extract_inner` below —
    // needed afterward to notify the session's channel about any drafts
    // written (issue #89).
    let channel = input.channel.clone();
    let reply_target = input.reply_target.clone();

    let result =
        tokio::time::timeout(OVERALL_TIMEOUT, run_skill_extract_inner(input)).await;
    drop(permit);

    match result {
        Ok(Ok(written)) => {
            if !written.is_empty() {
                tracing::info!(
                    skills_written = written.len(),
                    names = ?written,
                    "skill_extract: skills created"
                );
                notify_drafts_written(channel, reply_target, &written).await;
            } else {
                tracing::debug!("skill_extract: no skills extracted");
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(err = %e, "skill_extract: failed");
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = OVERALL_TIMEOUT.as_secs(),
                "skill_extract: timed out"
            );
        }
    }
}

/// Tell the session's channel about newly-written drafts, so they don't
/// accumulate silently (issue #89, layer ①). Best-effort: no channel wired
/// (headless/cron session) or a send failure just gets logged.
async fn notify_drafts_written(
    channel: Option<Arc<dyn Channel>>,
    reply_target: Option<String>,
    names: &[String],
) {
    let (Some(channel), Some(target)) = (channel, reply_target) else {
        tracing::debug!("skill_extract: no channel to notify about drafts written");
        return;
    };
    let quoted = names
        .iter()
        .map(|n| format!("「{n}」"))
        .collect::<Vec<_>>()
        .join("、");
    let text = format!(
        "本次会话沉淀了 {} 个 draft skill:{}，待审核。可以让我查看内容并提议保留/合并/删除。",
        names.len(),
        quoted
    );
    if let Err(e) = channel
        .send_message(&ChannelOutboundMessage::text(target, text))
        .await
    {
        tracing::warn!(err = %e, "skill_extract: failed to send draft-skill notification");
    }
}

async fn run_skill_extract_inner(input: SkillExtractInput) -> Result<Vec<String>> {
    // Build a minimal ToolContext for tool execution.
    let session_shell = crate::api::tool::ToolContext {
        owner: "skill_extract".to_string(),
        session_id: input.session_id.clone(),
        reply_target: None,
        last_message: None,
        parent_session_id: None,
        agent_name: "main".to_string(),
        turn_silenced: false,
        turn_headless: false,
        channel: None,
    };

    // Build existing skills index for dedup.
    let existing_index = build_existing_skills_index(&input.base_dir);

    // Assemble messages: conversation context + extraction prompt.
    let mut messages = input.messages;
    let prompt = build_skill_extract_prompt(&existing_index, &input.session_id);
    messages.push(ChatMessage::user_text(prompt));

    let provider = input.provider;
    let model_id = &input.model_id;
    let tool_specs = &input.tool_specs;
    let tool_registry = &input.tool_registry;
    let mut skills_written: Vec<String> = Vec::new();

    for round in 1..=MAX_ROUNDS {
        let req = ChatRequest {
            model: model_id,
            messages: &messages,
            temperature: None,
            max_tokens: None,
            thinking: None,
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
        let response = collect_extract_stream(stream).await?;

        if response.tool_calls.is_empty() {
            break;
        }

        tracing::info!(
            round,
            tool_calls = response.tool_calls.len(),
            "skill_extract: executing tool calls"
        );

        let mut assistant_msg = ChatMessage::assistant_text(&response.text);
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        messages.push(assistant_msg);

        for call in &response.tool_calls {
            if !ALLOWED.contains(&call.name.as_str()) {
                tracing::warn!(tool = %call.name, "skill_extract: tool not allowed, blocking");
                let mut tool_msg = ChatMessage::text(
                    "tool",
                    format!(
                        "tool '{}' not available during skill extraction",
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

            // Block non-create actions on skill_manage.
            if call.name == "skill_manage"
                && args["action"].as_str().is_some_and(|a| a != "create")
            {
                tracing::warn!(
                    action = ?args["action"],
                    "skill_extract: non-create skill_manage action blocked"
                );
                let mut tool_msg = ChatMessage::text(
                    "tool",
                    "Only skill_manage(action='create') is available during skill extraction.",
                );
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(true);
                messages.push(tool_msg);
                continue;
            }

            let created_name = args["name"].as_str().map(str::to_string);
            let raw = tool.execute(args, &session_shell).await;
            let result: anyhow::Result<ToolResult> = raw;
            let (result_content, is_error) = match &result {
                Ok(r) => {
                    if r.success && call.name == "skill_manage" {
                        if let Some(name) = created_name {
                            skills_written.push(name);
                        }
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

    Ok(skills_written)
}

/// Build a compact index of existing skills for the prompt.
fn build_existing_skills_index(base_dir: &str) -> String {
    let skills_dir = std::path::Path::new(base_dir).join("skills");
    let definitions =
        crate::agents::skill_loader::load_skills_from_dir(&skills_dir);
    if definitions.is_empty() {
        return "(empty — no skills yet)".to_string();
    }
    definitions
        .iter()
        .map(|d| {
            format!(
                "- {}: {}",
                d.name,
                d.description.chars().take(120).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_skill_extract_prompt(existing_index: &str, session_id: &str) -> String {
    format!(
        "\n\n---\n\
         You are now acting as the skill extraction subagent. Analyze the conversation above \
         and decide if any reusable procedural knowledge (a step-by-step HOW-TO with concrete \
         commands) should be saved as a skill.\n\
         \n\
         ## Existing skills\n\
         {existing_index}\n\
         \n\
         Check this list before creating — do NOT duplicate an existing skill. If a similar \
         skill exists, skip.\n\
         \n\
         ## Three-part admission gate (ALL must pass)\n\
         Before creating any skill, the conversation must satisfy all three:\n\
         \n\
         1. REUSABILITY: The same type of request will recur in future sessions. \
         Exclude one-off tasks (fixing a specific commit, restarting a specific service).\n\
         2. PROCEDURAL DEPTH: The solution involved 3+ distinct steps with concrete \
         commands, parameters, or tool sequences. A single-command solution is knowledge, \
         not a procedure.\n\
         3. NON-TRIVIAL DISCOVERY: At least one of:\n\
            - A pitfall/correction (something that did NOT work and why)\n\
            - A non-obvious parameter or configuration value\n\
            - A required operation ordering that is not documented\n\
            - An easy-to-miss prerequisite condition\n\
         \n\
         If any gate fails, respond with exactly: no skill needed\n\
         \n\
         ## Output rules\n\
         - Create at most 1 skill. Quality over quantity.\n\
         - Use `skill_manage` action `create` with a `name` (kebab-case directory name).\n\
         - The `content` parameter is the FULL SKILL.md including YAML frontmatter.\n\
         - Frontmatter MUST include `metadata.status: draft` — draft skills are hidden from \
         normal loading until a human reviews and activates them.\n\
         \n\
         ## SKILL.md content structure\n\
         ```yaml\n\
         ---\n\
         name: your-skill-name\n\
         description: \"One-line description with trigger conditions and keywords (中英文)\"\n\
         metadata:\n\
         \x20\x20keywords: [keyword1, keyword2]\n\
         \x20\x20status: draft\n\
         ---\n\
         ```\n\
         (`name`/`description` are the only standard top-level fields — everything else goes \
         under `metadata:`, matching how existing skills are structured.)\n\
         Then the body:\n\
         \n\
         # Skill Title\n\
         One sentence on what this does.\n\
         \n\
         ## Prerequisites\n\
         - Required environment, dependencies, or conditions\n\
         \n\
         ## Steps\n\
         1. Exact command with real parameters (not placeholders)\n\
         2. Exact command\n\
         3. ...\n\
         \n\
         ## Pitfalls\n\
         - What NOT to do and why (extracted from errors/corrections in the conversation)\n\
         \n\
         ## Verification\n\
         - How to confirm the operation succeeded\n\
         \n\
         ## Rules\n\
         - Commands MUST be real — actual commands that ran successfully in this conversation.\n\
         - The Pitfalls section is REQUIRED when the conversation involved any error or correction.\n\
         - The description must include trigger keywords that would cause this skill to activate.\n\
         - Add provenance at the bottom:\n\
         > Extracted from session `{session_id}` on {today}\n\
         \n\
         If no skill is warranted, respond with exactly: no skill needed\n\
         \n\
         You have a limited turn budget. Be efficient.\n\
         Available tools: skills_list, skill_manage.",
        existing_index = existing_index,
        session_id = session_id,
        today = chrono::Utc::now().format("%Y-%m-%d"),
    )
}

/// Stream collector — mirrors the fork collector.
struct ExtractResponse {
    text: String,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    usage: Option<ChatUsage>,
}

async fn collect_extract_stream(mut stream: BoxStream<StreamEvent>) -> Result<ExtractResponse> {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage: Option<ChatUsage> = None;
    let chunk_timeout = crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT;

    loop {
        match tokio::time::timeout(chunk_timeout, stream.next()).await {
            Ok(Some(event)) => match event {
                StreamEvent::Delta { text: delta } => text.push_str(&delta),
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
                    if !id.is_empty() {
                        tool_calls[idx].id = id;
                    }
                    if !name.is_empty() {
                        tool_calls[idx].name = name;
                    }
                    tool_calls[idx].arguments.push_str(&delta);
                }
                StreamEvent::Usage(u) => usage = Some(u),
                _ => {}
            },
            Ok(None) => break,
            Err(_) => {
                tracing::warn!("skill_extract: stream chunk timeout");
                break;
            }
        }
    }

    Ok(ExtractResponse {
        text,
        tool_calls,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockChannel {
        sent: Mutex<Vec<crate::channels::ChannelOutboundMessage>>,
    }

    #[async_trait::async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "mock"
        }
        async fn send_message(
            &self,
            msg: &ChannelOutboundMessage,
        ) -> anyhow::Result<crate::channels::OutboundSendResult> {
            self.sent.lock().unwrap().push(msg.clone());
            Ok(crate::channels::OutboundSendResult::empty())
        }
        async fn listen(
            &self,
        ) -> anyhow::Result<tokio::sync::mpsc::Receiver<crate::channels::ChannelInboundMessage>>
        {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn notify_drafts_written_is_noop_without_channel() {
        // Headless/cron session: no channel wired, no reply target. Must not
        // panic and must not attempt to send anything.
        notify_drafts_written(None, None, &["my-skill".to_string()]).await;
    }

    #[tokio::test]
    async fn notify_drafts_written_sends_one_message_naming_all_drafts() {
        let mock = Arc::new(MockChannel {
            sent: Mutex::new(Vec::new()),
        });
        let channel: Arc<dyn Channel> = mock.clone();
        notify_drafts_written(
            Some(channel),
            Some("chat:123".to_string()),
            &["skill-one".to_string(), "skill-two".to_string()],
        )
        .await;

        let sent = mock.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "expected exactly one notification sent");
        assert_eq!(sent[0].receiver.id, "chat:123");
        assert!(sent[0].content.text.contains("skill-one"));
        assert!(sent[0].content.text.contains("skill-two"));
    }
}
