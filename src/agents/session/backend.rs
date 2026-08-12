//! In-memory session backend and persistence hook implementations.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::providers::capability_chat::ChatMessage;
use crate::storage::{SavedSessionFile, SessionBackend, SessionInfo, SummaryRecord};

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
    suspensions: RwLock<HashMap<String, String>>,
    parents: RwLock<HashMap<String, String>>,
    checkpoints: RwLock<HashMap<String, crate::storage::DelegationCheckpoint>>,
    namespace: String,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self::with_namespace(crate::ids::DEFAULT_NAMESPACE)
    }

    /// In-memory backend generating session FQIDs under `namespace`.
    pub fn with_namespace(namespace: &str) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            messages: RwLock::new(HashMap::new()),
            summaries: RwLock::new(HashMap::new()),
            active: RwLock::new(HashMap::new()),
            suspensions: RwLock::new(HashMap::new()),
            parents: RwLock::new(HashMap::new()),
            checkpoints: RwLock::new(HashMap::new()),
            namespace: namespace.to_string(),
        }
    }
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionBackend for InMemoryBackend {
    fn create_session(
        &self,
        owner: &str,
        display_name: Option<&str>,
    ) -> std::io::Result<SessionInfo> {
        let id = crate::ids::Fqid::new(&self.namespace, crate::ids::TYPE_SESSION).to_string();
        let now = chrono::Utc::now();
        let info = SessionInfo {
            id: id.clone(),
            owner: owner.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            created_at: now,
            last_activity: now,
            message_count: 0,
        };
        self.sessions.write().insert(
            id.clone(),
            InMemorySessionMeta {
                owner: owner.to_string(),
                display_name: display_name.map(|s| s.to_string()),
                created_at: now,
                last_activity: now,
            },
        );
        self.messages.write().insert(id, Vec::new());
        Ok(info)
    }

    fn delete_session(&self, session_id: &str) -> std::io::Result<()> {
        self.sessions.write().remove(session_id);
        self.messages.write().remove(session_id);
        self.summaries.write().remove(session_id);
        self.parents.write().remove(session_id);
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
            let msgs = self
                .messages
                .read()
                .get(session_id)
                .map(|v| v.len())
                .unwrap_or(0);
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
        self.sessions
            .read()
            .iter()
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

    fn list_sessions_for_owner(&self, owner: &str) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> = self.sessions
            .read()
            .iter()
            .filter(|(_, meta)| meta.owner.starts_with(owner))
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
            .collect();
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        sessions
    }

    fn query_message(&self, session_id: &str, message_id: i64) -> Option<(String, String)> {
        let msgs = self.messages.read();
        let session_msgs = msgs.get(session_id)?;
        if message_id <= 0 { return None; }
        let idx = (message_id - 1) as usize;
        let msg = session_msgs.get(idx)?;
        let preview: String = msg.text_content().chars().take(200).collect();
        Some((msg.role.clone(), preview))
    }

    fn list_all_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .read()
            .iter()
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
        self.active
            .write()
            .insert(user_id.to_string(), session_id.to_string());
        Ok(())
    }

    fn load_messages(&self, session_id: &str) -> Vec<ChatMessage> {
        self.messages
            .read()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
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
        self.summaries
            .write()
            .entry(session_id.to_string())
            .or_default()
            .push(summary.clone());
        Ok(())
    }

    fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord> {
        self.summaries
            .read()
            .get(session_id)
            .and_then(|v| v.last().cloned())
    }

    fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)> {
        self.messages
            .read()
            .get(session_id)
            .map(|msgs| {
                msgs.iter()
                    .enumerate()
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

    fn save_session_file(
        &self,
        session_id: &str,
        preferred_name: Option<&str>,
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> std::io::Result<SavedSessionFile> {
        let file_name = crate::storage::session_file_name(preferred_name, bytes, mime_type);
        let root = std::env::current_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("sessions");
        crate::storage::write_session_file(&root, session_id, &file_name, bytes, mime_type)
    }

    // 方案 C (RFC §5): keep the suspension state in-memory so tests can
    // round-trip it like the file backend does on disk.
    fn save_suspension(&self, session_id: &str, json: &str) -> std::io::Result<()> {
        let mut map = self.suspensions.write();
        if json.is_empty() {
            map.remove(session_id);
        } else {
            map.insert(session_id.to_string(), json.to_string());
        }
        Ok(())
    }

    fn load_suspension(&self, session_id: &str) -> Option<String> {
        self.suspensions.read().get(session_id).cloned()
    }

    // Round-trip `parent_session_id` like JsonFileBackend does via meta.json
    // (required for the RFC §6 delegation depth guard to be testable with the
    // in-memory backend).
    fn save_parent_session_id(&self, session_id: &str, parent: &str) -> std::io::Result<()> {
        let mut map = self.parents.write();
        if parent.is_empty() {
            map.remove(session_id);
        } else {
            map.insert(session_id.to_string(), parent.to_string());
        }
        Ok(())
    }

    fn load_parent_session_id(&self, session_id: &str) -> Option<String> {
        self.parents.read().get(session_id).cloned()
    }

    fn save_delegation_checkpoint(
        &self,
        checkpoint: &crate::storage::DelegationCheckpoint,
    ) -> std::io::Result<()> {
        self.checkpoints
            .write()
            .insert(checkpoint.task_id.clone(), checkpoint.clone());
        Ok(())
    }

    fn delete_delegation_checkpoint(&self, task_id: &str) -> std::io::Result<()> {
        self.checkpoints.write().remove(task_id);
        Ok(())
    }

    fn load_delegation_checkpoints(&self) -> Vec<crate::storage::DelegationCheckpoint> {
        self.checkpoints.read().values().cloned().collect()
    }
}

/// Trait for hooks that persist session messages to the backend.
pub trait PersistHook: Send + Sync {
    /// Persist a message and return its assigned backend ID (None on failure).
    fn persist_message(&self, session_id: &str, message: &ChatMessage) -> Option<i64>;
    /// Save inbound bytes as a session-local file.
    fn save_file(
        &self,
        session_id: &str,
        preferred_name: Option<&str>,
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Option<SavedSessionFile>;
    fn save_compaction(&self, session_id: &str, summary: &SummaryRecord);
    /// Archive the current history segment; surviving messages are kept in the new file.
    fn rotate_history(&self, session_id: &str, surviving: &[(i64, ChatMessage)]);
    /// Persist the last known total token count so it survives restarts.
    fn save_token_count(&self, session_id: &str, total: u64);
    /// Persist the session override so it survives restarts.
    fn save_session_override(&self, session_id: &str, override_json: &str);
    /// Persist the last incoming message context (sender / receiver /
    /// text) so startup recovery can replay routing.
    fn save_last_message(&self, session_id: &str, msg: &crate::channels::PersistedChannelMessage);
    /// 方案 C (RFC §5): persist the turn-suspension state to
    /// `sessions/<sid>/suspension.json`; empty `json` deletes the file.
    /// Default no-op (sessions without a persist hook).
    fn save_suspension(&self, _session_id: &str, _json: &str) {}
    /// 方案 C (RFC §5): load the persisted turn-suspension state.
    fn load_suspension(&self, _session_id: &str) -> Option<String> {
        None
    }
    /// Truncate message history to keep only the first `keep_count` messages.
    /// Used for rollback when a turn fails completely.
    fn truncate_messages(&self, session_id: &str, keep_count: usize);
    /// Update (replace) an existing message by its backend ID.
    /// Used by media aging to replace inline File parts with text markers
    /// after the model has processed them in the current turn.
    fn update_message(&self, session_id: &str, message_id: i64, message: &ChatMessage) -> bool {
        let _ = (session_id, message_id, message);
        false
    }
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

    fn save_file(
        &self,
        session_id: &str,
        preferred_name: Option<&str>,
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Option<SavedSessionFile> {
        match self
            .backend
            .save_session_file(session_id, preferred_name, bytes, mime_type)
        {
            Ok(saved) => Some(saved),
            Err(e) => {
                tracing::warn!(session = %session_id, err = %e, "save session file failed");
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
        if let Err(e) = self
            .backend
            .save_session_override(session_id, override_json)
        {
            tracing::warn!(session = %session_id, err = %e, "save session override failed");
        }
    }

    fn save_last_message(&self, session_id: &str, msg: &crate::channels::PersistedChannelMessage) {
        if let Err(e) = self.backend.save_last_message(session_id, msg) {
            tracing::warn!(session = %session_id, err = %e, "save last message failed");
        }
    }

    fn save_suspension(&self, session_id: &str, json: &str) {
        if let Err(e) = self.backend.save_suspension(session_id, json) {
            tracing::warn!(session = %session_id, err = %e, "persist suspension failed");
        }
    }

    fn load_suspension(&self, session_id: &str) -> Option<String> {
        self.backend.load_suspension(session_id)
    }

    fn truncate_messages(&self, session_id: &str, keep_count: usize) {
        if let Err(e) = self.backend.truncate_messages(session_id, keep_count) {
            tracing::warn!(session = %session_id, err = %e, "truncate messages failed");
        }
    }

    fn update_message(&self, session_id: &str, message_id: i64, message: &ChatMessage) -> bool {
        match self.backend.update_message(session_id, message_id, message) {
            Ok(true) => true,
            Ok(false) => false,
            Err(e) => {
                tracing::warn!(
                    session = %session_id,
                    message_id,
                    err = %e,
                    "update_message failed"
                );
                false
            }
        }
    }
}
