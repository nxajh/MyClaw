//! Session manager — manages multi-session lifecycle and persistence.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::providers::capability_chat::ChatMessage;
use crate::storage::{SessionBackend, SessionInfo, SummaryRecord};

// ── SessionOverride ───────────────────────────────────────────────────────────

/// Per-session runtime overrides applied by slash commands.
///
/// Each field is `None` = use global config default.
/// Persisted in `meta.json` so overrides survive restarts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionOverride {
    /// Force a specific model ID instead of the routing default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override thinking/reasoning mode. None = use model's `reasoning` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// Thinking effort level when thinking is enabled ("low"/"medium"/"high").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Override autonomy level for this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<crate::config::agent::AutonomyLevel>,
    /// Override max tool calls per turn (0 = unlimited).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<usize>,
    /// Override compaction trigger threshold (0.0..1.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<f64>,
    /// Override number of recent work units to retain during compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retain_work_units: Option<usize>,
}

impl SessionOverride {
    /// Returns true if all fields are None (no active overrides).
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.thinking.is_none()
            && self.effort.is_none()
            && self.autonomy.is_none()
            && self.max_tool_calls.is_none()
            && self.compact_threshold.is_none()
            && self.retain_work_units.is_none()
    }

    /// Resolve the optional thinking/effort fields into a `ThinkingConfig`.
    /// `None` = use the model's own reasoning config (no override).
    pub fn to_thinking_config(&self) -> Option<crate::providers::ThinkingConfig> {
        match self.thinking {
            Some(true) => Some(crate::providers::ThinkingConfig {
                enabled: true,
                effort: self.effort.clone(),
            }),
            Some(false) => Some(crate::providers::ThinkingConfig {
                enabled: false,
                effort: None,
            }),
            None => None,
        }
    }
}

/// Remove orphan tool results (tool messages whose tool_call_id has no matching
/// assistant tool_call in the history). Also removes trailing assistant messages
/// with tool_calls that have no corresponding tool results (incomplete round,
/// e.g. process crashed during tool execution).
pub fn sanitize_history(history: &mut Vec<ChatMessage>) {
    // Step 1: Collect all tool_call_ids declared by assistant messages.
    let mut known_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in history.iter() {
        if msg.role == "assistant" {
            if let Some(ref tcs) = msg.tool_calls {
                for tc in tcs {
                    known_tool_ids.insert(tc.id.clone());
                }
            }
        }
    }

    // Step 2: Remove orphan tool results (no matching assistant tool_call).
    let before = history.len();
    history.retain(|msg| {
        if msg.role == "tool" {
            if let Some(ref tc_id) = msg.tool_call_id {
                return known_tool_ids.contains(tc_id);
            }
            return false;
        }
        true
    });

    let removed = before - history.len();
    if removed > 0 {
        tracing::warn!(removed, "sanitized orphan tool results from history");
    }

    // Step 3: Collect tool_call_ids that actually have a tool result.
    let fulfilled_ids: std::collections::HashSet<String> = history
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Step 4: Trim trailing assistant messages whose tool_calls are all unfulfilled.
    // These are left behind when the process crashes during tool execution.
    let mut trimmed = 0;
    while let Some(last) = history.last() {
        if last.role == "assistant" {
            if let Some(ref tcs) = last.tool_calls {
                if !tcs.is_empty() && tcs.iter().all(|tc| !fulfilled_ids.contains(&tc.id)) {
                    history.pop();
                    trimmed += 1;
                    continue;
                }
            }
        }
        break;
    }
    if trimmed > 0 {
        tracing::warn!(trimmed, "sanitized trailing assistant messages with unfulfilled tool_calls");
    }
}

/// Same as `sanitize_history` but keeps IDs paired with their messages throughout,
/// so the returned IDs correctly correspond to the surviving messages.
///
/// `sanitize_history` uses `retain()` which may remove messages from arbitrary
/// positions (not just the tail), so slicing the IDs array after the fact gives
/// wrong IDs. This variant avoids that by filtering both vecs together.
fn sanitize_paired(pairs: Vec<(i64, ChatMessage)>) -> Vec<(i64, ChatMessage)> {
    // Step 1: Remove orphan tool results.
    let known_tool_ids: std::collections::HashSet<String> = pairs
        .iter()
        .filter(|(_, m)| m.role == "assistant")
        .flat_map(|(_, m)| m.tool_calls.iter().flatten().map(|tc| tc.id.clone()))
        .collect();

    let before = pairs.len();
    let mut result: Vec<_> = pairs
        .into_iter()
        .filter(|(_, msg)| {
            if msg.role == "tool" {
                return msg
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| known_tool_ids.contains(id));
            }
            true
        })
        .collect();

    // Step 2: Collect fulfilled tool_call_ids (those with actual tool results).
    let fulfilled_ids: std::collections::HashSet<String> = result
        .iter()
        .filter(|(_, m)| m.role == "tool")
        .filter_map(|(_, m)| m.tool_call_id.clone())
        .collect();

    // Step 3: Trim trailing assistant messages with all-unfulfilled tool_calls.
    let mut trimmed = 0;
    while let Some((_, last)) = result.last() {
        if last.role == "assistant" {
            if let Some(ref tcs) = last.tool_calls {
                if !tcs.is_empty() && tcs.iter().all(|tc| !fulfilled_ids.contains(&tc.id)) {
                    result.pop();
                    trimmed += 1;
                    continue;
                }
            }
        }
        break;
    }
    if trimmed > 0 {
        tracing::warn!(trimmed, "sanitize_paired: trimmed trailing assistant messages with unfulfilled tool_calls");
    }

    let removed = before - result.len();
    if removed > 0 {
        tracing::warn!(removed, "sanitized orphan tool results from history");
    }
    result
}

// ── Breakpoint detection (session recovery) ───────────────────────────────────

/// Describes a tool call that was initiated but never received a result
/// (e.g. the process was killed during tool execution).
#[derive(Debug, Clone)]
pub struct BreakpointItem {
    pub tool_call_id: String,
    pub tool_name: String,
    /// JSON-encoded arguments string.
    pub arguments: String,
}

/// Analyze session history and return tool calls that lack corresponding results.
///
/// Walks the entire message list, tracking which `tool_call_id`s were issued by
/// assistant messages and which received a `tool` result.  Any pending (unfulfilled)
/// tool calls represent "breakpoints" — places where execution was interrupted.
pub fn identify_breakpoint(messages: &[ChatMessage]) -> Vec<BreakpointItem> {
    let mut pending_tools: HashMap<String, (&str, &str)> = HashMap::new();

    for msg in messages {
        match msg.role.as_str() {
            "assistant" => {
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        pending_tools.insert(call.id.clone(), (&call.name, &call.arguments));
                    }
                }
            }
            "tool" => {
                if let Some(id) = &msg.tool_call_id {
                    pending_tools.remove(id.as_str());
                }
            }
            _ => {}
        }
    }

    pending_tools
        .into_iter()
        .map(|(id, (name, arguments))| BreakpointItem {
            tool_call_id: id,
            tool_name: name.to_string(),
            arguments: arguments.to_string(),
        })
        .collect()
}

/// Check whether the message history ends with an incomplete assistant turn
/// (assistant emitted tool_calls but some/all are missing tool results).
///
/// This is a lighter check than [`identify_breakpoint`] — it only examines the
/// tail of the conversation rather than scanning the full history.
pub fn detect_incomplete_turn(messages: &[ChatMessage]) -> bool {
    messages
        .last()
        .map(|m| {
            m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty())
        })
        .unwrap_or(false)
}

struct InMemorySessionMeta {
    owner: String,
    display_name: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    last_activity: chrono::DateTime<chrono::Utc>,
}

/// In-memory session backend for development and testing.
pub struct InMemoryBackend {
    sessions: RwLock<HashMap<String, InMemorySessionMeta>>,
    messages: RwLock<HashMap<String, Vec<ChatMessage>>>,
    summaries: RwLock<HashMap<String, Vec<SummaryRecord>>>,
    active: RwLock<HashMap<String, String>>,
    counter: std::sync::atomic::AtomicU32,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            messages: RwLock::new(HashMap::new()),
            summaries: RwLock::new(HashMap::new()),
            active: RwLock::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionBackend for InMemoryBackend {
    fn create_session(&self, owner: &str, display_name: Option<&str>) -> std::io::Result<SessionInfo> {
        use std::sync::atomic::Ordering;
        let id = format!("{:08x}", self.counter.fetch_add(1, Ordering::Relaxed));
        let now = chrono::Utc::now();
        let info = SessionInfo {
            id: id.clone(),
            owner: owner.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            created_at: now,
            last_activity: now,
            message_count: 0,
        };
        self.sessions.write().insert(id.clone(), InMemorySessionMeta {
            owner: owner.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            created_at: now,
            last_activity: now,
        });
        self.messages.write().insert(id, Vec::new());
        Ok(info)
    }

    fn delete_session(&self, session_id: &str) -> std::io::Result<()> {
        self.sessions.write().remove(session_id);
        self.messages.write().remove(session_id);
        self.summaries.write().remove(session_id);
        let mut active = self.active.write();
        active.retain(|_, v| v != session_id);
        Ok(())
    }

    fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        if let Some(entry) = self.sessions.write().get_mut(session_id) {
            entry.display_name = Some(name.to_string());
        }
        Ok(())
    }

    fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.read().get(session_id).map(|meta| {
            let msgs = self.messages.read().get(session_id).map(|v| v.len()).unwrap_or(0);
            SessionInfo {
                id: session_id.to_string(),
                owner: meta.owner.clone(),
                display_name: meta.display_name.clone(),
                created_at: meta.created_at,
                last_activity: meta.last_activity,
                message_count: msgs,
            }
        })
    }

    fn list_sessions(&self, owner: &str) -> Vec<SessionInfo> {
        self.sessions.read().iter()
            .filter(|(_, meta)| meta.owner == owner)
            .map(|(id, meta)| {
                let msgs = self.messages.read().get(id).map(|v| v.len()).unwrap_or(0);
                SessionInfo {
                    id: id.clone(),
                    owner: meta.owner.clone(),
                    display_name: meta.display_name.clone(),
                    created_at: meta.created_at,
                    last_activity: meta.last_activity,
                    message_count: msgs,
                }
            })
            .collect()
    }

    fn get_active_session(&self, user_id: &str) -> Option<String> {
        self.active.read().get(user_id).cloned()
    }

    fn set_active_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()> {
        self.active.write().insert(user_id.to_string(), session_id.to_string());
        Ok(())
    }

    fn load_messages(&self, session_id: &str) -> Vec<ChatMessage> {
        self.messages.read().get(session_id).cloned().unwrap_or_default()
    }

    fn append_message(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<i64> {
        if let Some(meta) = self.sessions.write().get_mut(session_id) {
            meta.last_activity = chrono::Utc::now();
        }
        let mut guard = self.messages.write();
        let msgs = guard.entry(session_id.to_string()).or_default();
        msgs.push(message.clone());
        Ok(msgs.len() as i64)
    }

    fn truncate_messages(&self, session_id: &str, keep_count: usize) -> std::io::Result<()> {
        if let Some(msgs) = self.messages.write().get_mut(session_id) {
            msgs.truncate(keep_count);
        }
        Ok(())
    }

    fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool> {
        if let Some(msgs) = self.messages.write().get_mut(session_id) {
            if !msgs.is_empty() {
                msgs.pop();
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()> {
        self.summaries.write()
            .entry(session_id.to_string())
            .or_default()
            .push(summary.clone());
        Ok(())
    }

    fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord> {
        self.summaries.read().get(session_id).and_then(|v| v.last().cloned())
    }

    fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)> {
        self.messages.read().get(session_id)
            .map(|msgs| {
                msgs.iter().enumerate()
                    .filter(|(i, _)| ((*i + 1) as i64) > after_message_id)
                    .map(|(i, m)| ((i + 1) as i64, m.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn clear_summary(&self, session_id: &str) -> std::io::Result<()> {
        self.summaries.write().remove(session_id);
        Ok(())
    }

    fn cleanup_stale(&self, _ttl_hours: u32) -> std::io::Result<usize> {
        Ok(0)
    }
}

/// Trait for hooks that persist session messages to the backend.
pub trait PersistHook: Send + Sync {
    /// Persist a message and return its assigned backend ID (None on failure).
    fn persist_message(&self, session_id: &str, message: &ChatMessage) -> Option<i64>;
    fn save_compaction(&self, session_id: &str, summary: &SummaryRecord);
    /// Archive the current history segment; surviving messages are kept in the new file.
    fn rotate_history(&self, session_id: &str, surviving: &[(i64, ChatMessage)]);
    /// Persist the last known total token count so it survives restarts.
    fn save_token_count(&self, session_id: &str, total: u64);
    /// Persist the session override so it survives restarts.
    fn save_session_override(&self, session_id: &str, override_json: &str);
    /// Truncate message history to keep only the first `keep_count` messages.
    /// Used for rollback when a turn fails completely.
    fn truncate_messages(&self, session_id: &str, keep_count: usize);
}

/// PersistHook implementation backed by a SessionBackend.
pub struct BackendPersistHook {
    backend: Arc<dyn SessionBackend>,
}

impl BackendPersistHook {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self { backend }
    }
}

impl PersistHook for BackendPersistHook {
    fn persist_message(&self, session_id: &str, message: &ChatMessage) -> Option<i64> {
        match self.backend.append_message(session_id, message) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(session = %session_id, err = %e, "persist failed");
                None
            }
        }
    }

    fn save_compaction(&self, session_id: &str, summary: &SummaryRecord) {
        if let Err(e) = self.backend.save_summary(session_id, summary) {
            tracing::warn!(session = %session_id, err = %e, "save compaction failed");
        }
    }

    fn rotate_history(&self, session_id: &str, surviving: &[(i64, ChatMessage)]) {
        if let Err(e) = self.backend.rotate_history(session_id, surviving) {
            tracing::warn!(session = %session_id, err = %e, "history rotation failed");
        }
    }

    fn save_token_count(&self, session_id: &str, total: u64) {
        if let Err(e) = self.backend.save_token_count(session_id, total) {
            tracing::warn!(session = %session_id, err = %e, "save token count failed");
        }
    }

    fn save_session_override(&self, session_id: &str, override_json: &str) {
        if let Err(e) = self.backend.save_session_override(session_id, override_json) {
            tracing::warn!(session = %session_id, err = %e, "save session override failed");
        }
    }

    fn truncate_messages(&self, session_id: &str, keep_count: usize) {
        if let Err(e) = self.backend.truncate_messages(session_id, keep_count) {
            tracing::warn!(session = %session_id, err = %e, "truncate messages failed");
        }
    }
}

/// Summary metadata stored in Session memory (no text parsing needed).
#[derive(Debug, Clone)]
pub struct SummaryMetadata {
    pub version: u32,
    pub token_estimate: u64,
    pub up_to_message: i64,
}

/// Per-session conversation state held by AgentLoop.
#[derive(Debug, Clone)]
pub struct Session {
    /// Session ID (e.g. "k3jr9px2").
    pub id: String,
    /// Owner user ID (e.g. "telegram:12345").
    pub owner: String,
    /// Current conversation history (in-memory).
    pub history: Vec<ChatMessage>,
    /// Parallel to `history`: database message IDs, 0 for summary or unpersisted messages.
    pub message_ids: Vec<i64>,
    /// Monotonic compaction version counter.
    pub compact_version: u32,
    /// In-memory summary metadata (restored from backend on load).
    pub summary_metadata: Option<SummaryMetadata>,
    /// Last total token count reported by the API (input + cached + output).
    /// Loaded from meta.json on session restore; None for brand-new sessions.
    pub last_total_tokens: Option<u64>,
    /// Per-session runtime overrides set by slash commands.
    pub session_override: SessionOverride,
    /// Set when the last persisted turn ended with a user message but no
    /// corresponding assistant response (e.g. daemon crash/SIGKILL). The
    /// orchestrator will prompt the user to retry or abort on the next
    /// interaction. Not persisted — rebuilt on every session load.
    pub incomplete_turn: bool,
    /// Tool calls that were pending when the session was interrupted
    /// (assistant emitted tool_calls but no tool results were persisted).
    /// Detected on session load; used by the orchestrator to inject a
    /// recovery prompt so the model can re-execute the missing tools.
    pub breakpoint_items: Vec<BreakpointItem>,
}

impl Session {
    pub fn new(id: String) -> Self {
        Self {
            owner: String::new(),
            id,
            history: Vec::new(),
            message_ids: Vec::new(),
            compact_version: 0,
            summary_metadata: None,
            last_total_tokens: None,
            session_override: SessionOverride::default(),
            incomplete_turn: false,
            breakpoint_items: Vec::new(),
        }
    }

    /// Append a user message to history.
    pub fn add_user_text(&mut self, text: String) {
        self.history.push(ChatMessage::user_text(text));
        self.message_ids.push(0);
    }

    /// Append an assistant text message to history.
    /// Skips empty messages to avoid API format errors on reload.
    pub fn add_assistant_text(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history.push(ChatMessage::assistant_text(text));
        self.message_ids.push(0);
    }

    /// Append an assistant message with tool_calls to history.
    /// Skips only when text is empty AND there are no tool_calls.
    pub fn add_assistant_with_tools(
        &mut self,
        text: String,
        tool_calls: Vec<crate::providers::ToolCall>,
        thinking: Option<String>,
    ) {
        if text.trim().is_empty() && tool_calls.is_empty() {
            return;
        }
        let mut msg = ChatMessage::assistant_text(&text);
        msg.tool_calls = Some(tool_calls);
        if let Some(thinking) = thinking {
            use crate::providers::ContentPart;
            msg.parts.insert(0, ContentPart::Thinking { thinking });
        }
        self.history.push(msg);
        self.message_ids.push(0);
    }

    /// Append a tool result message to history.
    pub fn add_tool_result(&mut self, tool_call_id: String, content: String, is_error: bool) {
        let mut msg = ChatMessage::text("tool", &content);
        msg.tool_call_id = Some(tool_call_id);
        msg.is_error = Some(is_error);
        self.history.push(msg);
        self.message_ids.push(0);
    }

    /// Add a system message to history.
    pub fn add_system_text(&mut self, text: String) {
        self.history.push(ChatMessage::system_text(text));
        self.message_ids.push(0);
    }

    /// Remove the last assistant message (used when a loop break occurs).
    pub fn pop_last_assistant(&mut self) {
        if let Some(msg) = self.history.last() {
            if msg.role == "assistant" {
                self.history.pop();
                self.message_ids.pop();
            }
        }
    }

    /// Roll back history to the given length.
    /// Removes all messages added after position `len` (both in-memory history and message_ids).
    /// Used when a turn fails completely (e.g. empty LLM response) to undo all
    /// messages added during that turn (user + assistant/tool_calls/tool_results).
    pub fn rollback_to(&mut self, len: usize) {
        self.history.truncate(len);
        self.message_ids.truncate(len);
    }
}

/// Manages session lifecycle — creates, retrieves, and persists sessions.
pub struct SessionManager {
    backend: Arc<dyn SessionBackend>,
    /// In-memory session cache: session_id → Session.
    cache: RwLock<HashMap<String, Session>>,
    /// User's active session: user_id → session_id.
    active: RwLock<HashMap<String, String>>,
}

impl SessionManager {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self {
            backend,
            cache: RwLock::new(HashMap::new()),
            active: RwLock::new(HashMap::new()),
        }
    }

    pub fn in_memory() -> Self {
        Self::new(Arc::new(InMemoryBackend::new()))
    }

    /// Get the active session for a user. Auto-creates if none exists.
    /// Attempts summary-based recovery first, then falls back to full load.
    pub fn get_or_create(&self, user_id: &str) -> Session {
        // 1. Resolve active session_id.
        let session_id = self.resolve_active(user_id);

        // 2. Check cache.
        {
            let cache = self.cache.read();
            if let Some(s) = cache.get(&session_id) {
                return s.clone();
            }
        }

        // 3. Load from backend.
        let last_total_tokens = self.backend.load_token_count(&session_id);
        let session_override = self.backend.load_session_override(&session_id)
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let (summary_meta, compact_ver) = match self.backend.load_latest_summary(&session_id) {
            Some(summary) => {
                let meta = SummaryMetadata {
                    version: summary.version,
                    token_estimate: summary.token_estimate.unwrap_or(0),
                    up_to_message: summary.up_to_message,
                };
                (Some(meta), summary.version)
            }
            None => (None, 0),
        };
        let from_compacted = summary_meta.is_some();

        let rows = self.backend.load_incremental(&session_id, 0);
        let count = rows.len();

        // Detect breakpoints on raw (pre-sanitization) messages so we can
        // decide whether to preserve the trailing assistant tool_calls for
        // recovery rather than trimming them away.
        let raw_msgs: Vec<ChatMessage> = rows.iter().map(|(_, m)| m.clone()).collect();
        let breakpoints = identify_breakpoint(&raw_msgs);

        let (ids, msgs, breakpoints) = if !breakpoints.is_empty() {
            // Breakpoint mode: only remove orphan tool results, but keep the
            // trailing assistant message with tool_calls so the model can
            // re-execute the interrupted tools.
            let known_tool_ids: HashSet<String> = rows
                .iter()
                .filter(|(_, m)| m.role == "assistant")
                .flat_map(|(_, m)| m.tool_calls.iter().flatten().map(|tc| tc.id.clone()))
                .collect();
            let filtered: Vec<_> = rows
                .into_iter()
                .filter(|(_, msg)| {
                    if msg.role == "tool" {
                        return msg
                            .tool_call_id
                            .as_ref()
                            .is_some_and(|id| known_tool_ids.contains(id));
                    }
                    true
                })
                .collect();
            let (i, m): (Vec<i64>, Vec<_>) = filtered.into_iter().unzip();
            tracing::warn!(
                session = %session_id,
                breakpoint_count = breakpoints.len(),
                "detected breakpoint: tool calls without results, preserving for recovery"
            );
            (i, m, breakpoints)
        } else {
            let pairs = sanitize_paired(rows);
            let sanitized = pairs.len();
            let (i, m): (Vec<i64>, Vec<_>) = pairs.into_iter().unzip();
            if count > 0 {
                if from_compacted {
                    tracing::info!(
                        session = %session_id,
                        message_count = count,
                        sanitized,
                        last_total_tokens,
                        "session restored from compacted history"
                    );
                } else {
                    tracing::info!(
                        session = %session_id,
                        message_count = count,
                        sanitized,
                        "session restored from full history"
                    );
                }
            }
            (i, m, Vec::new())
        };

        let mut session = Session {
            id: session_id.clone(),
            owner: user_id.to_string(),
            history: msgs,
            message_ids: ids,
            compact_version: compact_ver,
            summary_metadata: summary_meta,
            last_total_tokens,
            session_override,
            incomplete_turn: false,
            breakpoint_items: breakpoints,
        };

        // Inject recovery system prompt when breakpoints are detected so the
        // model knows to re-execute the interrupted tool calls.
        if !session.breakpoint_items.is_empty() {
            let names: Vec<&str> = session
                .breakpoint_items
                .iter()
                .map(|b| b.tool_name.as_str())
                .collect();
            session.history.push(ChatMessage::system_text(format!(
                "⚠️ 上次请求在 tool 执行过程中被中断。中断的 tool calls: [{}]。Session 已恢复，请继续执行未完成的 tool calls。",
                names.join(", ")
            )));
            session.message_ids.push(0);
        }

        // 4. Detect incomplete turn (last message is user without assistant reply).
        //    Only check the most recent turn — earlier orphan user messages are
        //    ignored because compaction or manual cleanup may have removed them.
        if session.history.last().is_some_and(|m| m.role == "user") {
            session.incomplete_turn = true;
            tracing::warn!(session = %session_id, "detected incomplete turn on load");
        }

        // 5. Cache.
        {
            let mut cache = self.cache.write();
            cache.insert(session_id, session.clone());
        }

        session
    }

    /// Resolve the active session_id for a user. Creates one if none exists.
    fn resolve_active(&self, user_id: &str) -> String {
        // 1. Check in-memory mapping.
        if let Some(sid) = self.active.read().get(user_id) {
            return sid.clone();
        }

        // 2. Check backend.
        if let Some(sid) = self.backend.get_active_session(user_id) {
            self.active.write().insert(user_id.to_string(), sid.clone());
            return sid;
        }

        // 3. Auto-create.
        match self.backend.create_session(user_id, None) {
            Ok(info) => {
                let _ = self.backend.set_active_session(user_id, &info.id);
                self.active.write().insert(user_id.to_string(), info.id.clone());
                tracing::info!(user = %user_id, session = %info.id, "auto-created first session");
                info.id
            }
            Err(e) => {
                // Backend failed (disk full, permissions, …). Generate an ephemeral
                // session ID so the agent can still operate this turn, rather than
                // crashing the whole process.
                let ephemeral = format!("ephemeral:{}", uuid::Uuid::new_v4());
                tracing::error!(
                    error = %e,
                    user = %user_id,
                    session = %ephemeral,
                    "backend failed to create session; using ephemeral (non-persisted) session"
                );
                self.active.write().insert(user_id.to_string(), ephemeral.clone());
                ephemeral
            }
        }
    }

    /// Create a new session and make it active for the user.
    pub fn new_session(&self, user_id: &str, name: Option<&str>) -> std::io::Result<SessionInfo> {
        // Invalidate old cached session.
        if let Some(old_id) = self.active.read().get(user_id).cloned() {
            self.cache.write().remove(&old_id);
        }

        let info = self.backend.create_session(user_id, name)?;
        self.backend.set_active_session(user_id, &info.id)?;
        self.active.write().insert(user_id.to_string(), info.id.clone());
        tracing::info!(user = %user_id, session = %info.id, "new session created");
        Ok(info)
    }

    /// Switch to an existing session.
    pub fn switch_session(&self, user_id: &str, session_id: &str) -> std::io::Result<SessionInfo> {
        let info = self.backend.get_session(session_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "session not found"))?;

        if info.owner != user_id {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not your session"));
        }

        // Invalidate old cached session.
        if let Some(old_id) = self.active.read().get(user_id).cloned() {
            self.cache.write().remove(&old_id);
        }

        self.backend.set_active_session(user_id, session_id)?;
        self.active.write().insert(user_id.to_string(), session_id.to_string());
        tracing::info!(user = %user_id, session = %session_id, "switched session");
        Ok(info)
    }

    /// Delete a session. Cannot delete the active session.
    pub fn delete_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()> {
        // Check not active.
        if self.active.read().get(user_id).map(|s| s.as_str()) == Some(session_id) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "cannot delete the active session"));
        }

        let info = self.backend.get_session(session_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "session not found"))?;

        if info.owner != user_id {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not your session"));
        }

        self.cache.write().remove(session_id);
        self.backend.delete_session(session_id)?;
        tracing::info!(user = %user_id, session = %session_id, "session deleted");
        Ok(())
    }

    /// Rename a session.
    pub fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        self.backend.rename_session(session_id, name)
    }

    /// List all sessions for a user.
    pub fn list_sessions(&self, user_id: &str) -> Vec<SessionInfo> {
        self.backend.list_sessions(user_id)
    }

    /// Get the active session_id for a user (None if not resolved yet).
    pub fn active_session_id(&self, user_id: &str) -> Option<String> {
        self.active.read().get(user_id).cloned()
            .or_else(|| self.backend.get_active_session(user_id))
    }

    /// Save a session override for a user's active session.
    /// Updates the in-memory cache and persists to the backend.
    pub fn save_session_override(&self, user_id: &str, session_override: SessionOverride) {
        let session_id = match self.active_session_id(user_id) {
            Some(id) => id,
            None => return,
        };

        // Update cache.
        {
            let mut cache = self.cache.write();
            if let Some(session) = cache.get_mut(&session_id) {
                session.session_override = session_override.clone();
            }
        }

        // Persist.
        if let Ok(json) = serde_json::to_string(&session_override) {
            if let Err(e) = self.backend.save_session_override(&session_id, &json) {
                tracing::warn!(session = %session_id, err = %e, "persist session override failed");
            }
        }
    }

    /// Get the current session override for the user's active session.
    pub fn get_session_override(&self, user_id: &str) -> SessionOverride {
        let session_id = match self.active_session_id(user_id) {
            Some(id) => id,
            None => return SessionOverride::default(),
        };
        self.cache.read().get(&session_id)
            .map(|s| s.session_override.clone())
            .unwrap_or_default()
    }

    /// Append a message to a session and persist.
    pub fn append_message(&self, session_id: &str, message: ChatMessage) {
        let msg_id = self.backend.append_message(session_id, &message).unwrap_or(0);
        let mut cache = self.cache.write();
        if let Some(session) = cache.get_mut(session_id) {
            session.history.push(message);
            session.message_ids.push(msg_id);
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::in_memory()
    }
}

// ── Queue processing (hot-switch recovery) ────────────────────────────────────

/// Get the queue file path for a session.
///
/// Queue files live inside the session directory alongside `history.jsonl`:
/// `sessions/{session_id}/queue.jsonl`.
fn get_session_queue_path(sessions_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    sessions_dir.join(session_id).join("queue.jsonl")
}

/// Append a message to a session's queue file.
///
/// Used during hot-switch to buffer incoming messages while the old process
/// is shutting down and the new one hasn't started yet.  Each message is
/// written as one JSON line (JSONL format).
#[allow(dead_code)]
pub fn enqueue_message(
    sessions_dir: &std::path::Path,
    session_id: &str,
    msg: &ChatMessage,
) -> std::io::Result<()> {
    let queue_file = get_session_queue_path(sessions_dir, session_id);
    if let Some(parent) = queue_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(msg).map_err(std::io::Error::other)? + "\n";
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&queue_file)?
        .write_all(line.as_bytes())
}

/// Read and drain all queued messages for a single session.
///
/// Returns the messages and removes the queue file.  Returns an empty vec
/// if no queue file exists.
pub fn process_queue(
    sessions_dir: &std::path::Path,
    session_id: &str,
) -> std::io::Result<Vec<ChatMessage>> {
    let queue_file = get_session_queue_path(sessions_dir, session_id);
    if !queue_file.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&queue_file)?;
    let messages: Vec<ChatMessage> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // Remove the queue file after successful read.
    std::fs::remove_file(&queue_file)?;

    if !messages.is_empty() {
        tracing::info!(
            session = %session_id,
            count = messages.len(),
            "processed queued messages from hot switch"
        );
    }

    Ok(messages)
}

/// Process queue files for all sessions under the given directory.
///
/// Scans each subdirectory of `sessions_dir` for a `queue.jsonl` file,
/// reads the queued messages, and removes the file.  Returns a map of
/// `session_id → queued messages` (only sessions with non-empty queues
/// are included).
pub fn process_all_queues(
    sessions_dir: &std::path::Path,
) -> std::io::Result<HashMap<String, Vec<ChatMessage>>> {
    let mut result = HashMap::new();
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(_) => return Ok(result),
    };

    for entry in entries.flatten() {
        if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().to_string();
        let messages = process_queue(sessions_dir, &session_id)?;
        if !messages.is_empty() {
            result.insert(session_id, messages);
        }
    }

    Ok(result)
}
