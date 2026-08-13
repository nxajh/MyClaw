//! Idle-time agent-level memory distillation.
//!
//! When the system is idle and per-user memories have changed, scan all
//! `users/*/memory/*.md` files, ask the LLM to extract cross-user
//! generalizable knowledge (methodology / processes / rules / reusable
//! experience), and persist it to the shared agent layer
//! (`memory/`) via `memory_manage(scope='agent')`.
//!
//! Design intent (RFC docs/rfc-two-tier-memory.md): the fork layer never
//! judges generality — distillation is the only writer to the agent layer.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::Semaphore;

use crate::agents::session::Session;
use crate::agents::tool_registry::ToolRegistry;
use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest, ToolSpec};
use crate::providers::capability_tool::ToolResult;
use crate::providers::{
    BoxStream, ChatUsage, ProviderRegistry, StreamEvent, ThinkingConfig, ToolCall,
};

/// Tools the distillation pass is permitted to call. Mirrors fork ALLOWED.
const ALLOWED: &[&str] = &[
    "memory_list",
    "memory_view",
    "memory_search",
    "memory_manage",
];

const MAX_ROUNDS: usize = 5;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_INPUT_CHARS: usize = 60_000;

static DISTILL_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

/// Input bundle for the distillation pass.
///
/// Owned/`Arc` so it can move into `tokio::spawn` / `timeout`.
pub struct DistillInput {
    /// Model ID (same family as the main agent — prefix-cache friendly).
    pub model_id: String,
    /// Chat provider (Arc clone, cheap).
    pub provider: Arc<dyn ChatProvider>,
    /// Full tool spec list (for prefix-cache key matching).
    pub tool_specs: Vec<ToolSpec>,
    /// Tool registry for actual execution.
    pub tool_registry: Arc<ToolRegistry>,
    /// Workspace root — user memory files are scanned under `users/`.
    pub workspace_dir: String,
    /// Provider registry for resolving model config (reasoning flag).
    pub registry: Arc<dyn ProviderRegistry>,
}

/// Run one distillation pass. Returns the number of agent-layer memory files
/// written on success; `Err` signals a failed pass (caller should NOT advance
/// `last_distill_ts` — the new user memories stay pending for retry).
/// Concurrent distillation passes are prevented by a global semaphore.
pub async fn run_memory_distill(input: DistillInput) -> Result<usize> {
    let semaphore = DISTILL_SEMAPHORE.get_or_init(|| Semaphore::new(1));
    let permit = match semaphore.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::debug!("memory_distill: skipped because another pass is running");
            return Ok(0);
        }
    };

    tracing::info!(model = %input.model_id, "memory_distill: starting");

    let result = tokio::time::timeout(OVERALL_TIMEOUT, run_memory_distill_inner(input)).await;
    drop(permit);

    match result {
        Ok(Ok(written)) => {
            tracing::info!(files_written = written, "memory_distill: finished");
            Ok(written)
        }
        Ok(Err(e)) => {
            tracing::warn!(err = %e, "memory_distill: failed");
            Err(e)
        }
        Err(_) => {
            let msg = format!("memory_distill timed out after {}s", OVERALL_TIMEOUT.as_secs());
            tracing::warn!(timeout_secs = OVERALL_TIMEOUT.as_secs(), "memory_distill: timed out");
            Err(anyhow::anyhow!(msg))
        }
    }
}

// ── Distillation state (`.state/distill.json`) ─────────────────────────────

/// Persistent distillation progress state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DistillState {
    /// RFC3339 timestamp of the last successful distillation pass.
    pub last_distill_ts: Option<String>,
    /// RFC3339 timestamp of the last attempted pass (success or failure).
    pub last_attempt_ts: Option<String>,
    /// Consecutive failures — after 3, back off for 2 hours.
    pub consecutive_failures: u32,
}

impl DistillState {
    fn path(workspace_dir: &str) -> std::path::PathBuf {
        std::path::Path::new(workspace_dir).join(".state").join("distill.json")
    }

    /// Load state from disk; returns defaults when missing or unparsable.
    pub fn load(workspace_dir: &str) -> DistillState {
        let path = Self::path(workspace_dir);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => DistillState::default(),
        }
    }

    /// Save state atomically.
    pub fn save(&self, workspace_dir: &str) {
        let path = Self::path(workspace_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(content) => {
                if let Err(e) = std::fs::write(&path, content) {
                    tracing::warn!(err = %e, path = %path.display(), "memory_distill: failed to save state");
                }
            }
            Err(e) => tracing::warn!(err = %e, "memory_distill: failed to serialize state"),
        }
    }

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    /// Whether a pending-distillation check may run given backoff state.
    pub fn in_backoff(&self) -> bool {
        if self.consecutive_failures < 3 {
            return false;
        }
        let Some(last) = self.last_attempt_ts.as_deref() else {
            return false;
        };
        chrono::DateTime::parse_from_rfc3339(last)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .is_some_and(|dt| {
                chrono::Utc::now().signed_duration_since(dt) < chrono::Duration::hours(2)
            })
    }

    /// Mark an attempt (success or failure) and persist.
    pub fn record_attempt(&mut self, success: bool, workspace_dir: &str) {
        self.last_attempt_ts = Some(Self::now_rfc3339());
        if success {
            self.last_distill_ts = Some(Self::now_rfc3339());
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        }
        self.save(workspace_dir);
    }
}

/// Whether any per-user memory file was modified after `last_distill_ts`.
/// A `None` last-distill timestamp means "never distilled" → pending.
pub fn has_pending_user_memories(workspace_dir: &str, last_distill_ts: Option<&str>) -> bool {
    let last_distill = last_distill_ts
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let users_root = std::path::Path::new(workspace_dir).join("users");
    let entries = match std::fs::read_dir(&users_root) {
        Ok(rd) => rd,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let user_dir = entry.path();
        if !user_dir.is_dir() {
            continue;
        }
        let memory_dir = user_dir.join(crate::memory::MEMORY_DIR_NAME);
        let Ok(rd) = std::fs::read_dir(&memory_dir) else {
            continue;
        };
        for file in rd.flatten() {
            let path = file.path();
            if path.extension() != Some(std::ffi::OsStr::new("md")) {
                continue;
            }
            let Ok(meta) = file.metadata() else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let mtime: chrono::DateTime<chrono::Utc> = mtime.into();
            match last_distill {
                None => return true,
                Some(last) if mtime > last => return true,
                Some(_) => {}
            }
        }
    }
    false
}

/// Collect per-user memory files into an anonymized input document.
/// Returns (user_count, file_count, document).
fn collect_user_memories(workspace_dir: &str) -> (usize, usize, String) {
    let users_root = std::path::Path::new(workspace_dir).join("users");
    let mut batches: Vec<(String, String)> = Vec::new();
    let mut total_files = 0usize;
    let mut total_chars = 0usize;
    let mut user_idx = 0usize;

    let entries = match std::fs::read_dir(&users_root) {
        Ok(rd) => rd,
        Err(_) => return (0, 0, String::new()),
    };

    for entry in entries.flatten() {
        let user_dir = entry.path();
        if !user_dir.is_dir() {
            continue;
        }
        let memory_dir = user_dir.join(crate::memory::MEMORY_DIR_NAME);
        let files = crate::memory::scan_memory_files(&memory_dir);
        if files.is_empty() {
            continue;
        }
        user_idx += 1;
        let label = format!("User-{}", user_idx);
        let mut doc = String::new();
        for f in &files {
            // Guard against pathological sizes; per-file body is bounded by
            // MAX_CONTENT_CHARS on write, but legacy files may differ.
            let body = f.content.chars().take(4_000).collect::<String>();
            doc.push_str(&format!(
                "### memory: {}\n{}\n\n",
                f.name, body
            ));
            total_files += 1;
        }
        if doc.is_empty() {
            continue;
        }
        let batch = format!("<memory_batch user=\"{}\">\n{}\n</memory_batch>\n", label, doc);
        if total_chars + batch.len() > MAX_INPUT_CHARS {
            tracing::warn!(
                "memory_distill: input truncated at {} chars",
                MAX_INPUT_CHARS
            );
            break;
        }
        total_chars += batch.len();
        batches.push((label, batch));
    }

    let doc = batches.into_iter().map(|(_, b)| b).collect::<String>();
    (user_idx, total_files, doc)
}

fn name_tokens(name: &str) -> std::collections::HashSet<String> {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// Find an existing agent-layer memory whose name is a near-duplicate of the
/// candidate (Jaccard >= 0.4 with >= 3 shared tokens). Returns the existing
/// memory name. Runtime backstop for the prompt rule: distill must merge into
/// an existing memory instead of adding a duplicate topic.
fn find_duplicate_agent_memory(workspace_dir: &str, candidate_name: &str) -> Option<String> {
    let memory_dir = std::path::Path::new(workspace_dir).join(crate::memory::MEMORY_DIR_NAME);
    let files = crate::memory::scan_memory_files(&memory_dir);
    let cand = name_tokens(candidate_name);
    if cand.is_empty() {
        return None;
    }
    files
        .iter()
        .filter_map(|f| {
            let existing = name_tokens(&f.name);
            if existing.is_empty() {
                return None;
            }
            let inter = cand.intersection(&existing).count();
            let union = cand.union(&existing).count();
            if union == 0 {
                return None;
            }
            let jaccard = inter as f64 / union as f64;
            if inter >= 3 && jaccard >= 0.4 {
                Some((jaccard, f.name.clone()))
            } else {
                None
            }
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
        .map(|(_, name)| name)
}

/// Build an index of existing agent-level memories (`workspace/memory/`) for
/// prompt injection — mirrors `memory_fork::build_extraction_prompt` so the
/// model can spot covered topics without guessing.
fn build_existing_agent_index(workspace_dir: &str) -> String {
    let memory_dir = std::path::Path::new(workspace_dir).join(crate::memory::MEMORY_DIR_NAME);
    let files = crate::memory::scan_memory_files(&memory_dir);
    if files.is_empty() {
        return "(empty — no agent-level memories yet)".to_string();
    }
    let entries: Vec<crate::memory::IndexEntry> =
        files.iter().map(crate::memory::IndexEntry::from).collect();
    crate::memory::format_full_memory_index(&entries)
}

/// Snapshot agent-layer memory names and their content hashes.
/// Used to diff before/after pass 1 to find newly written/modified memories.
fn snapshot_agent_memories(workspace_dir: &str) -> std::collections::HashMap<String, String> {
    let memory_dir = std::path::Path::new(workspace_dir).join(crate::memory::MEMORY_DIR_NAME);
    let files = crate::memory::scan_memory_files(&memory_dir);
    files
        .iter()
        .map(|f| {
            let hash = std::fs::read_to_string(&f.path)
                .map(|c| {
                    crate::providers::capability_chat::sha256_hex(c.as_bytes())
                        .chars()
                        .take(16)
                        .collect::<String>()
                })
                .unwrap_or_default();
            (f.name.clone(), hash)
        })
        .collect()
}

/// Diff two snapshots and return the (name, content) pairs for new or modified
/// agent memories. Reads current file content from disk.
fn diff_agent_memories(
    before: &std::collections::HashMap<String, String>,
    after: &std::collections::HashMap<String, String>,
    workspace_dir: &str,
) -> Vec<(String, String)> {
    let memory_dir = std::path::Path::new(workspace_dir).join(crate::memory::MEMORY_DIR_NAME);
    let mut result = Vec::new();

    for (name, after_hash) in after {
        let is_new_or_modified = match before.get(name) {
            None => true, // newly created
            Some(before_hash) => before_hash != after_hash, // modified
        };
        if is_new_or_modified {
            if let Ok(content) = std::fs::read_to_string(memory_dir.join(format!("{}.md", name))) {
                // Take up to 2000 chars of content for the pass-2 prompt.
                let body: String = content.chars().take(2_000).collect();
                result.push((name.clone(), body));
            }
        }
    }
    result
}

fn build_distill_prompt(
    user_count: usize,
    file_count: usize,
    input_doc: &str,
    existing_index: &str,
) -> String {
    format!(
        "\n\n---\n\
         You are the agent-level memory distillation subagent. Below are memory files \
         extracted from conversations with {user_count} different users ({file_count} files total).\n\
         Your job: find knowledge that is cross-user generalizable and persist it to the \
         shared agent-level memory layer.\n\
         \n\
         ## Input (anonymized user memories)\n\
         {input_doc}\n\
         \n\
         ## Existing agent-level memories\n\
         {existing_index}\n\
         \n\
         ## What qualifies\n\
         - Methodology, processes, workflows, and rules that apply beyond a single user\n\
         - Reusable technical experience, debugging patterns, and operational procedures\n\
         - General principles and design guidance (not user-specific preferences)\n\
         \n\
         ## What does NOT qualify\n\
         - Single-user private facts, preferences, or history\n\
         - One-off events, task progress, or transient status\n\
         - Anything tied to a specific user's identity\n\
         \n\
         ## Rules\n\
         1. The index above lists ALL existing agent-level memories. Before any add, check the \
         candidate against it. If the topic is already covered (same or overlapping topic), use \
         `memory_manage` action `replace` to merge into the most relevant existing memory — \
         never `add` a new memory for a covered topic. Use `add` only for genuinely new topics.\n\
         2. All memory_manage calls are forced to `scope='agent'` by the runtime — do not omit it.\n\
         3. De-identification is CRITICAL: the output must NOT contain routing keys, user ids, \
         emails, phone numbers, real names, or organization names. Use generic terms \
         (\"a user's host\", \"the project\") instead. The static guard will reject violations.\n\
         4. Write memories as declarative facts, not instructions.\n\
         5. Only memories with `inject: 'always'` are auto-injected; use `inject: 'search'` for \
         on-demand context unless the knowledge must always apply.\n\
         6. End content with `## See Also` links to closely related agent-level memories when \
         applicable, canonical form: `[Related: other_memory_name](other_memory_name.md)`.\n\
         7. Never call `remove`.\n\
         8. In this pass, write ONLY `type='reference'` or `type='project'` memories — \
         facts, scenarios, and reusable experience. Do NOT create `type='rule'` \
         memories; rules are synthesized in a separate second pass.\n\
         9. Every memory you write MUST end with a `## Evidence` section listing \
         the source user-memory names from the input above that informed it:\n\
         ## Evidence\n\
         - Distilled from: memory_name_1, memory_name_2\n\
         Use the logical names (without .md) of the user memories. This enables \
         traceability back to the source material.\n\
         \n\
         If nothing is cross-user generalizable, respond with exactly: no distillation needed\n\
         \n\
         You have a limited turn budget. Be efficient: decide what to write, then write it.\n\
         Available tools: memory_list, memory_view, memory_search, memory_manage.",
        user_count = user_count,
        file_count = file_count,
        input_doc = input_doc,
        existing_index = existing_index,
    )
}

/// Build the second-pass prompt for synthesizing behavioral rules from newly
/// written agent-level memories (output of pass 1).
fn build_rule_synthesis_prompt(
    new_memories_doc: &str,
    existing_index: &str,
) -> String {
    format!(
        "\n\n---\n\
         You are the agent-level rule synthesis subagent (pass 2). Below are \
         newly written agent-level memories from the first distillation pass.\n\
         Your job: synthesize cross-user behavioral rules from these facts and \
         experiences, and persist them as `type='rule'` agent-level memories.\n\
         \n\
         ## Newly extracted agent memories (pass 1 output)\n\
         {new_memories_doc}\n\
         \n\
         ## Existing agent-level memories\n\
         {existing_index}\n\
         \n\
         ## What qualifies for rule synthesis\n\
         - Behavioral rules that generalize from multiple facts or recurring patterns\n\
         - Operational procedures or constraints validated through experience\n\
         - \"What NOT to do and why\" — anti-patterns discovered through failures\n\
         - Defaults and conventions worth enforcing across all future sessions\n\
         \n\
         ## What does NOT qualify\n\
         - Restating facts already captured in pass 1 — rules are abstractions ABOVE facts\n\
         - Single-occurrence observations without generalizable lesson\n\
         - Anything that would duplicate an existing agent memory (check the index)\n\
         \n\
         ## Rules\n\
         1. Check the index above before writing. If an existing rule covers the same \
         ground, use `memory_manage` action `replace` to merge — never duplicate.\n\
         2. All memory_manage calls are forced to `scope='agent'` by the runtime.\n\
         3. De-identification is CRITICAL: no routing keys, user ids, emails, phone \
         numbers, real names, or organization names.\n\
         4. Write memories as declarative facts, not instructions.\n\
         5. Use `memory_type='rule'` for all memories in this pass.\n\
         6. Every memory MUST end with a `## Evidence` section listing the agent-level \
         memory names from pass 1 that informed this rule:\n\
         ## Evidence\n\
         - Synthesized from: agent_memory_name_1, agent_memory_name_2\n\
         7. End content with `## See Also` links when applicable.\n\
         8. Never call `remove`.\n\
         \n\
         If no rules can be synthesized, respond with exactly: no rules needed\n\
         \n\
         You have a limited turn budget. Be efficient: decide what to write, then write it.\n\
         Available tools: memory_list, memory_view, memory_search, memory_manage.",
        new_memories_doc = new_memories_doc,
        existing_index = existing_index,
    )
}

async fn run_memory_distill_inner(input: DistillInput) -> Result<usize> {
    let (user_count, file_count, input_doc) = collect_user_memories(&input.workspace_dir);
    if user_count == 0 {
        tracing::debug!("memory_distill: no user memories to distill");
        return Ok(0);
    }

    // Build a minimal Session shell — memory tools only need `owner`.
    let mut session_shell = Session::new("memory_distill".to_string());
    session_shell.owner = "memory_distill".to_string();

    // Resolve thinking config from model config.
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

    let existing_index = build_existing_agent_index(&input.workspace_dir);

    // Snapshot agent memory state before pass 1 so we can diff after.
    let before = snapshot_agent_memories(&input.workspace_dir);

    // ── Pass 1: extract facts/scenarios/reference from user memories ──
    tracing::info!("memory_distill: pass 1 (facts/scenarios)");
    let distill_prompt = build_distill_prompt(user_count, file_count, &input_doc, &existing_index);
    let messages1 = vec![ChatMessage::user_text(distill_prompt)];
    let pass1_written = run_distill_rounds(
        messages1,
        Arc::clone(&input.provider),
        &input.model_id,
        &input.tool_specs,
        Arc::clone(&input.tool_registry),
        &session_shell,
        &thinking,
        &input.workspace_dir,
    )
    .await?;

    let mut total_written = pass1_written;

    // ── Pass 2: synthesize rules from newly written agent memories ──
    let after = snapshot_agent_memories(&input.workspace_dir);
    let new_memories = diff_agent_memories(&before, &after, &input.workspace_dir);

    if !new_memories.is_empty() {
        tracing::info!(
            new_count = new_memories.len(),
            "memory_distill: pass 2 (rule synthesis)"
        );
        let new_memories_doc = new_memories
            .iter()
            .map(|(name, body)| format!("### memory: {}\n{}\n\n", name, body))
            .collect::<String>();

        // Refresh index to include pass-1 writes.
        let updated_index = build_existing_agent_index(&input.workspace_dir);
        let rule_prompt = build_rule_synthesis_prompt(&new_memories_doc, &updated_index);
        let messages2 = vec![ChatMessage::user_text(rule_prompt)];
        let pass2_written = run_distill_rounds(
            messages2,
            Arc::clone(&input.provider),
            &input.model_id,
            &input.tool_specs,
            Arc::clone(&input.tool_registry),
            &session_shell,
            &thinking,
            &input.workspace_dir,
        )
        .await?;
        total_written += pass2_written;
    } else {
        tracing::debug!("memory_distill: pass 2 skipped (no new agent memories)");
    }

    Ok(total_written)
}

/// Run the distillation tool-calling loop for up to MAX_ROUNDS.
/// Returns the number of successful memory_manage writes.
#[allow(clippy::too_many_arguments)]
async fn run_distill_rounds(
    mut messages: Vec<ChatMessage>,
    provider: Arc<dyn ChatProvider>,
    model_id: &str,
    tool_specs: &[ToolSpec],
    tool_registry: Arc<ToolRegistry>,
    session_shell: &Session,
    thinking: &Option<ThinkingConfig>,
    workspace_dir: &str,
) -> Result<usize> {
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
        let response = collect_distill_stream(stream).await?;

        if response.tool_calls.is_empty() {
            break;
        }

        tracing::info!(
            round,
            tool_calls = response.tool_calls.len(),
            "memory_distill: executing tool calls"
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
                tracing::warn!(tool = %call.name, "memory_distill: tool not allowed, blocking");
                let mut tool_msg = ChatMessage::text(
                    "tool",
                    format!(
                        "tool '{}' not available during memory distillation",
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
                        .or_insert_with(|| serde_json::Value::String(model_id.to_string()));
                    // Distillation is the ONLY writer to the agent layer.
                    obj.insert(
                        "scope".to_string(),
                        serde_json::Value::String("agent".to_string()),
                    );
                    // Dedup backstop: block `add` when a near-duplicate agent
                    // memory already exists — steer the model to `replace`.
                    if obj.get("action").and_then(|a| a.as_str()) == Some("add") {
                        if let Some(name) = obj.get("name").and_then(|n| n.as_str()) {
                            if let Some(existing) =
                                find_duplicate_agent_memory(workspace_dir, name)
                            {
                                tracing::warn!(
                                    candidate = name,
                                    existing,
                                    "memory_distill: duplicate add blocked"
                                );
                                let mut tool_msg = ChatMessage::text(
                                    "tool",
                                    format!(
                                        "add blocked: agent memory '{existing}' already covers this \
                                         topic (near-duplicate name). Use memory_manage action='replace' \
                                         to merge into '{existing}' instead of adding a duplicate."
                                    ),
                                );
                                tool_msg.tool_call_id = Some(call.id.clone());
                                tool_msg.is_error = Some(true);
                                messages.push(tool_msg);
                                continue;
                            }
                        }
                    }
                }
            }
            if call.name == "memory_manage" && args["action"].as_str() == Some("remove") {
                tracing::warn!("memory_distill: remove action blocked");
                let mut tool_msg = ChatMessage::text(
                    "tool",
                    "memory_manage(action='remove') is not available during memory distillation",
                );
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(true);
                messages.push(tool_msg);
                continue;
            }

            let raw = tool.execute(args, session_shell).await;
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

/// Stream collector — mirrors the fork collector (no UI streaming).
struct DistillResponse {
    text: String,
    reasoning_content: Option<String>,
    thinking_signature: Option<String>,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    usage: Option<ChatUsage>,
}

async fn collect_distill_stream(mut stream: BoxStream<StreamEvent>) -> Result<DistillResponse> {
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
                    anyhow::bail!("memory_distill stream error: {}", message)
                }
                StreamEvent::Error(e) => anyhow::bail!("memory_distill stream error: {}", e),
                StreamEvent::ModelUsed { .. } => {} // informational; distill keeps its model_id
            },
            Ok(None) => {
                tracing::warn!("memory_distill stream ended without Done event");
                break;
            }
            Err(_) => anyhow::bail!(
                "memory_distill stream chunk timeout after {}s",
                chunk_timeout.as_secs()
            ),
        }
    }

    Ok(DistillResponse {
        text,
        reasoning_content,
        thinking_signature,
        tool_calls,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distill_state_backoff_after_three_failures_and_reset_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();

        let mut state = DistillState::default();
        assert!(!state.in_backoff());

        state.record_attempt(false, ws);
        state.record_attempt(false, ws);
        state.record_attempt(false, ws);
        assert_eq!(state.consecutive_failures, 3);
        assert!(state.in_backoff(), "3 consecutive failures must enter backoff");
        assert!(state.last_distill_ts.is_none(), "failed passes must not advance last_distill_ts");

        state.record_attempt(true, ws);
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.in_backoff());
        assert!(state.last_distill_ts.is_some(), "success must advance last_distill_ts");
    }

    #[test]
    fn distill_state_backoff_expires_after_two_hours() {
        let mut state = DistillState {
            consecutive_failures: 3,
            last_attempt_ts: Some(
                (chrono::Utc::now() - chrono::Duration::hours(3)).to_rfc3339(),
            ),
            last_distill_ts: None,
        };
        assert!(
            !state.in_backoff(),
            "backoff must expire once 2h has passed since the last attempt"
        );

        // Fresh attempt → still in backoff.
        state.last_attempt_ts = Some(chrono::Utc::now().to_rfc3339());
        assert!(state.in_backoff());
    }

    #[test]
    fn distill_state_persists_across_load() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();

        let mut state = DistillState::default();
        state.record_attempt(false, ws);
        state.record_attempt(false, ws);
        state.record_attempt(true, ws);

        let loaded = DistillState::load(ws);
        assert_eq!(loaded.consecutive_failures, 0);
        assert!(loaded.last_distill_ts.is_some());
        assert!(loaded.last_attempt_ts.is_some());
        assert!(dir.path().join(".state/distill.json").exists());
    }

    #[test]
    fn has_pending_user_memories_detects_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();

        // No users/ at all → nothing pending.
        assert!(!has_pending_user_memories(ws, None));

        // Write one user memory.
        let mem_dir = dir.path().join("users/user-1/memory");
        std::fs::create_dir_all(&mem_dir).unwrap();
        let file = mem_dir.join("note.md");
        std::fs::write(&file, "# note\n").unwrap();

        // Never distilled → pending.
        assert!(has_pending_user_memories(ws, None));

        // Last distill AFTER the file mtime → not pending.
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(!has_pending_user_memories(ws, Some(&future)));

        // Last distill BEFORE the file mtime → pending.
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(has_pending_user_memories(ws, Some(&past)));
    }

    #[test]
    fn build_distill_prompt_contains_scope_and_sanitization_rules() {
        let prompt = build_distill_prompt(2, 3, "anonymized input", "(empty — no agent-level memories yet)");
        assert!(prompt.contains("scope='agent'"), "prompt must force agent scope");
        assert!(
            prompt.contains("De-identification is CRITICAL"),
            "prompt must mandate de-identification"
        );
        assert!(
            prompt.contains("no distillation needed"),
            "prompt must allow a no-op response"
        );
        assert!(prompt.contains("anonymized"), "input must be described as anonymized");
        assert!(
            prompt.contains("## Existing agent-level memories"),
            "prompt must include the existing agent memory index section"
        );
        assert!(
            prompt.contains("never `add` a new memory for a covered topic"),
            "prompt must forbid adding memories for covered topics"
        );
    }

    #[test]
    fn build_existing_agent_index_lists_agent_memories() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();
        let mem_dir = dir.path().join("memory");
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(
            mem_dir.join("spec-verification-before-delivery.md"),
            "---\n\
             name: \"spec-verification-before-delivery\"\n\
             description: \"verify deliverables against spec\"\n\
             type: \"rule\"\n\
             inject: search\n\
             ---\n\
             body\n",
        )
        .unwrap();

        let index = build_existing_agent_index(ws);
        assert!(
            index.contains("spec-verification-before-delivery"),
            "index must list existing agent memory names"
        );
        assert!(index.contains("verify deliverables against spec"));
    }

    #[test]
    fn build_existing_agent_index_empty_when_no_memories() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();
        let index = build_existing_agent_index(ws);
        assert!(
            index.contains("empty"),
            "index must report empty when no agent memories exist"
        );
    }

    #[test]
    fn dedup_blocks_near_duplicate_name() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();
        let mem_dir = dir.path().join("memory");
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(
            mem_dir.join("spec-verification-before-delivery.md"),
            "---\n\
             name: \"spec-verification-before-delivery\"\n\
             description: \"verify deliverables against spec\"\n\
             type: \"rule\"\n\
             inject: search\n\
             ---\n\
             body\n",
        )
        .unwrap();

        // B 演练实测案例：conformance-check vs verification — 3 个共享 token。
        let dup = find_duplicate_agent_memory(ws, "spec-conformance-check-before-delivery");
        assert_eq!(
            dup.as_deref(),
            Some("spec-verification-before-delivery"),
            "near-duplicate name must resolve to the existing memory"
        );
    }

    #[test]
    fn dedup_allows_distinct_name() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();
        let mem_dir = dir.path().join("memory");
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(
            mem_dir.join("spec-verification-before-delivery.md"),
            "---\n\
             name: \"spec-verification-before-delivery\"\n\
             description: \"verify deliverables against spec\"\n\
             type: \"rule\"\n\
             inject: search\n\
             ---\n\
             body\n",
        )
        .unwrap();

        assert_eq!(
            find_duplicate_agent_memory(ws, "release-confirmation-gate"),
            None,
            "distinct topic must not be flagged"
        );
    }

    #[test]
    fn dedup_requires_min_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();
        let mem_dir = dir.path().join("memory");
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(
            mem_dir.join("after-delivery-checks.md"),
            "---\n\
             name: \"after-delivery-checks\"\n\
             description: \"post-delivery verification\"\n\
             type: \"rule\"\n\
             inject: search\n\
             ---\n\
             body\n",
        )
        .unwrap();

        // Shares only {delivery, checks} — below the >=3 token threshold.
        assert_eq!(
            find_duplicate_agent_memory(ws, "before-delivery-checks"),
            None,
            "names sharing fewer than 3 tokens must not be flagged"
        );
    }
}
