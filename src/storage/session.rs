//! Session domain — types and backend trait for multi-session management.

use chrono::{DateTime, Utc};
use std::io::Write;

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

/// A file saved under a session's `files/` directory.
#[derive(Debug, Clone)]
pub struct SavedSessionFile {
    pub path: String,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub size_bytes: u64,
}

/// Build a safe file name for saved session files.
pub fn session_file_name(
    preferred_name: Option<&str>,
    bytes: &[u8],
    mime_type: Option<&str>,
) -> String {
    let mut name = preferred_name
        .map(sanitize_file_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let hash = crate::providers::capability_chat::sha256_hex(bytes);
            let ext = extension_for_mime(mime_type).unwrap_or("bin");
            format!("file-{}.{}", &hash[..12], ext)
        });
    if std::path::Path::new(&name).extension().is_none() {
        if let Some(ext) = extension_for_mime(mime_type) {
            name.push('.');
            name.push_str(ext);
        }
    }
    name
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
    if cleaned.len() <= 180 {
        cleaned
    } else {
        let path = std::path::Path::new(&cleaned);
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
        let mut stem: String = stem.chars().take(150).collect();
        if let Some(ext) = ext {
            stem.push('.');
            stem.push_str(&ext);
        }
        stem
    }
}

fn extension_for_mime(mime: Option<&str>) -> Option<&'static str> {
    match mime?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "audio/ogg" => Some("ogg"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/flac" => Some("flac"),
        "audio/mp4" | "audio/m4a" => Some("m4a"),
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        _ => None,
    }
}

/// Write bytes to a unique file under `<sessions_root>/<session_id>/files/`.
pub fn write_session_file(
    sessions_root: &std::path::Path,
    session_id: &str,
    preferred_name: &str,
    bytes: &[u8],
    mime_type: Option<&str>,
) -> std::io::Result<SavedSessionFile> {
    let dir = sessions_root.join(session_id).join("files");
    std::fs::create_dir_all(&dir)?;
    let path = std::path::Path::new(preferred_name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    let mut candidate = preferred_name.to_string();
    let mut n = 2usize;
    while dir.join(&candidate).exists() {
        candidate = match ext {
            Some(ext) if !ext.is_empty() => format!("{stem}-{n}.{ext}"),
            _ => format!("{stem}-{n}"),
        };
        n += 1;
    }
    let final_path = dir.join(&candidate);
    let tmp = final_path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &final_path)?;
    Ok(SavedSessionFile {
        path: format!("sessions/{session_id}/files/{candidate}"),
        file_name: candidate,
        mime_type: mime_type.map(str::to_string),
        size_bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_name_sanitizes_and_adds_mime_extension() {
        assert_eq!(
            session_file_name(Some(" ../危险/name\0 "), b"abc", Some("image/png")),
            "_危险_name_.png"
        );
        assert_eq!(
            session_file_name(Some("voice"), b"abc", Some("audio/ogg")),
            "voice.ogg"
        );
    }

    #[test]
    fn write_session_file_uses_workspace_relative_path_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let first = write_session_file(
            dir.path(),
            "abc123",
            &session_file_name(Some("photo.png"), b"one", Some("image/png")),
            b"one",
            Some("image/png"),
        )
        .unwrap();
        let second = write_session_file(
            dir.path(),
            "abc123",
            &session_file_name(Some("photo.png"), b"two", Some("image/png")),
            b"two",
            Some("image/png"),
        )
        .unwrap();

        assert_eq!(first.path, "sessions/abc123/files/photo.png");
        assert_eq!(second.path, "sessions/abc123/files/photo-2.png");
        assert_eq!(
            std::fs::read(dir.path().join("abc123/files/photo.png")).unwrap(),
            b"one"
        );
        assert_eq!(
            std::fs::read(dir.path().join("abc123/files/photo-2.png")).unwrap(),
            b"two"
        );
    }
}

/// Trait for session persistence backends.
pub trait SessionBackend: Send + Sync {
    // ── Session CRUD ───────────────────────────────────────────────────────

    /// Create a new session for the given owner. Returns the session info.
    /// The session ID is generated internally (random 8-hex-char string).
    fn create_session(
        &self,
        owner: &str,
        display_name: Option<&str>,
    ) -> std::io::Result<SessionInfo>;

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

    /// Persist the last incoming ChannelMessage as JSON. The
    /// `reply_target` field is read back from this when recovery needs
    /// to know where to deliver the resumed turn's response.
    fn save_last_message(
        &self,
        _session_id: &str,
        _msg: &crate::channels::ChannelMessage,
    ) -> std::io::Result<()> {
        Ok(())
    }

    /// Load the persisted last ChannelMessage.
    fn load_last_message(&self, _session_id: &str) -> Option<crate::channels::ChannelMessage> {
        None
    }

    /// Persist the agent_name owning this session ("main" for top-level, or
    /// the sub-agent name for delegate-spawned sessions).
    fn save_agent_name(&self, _session_id: &str, _name: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Load the agent_name. None → caller uses "main" default.
    fn load_agent_name(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// Persist parent_session_id (sub-sessions only).
    fn save_parent_session_id(&self, _session_id: &str, _parent: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Load parent_session_id. None → top-level session.
    fn load_parent_session_id(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// Save inbound file bytes into `sessions/<session_id>/files/` and return a
    /// workspace-relative path. Backends that cannot persist files may return an
    /// Unsupported error.
    fn save_session_file(
        &self,
        _session_id: &str,
        _preferred_name: Option<&str>,
        _bytes: &[u8],
        _mime_type: Option<&str>,
    ) -> std::io::Result<SavedSessionFile> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session file storage is not supported by this backend",
        ))
    }

    // ── Maintenance ────────────────────────────────────────────────────────

    /// Clean up sessions older than ttl_hours.
    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize>;
}
