//! In-memory session backend and persistence hook implementations.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::providers::capability_chat::ChatMessage;
use crate::storage::{SessionBackend, SessionInfo, SummaryRecord};

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

    fn list_all_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.read().iter()
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
    /// Persist the last reply_target for this session.
    fn save_reply_target(&self, session_id: &str, target: &str);
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

    fn save_reply_target(&self, session_id: &str, target: &str) {
        if let Err(e) = self.backend.save_reply_target(session_id, target) {
            tracing::warn!(session = %session_id, err = %e, "save reply target failed");
        }
    }

    fn truncate_messages(&self, session_id: &str, keep_count: usize) {
        if let Err(e) = self.backend.truncate_messages(session_id, keep_count) {
            tracing::warn!(session = %session_id, err = %e, "truncate messages failed");
        }
    }
}
