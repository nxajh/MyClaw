//! Session manager — manages session lifecycle and persistence.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::RwLock;

use crate::providers::capability_chat::ChatMessage;
use crate::storage::{SessionBackend, SummaryRecord};

/// Remove orphan tool results (tool messages whose tool_call_id has no matching
/// assistant tool_call in the history). Also removes any trailing assistant
/// message with tool_calls that has no subsequent tool results (incomplete round).
///
/// This must be called:
/// 1. After loading session history from DB (recovery path)
/// 2. Before sending messages to the provider (request path)
pub fn sanitize_history(history: &mut Vec<ChatMessage>) {
    // Pass 1: collect all tool_call IDs from assistant messages.
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

    // Pass 2: remove tool messages whose tool_call_id is unknown.
    let before = history.len();
    history.retain(|msg| {
        if msg.role == "tool" {
            if let Some(ref tc_id) = msg.tool_call_id {
                return known_tool_ids.contains(tc_id);
            }
            return false; // tool message without tool_call_id is always invalid
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
    /// session_key -> Vec<ChatMessage>
    messages: RwLock<HashMap<String, Vec<ChatMessage>>>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            messages: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionBackend for InMemoryBackend {
    fn load(&self, session_key: &str) -> Vec<ChatMessage> {
        self.messages
            .read()
            .get(session_key)
            .cloned()
            .unwrap_or_default()
    }

    fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()> {
        let mut guard = self.messages.write();
        guard
            .entry(session_key.to_string())
            .or_default()
            .push(message.clone());
        Ok(())
    }

    fn remove_last(&self, session_key: &str) -> std::io::Result<bool> {
        let mut guard = self.messages.write();
        if let Some(msgs) = guard.get_mut(session_key) {
            if !msgs.is_empty() {
                msgs.pop();
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn list_sessions(&self) -> Vec<String> {
        self.messages.read().keys().cloned().collect()
    }
}

/// Trait for hooks that persist session messages to the backend.
pub trait PersistHook: Send + Sync {
    fn persist_message(&self, session_key: &str, message: &ChatMessage);
    fn save_compaction(&self, session_key: &str, summary: &SummaryRecord);
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
    fn persist_message(&self, session_key: &str, message: &ChatMessage) {
        if let Err(e) = self.backend.append(session_key, message) {
            tracing::warn!(session = %session_key, err = %e, "persist failed");
        }
    }

    fn save_compaction(&self, session_key: &str, summary: &SummaryRecord) {
        if let Err(e) = self.backend.save_summary(session_key, summary) {
            tracing::warn!(session = %session_key, err = %e, "save compaction failed");
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
    /// Unique session key, e.g. "wechat:o9cq80zXpSX1Hz0ph_QNs591k4PA".
    pub key: String,
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
    pub fn new(key: String) -> Self {
        Self {
            key,
            history: Vec::new(),
            message_ids: Vec::new(),
            compact_version: 0,
            summary_metadata: None,
        }
    }

    /// Append a user message to history.
    pub fn add_user_text(&mut self, text: String) {
        self.history
            .push(ChatMessage::user_text(text));
        self.message_ids.push(0);
    }

    /// Append an assistant text message to history.
    pub fn add_assistant_text(&mut self, text: String) {
        self.history
            .push(ChatMessage::assistant_text(text));
        self.message_ids.push(0);
    }

    /// Append an assistant message with tool_calls to history.
    /// Preserves tool_calls and thinking content for correct request formatting.
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
    /// Preserves tool_call_id so providers can format it as tool_result block.
    pub fn add_tool_result(&mut self, tool_call_id: String, content: String, is_error: bool) {
        let mut msg = ChatMessage::text("tool", &content);
        msg.tool_call_id = Some(tool_call_id);
        msg.is_error = Some(is_error);
        self.history.push(msg);
        self.message_ids.push(0);
    }

    /// Add a system message to history.
    pub fn add_system_text(&mut self, text: String) {
        self.history
            .push(ChatMessage::system_text(text));
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
    /// Active sessions (in-memory cache), keyed by actual backend key.
    active: RwLock<HashMap<String, Session>>,
    /// Aliases: user-facing key → actual backend session key.
    /// When /new is called, a new backend key is generated and mapped here.
    aliases: RwLock<HashMap<String, String>>,
    /// Monotonic counter for generating unique backend keys.
    alias_counter: AtomicU32,
}

impl SessionManager {
    /// Create a new SessionManager with the given backend.
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self {
            backend,
            active: RwLock::new(HashMap::new()),
            aliases: RwLock::new(HashMap::new()),
            alias_counter: AtomicU32::new(0),
        }
    }

    /// Create a new in-memory SessionManager (for development).
    pub fn in_memory() -> Self {
        Self::new(Arc::new(InMemoryBackend::new()))
    }

    /// Resolve the actual backend session key for a user-facing key.
    fn resolve_key(&self, key: &str) -> String {
        self.aliases.read().get(key).cloned().unwrap_or_else(|| key.to_string())
    }

    /// Get or create a session by key.
    /// Attempts summary-based recovery first, then falls back to full load.
    pub fn get_or_create(&self, key: &str) -> Session {
        let actual_key = self.resolve_key(key);

        // 1. Check in-memory cache.
        {
            let active = self.active.read();
            if let Some(s) = active.get(&actual_key) {
                return s.clone();
            }
        }

        // 2. Ensure session exists in backend.
        self.backend.ensure_session(&actual_key).ok();

        // 3. Try summary-based recovery.
        let session = match self.backend.load_latest_summary(&actual_key) {
            Some(summary) => {
                let incremental = self.backend.load_incremental(&actual_key, summary.up_to_message);
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
                    session = %actual_key,
                    summary_tokens = ?summary.token_estimate,
                    incremental_count = history.len() - 1,
                    "session restored from summary"
                );

                // message_ids may be longer than history after sanitization; trim.
                message_ids.truncate(history.len());

                Session {
                    key: actual_key.clone(),
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
                let mut full = self.backend.load(&actual_key);
                let count = full.len();
                sanitize_history(&mut full);
                if count > 0 {
                    tracing::info!(session = %actual_key, message_count = count, sanitized = full.len(), "session restored from full history");
                }
                Session {
                    key: actual_key.clone(),
                    message_ids: vec![0; full.len()],
                    history: full,
                    compact_version: 0,
                    summary_metadata: None,
                }
            }
        };

        // 4. Cache.
        {
            let mut active = self.active.write();
            active.insert(actual_key, session.clone());
        }

        session
    }

    /// Add a message to a session and persist.
    pub fn append_message(&self, session_key: &str, message: ChatMessage) {
        let actual_key = self.resolve_key(session_key);
        self.backend.append(&actual_key, &message).ok();
        let mut active = self.active.write();
        if let Some(session) = active.get_mut(&actual_key) {
            session.history.push(message);
            session.message_ids.push(0);
        }
    }

    /// List all session keys.
    pub fn list_sessions(&self) -> Vec<String> {
        self.backend.list_sessions()
    }

    /// Reset a session by creating a new backend key instead of clearing old data.
    /// The old session data remains untouched in the backend.
    pub fn reset(&self, session_key: &str) {
        // Get the current actual key before we change the alias.
        let old_actual_key = self.resolve_key(session_key);
        // Generate new backend key.
        let n = self.alias_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let new_backend_key = format!("{}#s{}", session_key, n);
        // Update alias mapping.
        self.aliases.write().insert(session_key.to_string(), new_backend_key.clone());
        // Remove old cached session.
        self.active.write().remove(&old_actual_key);
        tracing::info!(session = %session_key, old_backend = %old_actual_key, new_backend = %new_backend_key, "session reset (new session created)");
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::in_memory()
    }
}
