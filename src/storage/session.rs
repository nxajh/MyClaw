//! Session domain — types and backend trait for multi-session management.

use chrono::{DateTime, Utc};

/// Re-export ChatMessage from providers module.
pub use crate::providers::ChatMessage;

/// Lightweight session metadata (no history payload).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub owner: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub message_count: usize,
}

/// A persisted summary record from context compaction.
#[derive(Debug, Clone)]
pub struct SummaryRecord {
    pub id: i64,
    pub version: u32,
    pub summary: String,
    pub up_to_message: i64,
    pub token_estimate: Option<u64>,
    pub created_at: DateTime<Utc>,
}

/// Trait for session persistence backends.
pub trait SessionBackend: Send + Sync {
    // ── Session CRUD ───────────────────────────────────────────────────────

    /// Create a new session for the given owner. Returns the session info.
    /// The session ID is generated internally (random 8-hex-char string).
    fn create_session(&self, owner: &str, display_name: Option<&str>) -> std::io::Result<SessionInfo>;

    /// Delete a session and all its messages/summaries.
    fn delete_session(&self, session_id: &str) -> std::io::Result<()>;

    /// Rename a session.
    fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()>;

    /// Get metadata for a single session.
    fn get_session(&self, session_id: &str) -> Option<SessionInfo>;

    /// List all sessions for a given owner, ordered by last_activity DESC.
    fn list_sessions(&self, owner: &str) -> Vec<SessionInfo>;

    /// List ALL sessions across all owners (for startup recovery).
    fn list_all_sessions(&self) -> Vec<SessionInfo>;

    // ── Active session ────────────────────────────────────────────────────

    /// Get the active session ID for a user.
    fn get_active_session(&self, user_id: &str) -> Option<String>;

    /// Set the active session for a user.
    fn set_active_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()>;

    // ── Messages ───────────────────────────────────────────────────────────

    /// Load all messages for a session.
    fn load_messages(&self, session_id: &str) -> Vec<ChatMessage>;

    /// Append a message to a session. Returns the assigned message ID.
    fn append_message(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<i64>;

    /// Remove the last message from a session.
    fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool>;

    /// Truncate message history to keep only the first `keep_count` messages.
    /// Used for rollback when a turn fails completely (e.g. empty LLM response).
    /// Default: no-op (in-memory backend truncates in `Session::rollback_to`).
    fn truncate_messages(&self, _session_id: &str, _keep_count: usize) -> std::io::Result<()> {
        Ok(())
    }

    // ── Summaries ──────────────────────────────────────────────────────────

    /// Save a compaction summary.
    fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()>;

    /// Load the latest compaction summary.
    fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord>;

    /// Load messages added after a given message id (for incremental replay).
    fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)>;

    /// Clear all summaries for a session.
    fn clear_summary(&self, session_id: &str) -> std::io::Result<()>;

    /// Archive the current history segment and write `surviving` messages into
    /// a fresh file.  Called after each compaction.  Default: no-op.
    fn rotate_history(
        &self,
        _session_id: &str,
        _surviving: &[(i64, ChatMessage)],
    ) -> std::io::Result<()> {
        Ok(())
    }

    /// Persist the last known total token count for a session.
    /// Called after each API response so the value survives restarts.
    /// Default: no-op (in-memory backend doesn't need this).
    fn save_token_count(&self, _session_id: &str, _total: u64) -> std::io::Result<()> {
        Ok(())
    }

    /// Load the last persisted total token count for a session.
    fn load_token_count(&self, _session_id: &str) -> Option<u64> {
        None
    }

    /// Persist per-session runtime overrides as a JSON string.
    fn save_session_override(&self, _session_id: &str, _json: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Load persisted per-session runtime overrides (raw JSON string).
    fn load_session_override(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// Persist the last reply_target for a session (e.g. "c2c:<openid>").
    fn save_reply_target(&self, _session_id: &str, _target: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Load persisted last reply_target for a session.
    fn load_reply_target(&self, _session_id: &str) -> Option<String> {
        None
    }

    // ── Maintenance ────────────────────────────────────────────────────────

    /// Clean up sessions older than ttl_hours.
    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize>;
}
