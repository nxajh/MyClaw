//! Session manager — manages multi-session lifecycle and persistence.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::providers::capability_chat::ChatMessage;
use crate::storage::{SessionBackend, SessionInfo, SummaryRecord};

/// Remove orphan tool results (tool messages whose tool_call_id has no matching
/// assistant tool_call in the history). Also removes any trailing assistant
/// message with tool_calls that has no subsequent tool results (incomplete round).
pub fn sanitize_history(history: &mut Vec<ChatMessage>) {
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
}

/// In-memory session backend for development and testing.
pub struct InMemoryBackend {
    sessions: RwLock<HashMap<String, (String, Option<String>)>>, // id → (owner, display_name)
    messages: RwLock<HashMap<String, Vec<ChatMessage>>>,         // session_id → messages
    summaries: RwLock<HashMap<String, Vec<SummaryRecord>>>,      // session_id → summaries
    active: RwLock<HashMap<String, String>>,                     // user_id → session_id
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
        let info = SessionInfo {
            id: id.clone(),
            owner: owner.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            created_at: chrono::Utc::now(),
            last_activity: chrono::Utc::now(),
            message_count: 0,
        };
        self.sessions.write().insert(id.clone(), (owner.to_string(), display_name.map(|s| s.to_string())));
        self.messages.write().insert(id, Vec::new());
        Ok(info)
    }

    fn delete_session(&self, session_id: &str) -> std::io::Result<()> {
        self.sessions.write().remove(session_id);
        self.messages.write().remove(session_id);
        self.summaries.write().remove(session_id);
        // Clean up any user_state pointing to this session.
        let mut active = self.active.write();
        active.retain(|_, v| v != session_id);
        Ok(())
    }

    fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        if let Some(entry) = self.sessions.write().get_mut(session_id) {
            entry.1 = Some(name.to_string());
        }
        Ok(())
    }

    fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.read().get(session_id).map(|(owner, name)| {
            let msgs = self.messages.read().get(session_id).map(|v| v.len()).unwrap_or(0);
            SessionInfo {
                id: session_id.to_string(),
                owner: owner.clone(),
                display_name: name.clone(),
                created_at: chrono::Utc::now(),
                last_activity: chrono::Utc::now(),
                message_count: msgs,
            }
        })
    }

    fn list_sessions(&self, owner: &str) -> Vec<SessionInfo> {
        self.sessions.read().iter()
            .filter(|(_, (o, _))| o == owner)
            .map(|(id, (owner, name))| {
                let msgs = self.messages.read().get(id).map(|v| v.len()).unwrap_or(0);
                SessionInfo {
                    id: id.clone(),
                    owner: owner.clone(),
                    display_name: name.clone(),
                    created_at: chrono::Utc::now(),
                    last_activity: chrono::Utc::now(),
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
        let mut guard = self.messages.write();
        let msgs = guard.entry(session_id.to_string()).or_default();
        msgs.push(message.clone());
        Ok(msgs.len() as i64)
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
        }
    }

    /// Append a user message to history.
    pub fn add_user_text(&mut self, text: String) {
        self.history.push(ChatMessage::user_text(text));
        self.message_ids.push(0);
    }

    /// Append an assistant text message to history.
    pub fn add_assistant_text(&mut self, text: String) {
        self.history.push(ChatMessage::assistant_text(text));
        self.message_ids.push(0);
    }

    /// Append an assistant message with tool_calls to history.
    pub fn add_assistant_with_tools(
        &mut self,
        text: String,
        tool_calls: Vec<crate::providers::ToolCall>,
        thinking: Option<String>,
    ) {
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
        let session = match self.backend.load_latest_summary(&session_id) {
            Some(summary) => {
                let incremental = self.backend.load_incremental(&session_id, summary.up_to_message);
                let mut history = Vec::with_capacity(incremental.len() + 1);
                let mut message_ids = Vec::with_capacity(incremental.len() + 1);

                history.push(ChatMessage::user_text(
                    format!("[Context Summary] {}", summary.summary)
                ));
                message_ids.push(0);

                for (id, msg) in incremental {
                    history.push(msg);
                    message_ids.push(id);
                }

                sanitize_history(&mut history);

                tracing::info!(
                    session = %session_id,
                    summary_tokens = ?summary.token_estimate,
                    incremental_count = history.len() - 1,
                    "session restored from summary"
                );

                message_ids.truncate(history.len());

                Session {
                    id: session_id.clone(),
                    owner: user_id.to_string(),
                    history,
                    message_ids,
                    compact_version: summary.version,
                    summary_metadata: Some(SummaryMetadata {
                        version: summary.version,
                        token_estimate: summary.token_estimate.unwrap_or(0),
                        up_to_message: summary.up_to_message,
                    }),
                }
            }
            None => {
                // Load all messages with their backend IDs (id > 0 covers all rows).
                let rows = self.backend.load_incremental(&session_id, 0);
                let count = rows.len();
                let (ids, mut msgs): (Vec<i64>, Vec<_>) = rows.into_iter().unzip();
                sanitize_history(&mut msgs);
                let ids = ids[..msgs.len()].to_vec();
                if count > 0 {
                    tracing::info!(session = %session_id, message_count = count, sanitized = msgs.len(), "session restored from full history");
                }
                Session {
                    id: session_id.clone(),
                    owner: user_id.to_string(),
                    message_ids: ids,
                    history: msgs,
                    compact_version: 0,
                    summary_metadata: None,
                }
            }
        };

        // 4. Cache.
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
        let info = self.backend.create_session(user_id, None)
            .expect("failed to auto-create session");
        let _ = self.backend.set_active_session(user_id, &info.id);
        self.active.write().insert(user_id.to_string(), info.id.clone());
        tracing::info!(user = %user_id, session = %info.id, "auto-created first session");
        info.id
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
