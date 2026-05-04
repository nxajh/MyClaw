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

/// A persisted summary record from context compaction.
#[derive(Debug, Clone)]
pub struct SummaryRecord {
    pub id: i64,
    /// Session-level logical version (monotonic, caller-managed).
    pub version: u32,
    pub summary: String,
    pub up_to_message: i64,
    pub token_estimate: Option<u64>,
    pub created_at: DateTime<Utc>,
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

    // ── Session persistence + compaction methods ──────────────────────────

    /// Ensure a session exists in the backend (idempotent).
    fn ensure_session(&self, _session_key: &str) -> std::io::Result<()> { Ok(()) }

    /// Touch the session's last_activity timestamp.
    fn touch_session(&self, _session_key: &str) -> std::io::Result<()> { Ok(()) }

    /// Update the stored message count for a session.
    fn update_message_count(&self, _session_key: &str, _count: usize) -> std::io::Result<()> { Ok(()) }

    /// Persist a compaction summary for a session.
    fn save_summary(&self, _session_key: &str, _summary: &SummaryRecord) -> std::io::Result<()> { Ok(()) }

    /// Load the latest compaction summary for a session, if any.
    fn load_latest_summary(&self, _session_key: &str) -> Option<SummaryRecord> { None }

    /// Load messages added after a given message id (for incremental replay).
    fn load_incremental(&self, _session_key: &str, _after_message_id: i64) -> Vec<(i64, ChatMessage)> { Vec::new() }

    /// Clear all summaries for a session.
    fn clear_summary(&self, _session_key: &str) -> std::io::Result<()> { Ok(()) }
}
