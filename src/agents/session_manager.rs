//! Session manager — manages session lifecycle and persistence.
//!
//! For now uses an in-memory backend. Storage-backed implementation
//! will be added in Phase B.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::providers::capability_chat::ChatMessage;
use crate::storage::SessionBackend;

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

/// Per-session conversation state held by AgentLoop.
#[derive(Debug, Clone)]
pub struct Session {
    /// Unique session key, e.g. "wechat:o9cq80zXpSX1Hz0ph_QNs591k4PA".
    pub key: String,
    /// Current conversation history (in-memory).
    pub history: Vec<ChatMessage>,
}

impl Session {
    pub fn new(key: String) -> Self {
        Self {
            key,
            history: Vec::new(),
        }
    }

    /// Append a user message to history.
    pub fn add_user_text(&mut self, text: String) {
        self.history
            .push(ChatMessage::user_text(text));
    }

    /// Append an assistant text message to history.
    pub fn add_assistant_text(&mut self, text: String) {
        self.history
            .push(ChatMessage::assistant_text(text));
    }

    /// Add a system message to history.
    pub fn add_system_text(&mut self, text: String) {
        self.history
            .push(ChatMessage::system_text(text));
    }

    /// Remove the last assistant message (used when a loop break occurs).
    pub fn pop_last_assistant(&mut self) {
        if let Some(msg) = self.history.pop() {
            if msg.role != "assistant" {
                self.history.push(msg);
            }
        }
    }
}

/// Manages session lifecycle — creates, retrieves, and persists sessions.
pub struct SessionManager {
    backend: Arc<dyn SessionBackend>,
    /// Active sessions (in-memory cache).
    active: RwLock<HashMap<String, Session>>,
}

impl SessionManager {
    /// Create a new SessionManager with the given backend.
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self {
            backend,
            active: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new in-memory SessionManager (for development).
    pub fn in_memory() -> Self {
        Self::new(Arc::new(InMemoryBackend::new()))
    }

    /// Get or create a session by key.
    pub fn get_or_create(&self, key: &str) -> Session {
        // Try in-memory cache first.
        {
            let active = self.active.read();
            if let Some(s) = active.get(key) {
                return s.clone();
            }
        }

        // Load from backend or create new.
        let history = self.backend.load(key);
        let session = if history.is_empty() {
            Session::new(key.to_string())
        } else {
            Session {
                key: key.to_string(),
                history,
            }
        };

        // Cache it.
        {
            let mut active = self.active.write();
            active.insert(key.to_string(), session.clone());
        }

        session
    }

    /// Add a message to a session and persist.
    pub fn append_message(&self, session_key: &str, message: ChatMessage) {
        self.backend.append(session_key, &message).ok();
        let mut active = self.active.write();
        if let Some(session) = active.get_mut(session_key) {
            session.history.push(message);
        }
    }

    /// List all session keys.
    pub fn list_sessions(&self) -> Vec<String> {
        self.backend.list_sessions()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::in_memory()
    }
}
