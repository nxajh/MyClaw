//! Session domain — types migrated from domain/session.

use chrono::{DateTime, Utc};

/// Re-export ChatMessage from providers module (multimodal: Vec<ContentPart>).
pub use crate::providers::ChatMessage;

/// Metadata about a persisted session.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub key: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub message_count: usize,
}

/// Query parameters for listing sessions.
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    pub keyword: Option<String>,
    pub limit: Option<usize>,
}

/// Session state information.
#[derive(Debug, Clone)]
pub struct SessionState {
    pub state: String,
    pub turn_id: Option<String>,
    pub turn_started_at: Option<DateTime<Utc>>,
}

/// Trait for session persistence backends.
pub trait SessionBackend: Send + Sync {
    fn load(&self, session_key: &str) -> Vec<ChatMessage>;
    fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()>;
    fn remove_last(&self, session_key: &str) -> std::io::Result<bool>;
    fn list_sessions(&self) -> Vec<String>;
    fn list_sessions_with_metadata(&self) -> Vec<SessionMetadata> {
        self.list_sessions()
            .into_iter()
            .map(|key| {
                let messages = self.load(&key);
                SessionMetadata {
                    key,
                    name: None,
                    created_at: Utc::now(),
                    last_activity: Utc::now(),
                    message_count: messages.len(),
                }
            })
            .collect()
    }
    fn compact(&self, _session_key: &str) -> std::io::Result<()> { Ok(()) }
    fn cleanup_stale(&self, _ttl_hours: u32) -> std::io::Result<usize> { Ok(0) }
    fn search(&self, _query: &SessionQuery) -> Vec<SessionMetadata> { Vec::new() }
    fn list_stuck_sessions(&self, _threshold_secs: u64) -> Vec<SessionMetadata> { Vec::new() }
}
