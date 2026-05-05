//! SQLite-backed session storage.
//!
//! Uses a unified schema with `sessions`, `messages`, `summaries`, and
//! `user_state` tables for multi-session management.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use crate::storage::{ChatMessage, SessionBackend, SessionInfo, SummaryRecord};

/// SQLite-backed session persistence.
pub struct SqliteSessionBackend {
    conn: parking_lot::Mutex<Connection>,
}

impl SqliteSessionBackend {
    /// Open (or create) a SQLite database at the given path.
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        let backend = Self {
            conn: parking_lot::Mutex::new(conn),
        };
        backend.init_tables()?;
        Ok(backend)
    }

    /// Open an in-memory database (for tests).
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let backend = Self {
            conn: parking_lot::Mutex::new(conn),
        };
        backend.init_tables()?;
        Ok(backend)
    }

    fn init_tables(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id            TEXT PRIMARY KEY,
                owner         TEXT NOT NULL,
                display_name  TEXT,
                created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                last_activity TEXT NOT NULL DEFAULT (datetime('now')),
                message_count INTEGER DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_owner ON sessions(owner, last_activity DESC);

            CREATE TABLE IF NOT EXISTS messages (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id  TEXT NOT NULL,
                seq         INTEGER NOT NULL,
                role        TEXT NOT NULL,
                content     TEXT NOT NULL,
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(session_id, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, seq);

            CREATE TABLE IF NOT EXISTS summaries (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id      TEXT NOT NULL,
                summary         TEXT NOT NULL,
                up_to_message   INTEGER NOT NULL,
                token_estimate  INTEGER,
                version         INTEGER DEFAULT 0,
                created_at      TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_summaries_session ON summaries(session_id);

            CREATE TABLE IF NOT EXISTS user_state (
                user_id         TEXT PRIMARY KEY,
                active_session  TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    /// Serialize a ChatMessage to a JSON string.
    fn serialize_message(msg: &ChatMessage) -> anyhow::Result<String> {
        Ok(serde_json::to_string(msg)?)
    }

    /// Deserialize a ChatMessage from a JSON string.
    fn deserialize_message(json: &str) -> anyhow::Result<ChatMessage> {
        Ok(serde_json::from_str(json)?)
    }

    /// Parse a datetime string from SQLite into DateTime<Utc>.
    fn parse_datetime(s: &str) -> DateTime<Utc> {
        // SQLite datetime format: "YYYY-MM-DD HH:MM:SS"
        // Try RFC 3339 first, then fall back to SQLite format.
        DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| {
                // Try parsing as SQLite format "YYYY-MM-DD HH:MM:SS"
                chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                    .map(|dt| dt.and_utc())
                    .unwrap_or_else(|_| Utc::now())
            })
    }

    /// Generate a random 8-hex-char session ID.
    fn generate_session_id() -> String {
        format!("{:08x}", rand::random::<u32>())
    }
}

impl SessionBackend for SqliteSessionBackend {
    fn create_session(&self, owner: &str, display_name: Option<&str>) -> std::io::Result<SessionInfo> {
        let id = Self::generate_session_id();
        let conn = self.conn.lock();

        conn.execute(
            "INSERT INTO sessions (id, owner, display_name) VALUES (?1, ?2, ?3)",
            params![id, owner, display_name],
        )
        .map_err(std::io::Error::other)?;

        // If user has no active session, set this one as active.
        let has_active: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM user_state WHERE user_id = ?1",
                params![owner],
                |row| {
                    let c: i64 = row.get(0)?;
                    Ok(c > 0)
                },
            )
            .unwrap_or(false);

        if !has_active {
            conn.execute(
                "INSERT INTO user_state (user_id, active_session) VALUES (?1, ?2)",
                params![owner, id],
            )
            .map_err(std::io::Error::other)?;
        }

        let created_at: String = conn
            .query_row(
                "SELECT created_at FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(std::io::Error::other)?;

        let last_activity: String = conn
            .query_row(
                "SELECT last_activity FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(std::io::Error::other)?;

        Ok(SessionInfo {
            id,
            owner: owner.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            created_at: Self::parse_datetime(&created_at),
            last_activity: Self::parse_datetime(&last_activity),
            message_count: 0,
        })
    }

    fn delete_session(&self, session_id: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();

        conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(std::io::Error::other)?;

        conn.execute(
            "DELETE FROM summaries WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(std::io::Error::other)?;

        conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![session_id],
        )
        .map_err(std::io::Error::other)?;

        // If this was the active session, clear user_state or switch to another session.
        let users_to_fix: Vec<String> = conn
            .query_row(
                "SELECT user_id FROM user_state WHERE active_session = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .into_iter()
            .filter_map(|r: Result<String, _>| r.ok())
            .collect();

        // Also try query_map for multiple users (shouldn't happen, but be safe).
        let users_to_fix = if users_to_fix.is_empty() {
            let mut stmt = conn
                .prepare("SELECT user_id FROM user_state WHERE active_session = ?1")
                .map_err(std::io::Error::other)?;
            stmt.query_map(params![session_id], |row| row.get(0))
                .map_err(std::io::Error::other)?
                .filter_map(|r: Result<String, _>| r.ok())
                .collect()
        } else {
            users_to_fix
        };

        for user_id in users_to_fix {
            // Try to switch to another session for this user.
            let next_session: Option<String> = conn
                .query_row(
                    "SELECT id FROM sessions WHERE owner = ?1 ORDER BY last_activity DESC LIMIT 1",
                    params![user_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(std::io::Error::other)?;

            match next_session {
                Some(sid) => {
                    conn.execute(
                        "UPDATE user_state SET active_session = ?1 WHERE user_id = ?2",
                        params![sid, user_id],
                    )
                    .map_err(std::io::Error::other)?;
                }
                None => {
                    conn.execute(
                        "DELETE FROM user_state WHERE user_id = ?1",
                        params![user_id],
                    )
                    .map_err(std::io::Error::other)?;
                }
            }
        }

        Ok(())
    }

    fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET display_name = ?1 WHERE id = ?2",
            params![name, session_id],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        let conn = self.conn.lock();
        let result = conn.query_row(
            "SELECT id, owner, display_name, created_at, last_activity, message_count
             FROM sessions WHERE id = ?1",
            params![session_id],
            |row| {
                let id: String = row.get(0)?;
                let owner: String = row.get(1)?;
                let display_name: Option<String> = row.get(2)?;
                let created_at: String = row.get(3)?;
                let last_activity: String = row.get(4)?;
                let message_count: usize = row.get(5)?;
                Ok((id, owner, display_name, created_at, last_activity, message_count))
            },
        );

        match result {
            Ok((id, owner, display_name, created_at, last_activity, message_count)) => Some(SessionInfo {
                id,
                owner,
                display_name,
                created_at: Self::parse_datetime(&created_at),
                last_activity: Self::parse_datetime(&last_activity),
                message_count,
            }),
            Err(_) => None,
        }
    }

    fn list_sessions(&self, owner: &str) -> Vec<SessionInfo> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, owner, display_name, created_at, last_activity, message_count
             FROM sessions WHERE owner = ?1 ORDER BY last_activity DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = stmt.query_map(params![owner], |row| {
            let id: String = row.get(0)?;
            let owner: String = row.get(1)?;
            let display_name: Option<String> = row.get(2)?;
            let created_at: String = row.get(3)?;
            let last_activity: String = row.get(4)?;
            let message_count: usize = row.get(5)?;
            Ok((id, owner, display_name, created_at, last_activity, message_count))
        });

        match rows {
            Ok(iter) => iter
                .filter_map(|r| r.ok())
                .map(|(id, owner, display_name, created_at, last_activity, message_count)| SessionInfo {
                    id,
                    owner,
                    display_name,
                    created_at: Self::parse_datetime(&created_at),
                    last_activity: Self::parse_datetime(&last_activity),
                    message_count,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn get_active_session(&self, user_id: &str) -> Option<String> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT active_session FROM user_state WHERE user_id = ?1",
            params![user_id],
            |row| row.get(0),
        )
        .ok()
    }

    fn set_active_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO user_state (user_id, active_session) VALUES (?1, ?2)",
            params![user_id, session_id],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn load_messages(&self, session_id: &str) -> Vec<ChatMessage> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT content FROM messages WHERE session_id = ?1 ORDER BY seq ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session_id, error = %e, "failed to prepare load_messages query");
                return Vec::new();
            }
        };

        let rows = stmt.query_map(params![session_id], |row| {
            let content: String = row.get(0)?;
            Ok(content)
        });

        match rows {
            Ok(iter) => iter
                .filter_map(|r| r.ok())
                .filter_map(|json| Self::deserialize_message(&json).ok())
                .collect(),
            Err(e) => {
                tracing::warn!(session_id, error = %e, "failed to load messages");
                Vec::new()
            }
        }
    }

    fn append_message(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<()> {
        let json = Self::serialize_message(message).map_err(std::io::Error::other)?;

        let conn = self.conn.lock();

        // Get next sequence number.
        let next_seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap_or(1);

        conn.execute(
            "INSERT INTO messages (session_id, seq, role, content) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, next_seq, message.role, json],
        )
        .map_err(std::io::Error::other)?;

        // Update last_activity and bump message_count.
        conn.execute(
            "UPDATE sessions SET last_activity = datetime('now'), message_count = message_count + 1 WHERE id = ?1",
            params![session_id],
        )
        .map_err(std::io::Error::other)?;

        Ok(())
    }

    fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool> {
        let conn = self.conn.lock();

        // Find the max seq for this session.
        let max_seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq) FROM messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(std::io::Error::other)?
            .flatten();

        let Some(seq) = max_seq else {
            return Ok(false);
        };

        let deleted = conn
            .execute(
                "DELETE FROM messages WHERE session_id = ?1 AND seq = ?2",
                params![session_id, seq],
            )
            .map_err(std::io::Error::other)?;

        if deleted > 0 {
            conn.execute(
                "UPDATE sessions SET message_count = MAX(message_count - 1, 0) WHERE id = ?1",
                params![session_id],
            )
            .map_err(std::io::Error::other)?;
        }

        Ok(deleted > 0)
    }

    fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()> {
        let conn = self.conn.lock();

        conn.execute(
            "INSERT INTO summaries (session_id, summary, up_to_message, token_estimate, version) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                summary.summary,
                summary.up_to_message,
                summary.token_estimate.map(|t| t as i64),
                summary.version as i64,
            ],
        )
        .map_err(std::io::Error::other)?;

        Ok(())
    }

    fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord> {
        let conn = self.conn.lock();

        let result = conn.query_row(
            "SELECT id, summary, up_to_message, token_estimate, version, created_at
             FROM summaries WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
            params![session_id],
            |row| {
                let id: i64 = row.get(0)?;
                let summary: String = row.get(1)?;
                let up_to_message: i64 = row.get(2)?;
                let token_estimate: Option<i64> = row.get(3)?;
                let version: i64 = row.get(4)?;
                let created_at: String = row.get(5)?;
                Ok(SummaryRecord {
                    id,
                    version: version as u32,
                    summary,
                    up_to_message,
                    token_estimate: token_estimate.map(|t| t as u64),
                    created_at: Self::parse_datetime(&created_at),
                })
            },
        );

        result.ok()
    }

    fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)> {
        let conn = self.conn.lock();

        let mut stmt = match conn.prepare(
            "SELECT id, content FROM messages WHERE session_id = ?1 AND id > ?2 ORDER BY id ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session_id, error = %e, "failed to prepare load_incremental query");
                return Vec::new();
            }
        };

        let rows = stmt.query_map(params![session_id, after_message_id], |row| {
            let id: i64 = row.get(0)?;
            let content: String = row.get(1)?;
            Ok((id, content))
        });

        match rows {
            Ok(iter) => iter
                .filter_map(|r| r.ok())
                .filter_map(|(id, json)| {
                    Self::deserialize_message(&json).ok().map(|msg| (id, msg))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(session_id, error = %e, "failed to load incremental messages");
                Vec::new()
            }
        }
    }

    fn clear_summary(&self, session_id: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM summaries WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize> {
        let conn = self.conn.lock();

        // Find stale sessions.
        let mut stmt = conn
            .prepare(
                "SELECT id FROM sessions WHERE last_activity < datetime('now', ?1)",
            )
            .map_err(std::io::Error::other)?;

        let stale_ids: Vec<String> = stmt
            .query_map([format!("-{} hours", ttl_hours)], |row| row.get(0))
            .map_err(std::io::Error::other)?
            .filter_map(|r| r.ok())
            .collect();

        drop(stmt);

        let count = stale_ids.len();

        for id in &stale_ids {
            let _ = conn.execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params![id],
            );
            let _ = conn.execute(
                "DELETE FROM summaries WHERE session_id = ?1",
                params![id],
            );
            let _ = conn.execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![id],
            );
        }

        // Clean up user_state entries pointing to deleted sessions.
        for id in &stale_ids {
            // Try to switch to another session, or delete the user_state entry.
            let users_to_fix: Vec<String> = {
                let mut s = conn
                    .prepare("SELECT user_id FROM user_state WHERE active_session = ?1")
                    .map_err(std::io::Error::other)?;
                s.query_map(params![id], |row| row.get(0))
                    .map_err(std::io::Error::other)?
                    .filter_map(|r: Result<String, _>| r.ok())
                    .collect()
            };

            for user_id in users_to_fix {
                let next_session: Option<String> = conn
                    .query_row(
                        "SELECT id FROM sessions WHERE owner = ?1 ORDER BY last_activity DESC LIMIT 1",
                        params![user_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(std::io::Error::other)?;

                match next_session {
                    Some(sid) => {
                        let _ = conn.execute(
                            "UPDATE user_state SET active_session = ?1 WHERE user_id = ?2",
                            params![sid, user_id],
                        );
                    }
                    None => {
                        let _ = conn.execute(
                            "DELETE FROM user_state WHERE user_id = ?1",
                            params![user_id],
                        );
                    }
                }
            }
        }

        Ok(count)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory() {
        let backend = SqliteSessionBackend::in_memory().unwrap();
        let sessions = backend.list_sessions("test_user");
        assert!(sessions.is_empty());
    }

    #[test]
    fn append_and_load() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let session = backend.create_session("alice", None).unwrap();
        let session_id = &session.id;

        let msg1 = ChatMessage::user_text("hello");
        let msg2 = ChatMessage::assistant_text("hi there");

        backend.append_message(session_id, &msg1).unwrap();
        backend.append_message(session_id, &msg2).unwrap();

        let loaded = backend.load_messages(session_id);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text_content(), "hello");
        assert_eq!(loaded[1].text_content(), "hi there");
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let backend = SqliteSessionBackend::in_memory().unwrap();
        let loaded = backend.load_messages("no:such:session");
        assert!(loaded.is_empty());
    }

    #[test]
    fn remove_last_message() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let session = backend.create_session("alice", None).unwrap();
        let session_id = &session.id;

        backend.append_message(session_id, &ChatMessage::user_text("a")).unwrap();
        backend.append_message(session_id, &ChatMessage::assistant_text("b")).unwrap();
        backend.append_message(session_id, &ChatMessage::user_text("c")).unwrap();

        let removed = backend.remove_last_message(session_id).unwrap();
        assert!(removed);

        let loaded = backend.load_messages(session_id);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].text_content(), "b");
    }

    #[test]
    fn list_sessions() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.create_session("alice", None).unwrap();
        backend.create_session("alice", None).unwrap();

        let sessions = backend.list_sessions("alice");
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn cleanup_stale() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let session = backend.create_session("alice", None).unwrap();

        // Manually set last_activity to far past.
        {
            let conn = backend.conn.lock();
            conn.execute(
                "UPDATE sessions SET last_activity = datetime('now', '-100 hours') WHERE id = ?1",
                params![session.id],
            ).unwrap();
        }

        let deleted = backend.cleanup_stale(24).unwrap();
        assert_eq!(deleted, 1);
        assert!(backend.load_messages(&session.id).is_empty());
    }

    #[test]
    fn persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db_str = db_path.to_string_lossy().to_string();

        let session_id = {
            let backend = SqliteSessionBackend::open(&db_str).unwrap();
            let session = backend.create_session("alice", None).unwrap();
            let sid = session.id.clone();
            backend.append_message(&sid, &ChatMessage::user_text("persistent")).unwrap();
            sid
        };

        // Reopen.
        let backend = SqliteSessionBackend::open(&db_str).unwrap();
        let loaded = backend.load_messages(&session_id);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text_content(), "persistent");
    }

    #[test]
    fn save_and_load_summary() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let session = backend.create_session("alice", None).unwrap();
        let session_id = &session.id;

        backend.append_message(session_id, &ChatMessage::user_text("a")).unwrap();
        backend.append_message(session_id, &ChatMessage::assistant_text("b")).unwrap();

        let summary = SummaryRecord {
            id: 0,
            version: 1,
            summary: "User said a, assistant replied b".to_string(),
            up_to_message: 2,
            token_estimate: Some(50),
            created_at: Utc::now(),
        };

        backend.save_summary(session_id, &summary).unwrap();
        let loaded = backend.load_latest_summary(session_id);
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.summary, "User said a, assistant replied b");
        assert_eq!(loaded.up_to_message, 2);
        assert_eq!(loaded.token_estimate, Some(50));
    }

    #[test]
    fn load_incremental_after_summary() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let session = backend.create_session("alice", None).unwrap();
        let session_id = &session.id;

        backend.append_message(session_id, &ChatMessage::user_text("msg1")).unwrap();
        backend.append_message(session_id, &ChatMessage::assistant_text("msg2")).unwrap();
        backend.append_message(session_id, &ChatMessage::user_text("msg3")).unwrap();

        let summary = SummaryRecord {
            id: 0,
            version: 1,
            summary: "summary".to_string(),
            up_to_message: 2,
            token_estimate: Some(10),
            created_at: Utc::now(),
        };
        backend.save_summary(session_id, &summary).unwrap();

        // load_incremental with after_message_id=2 should return only msg3
        let incremental = backend.load_incremental(session_id, 2);
        assert_eq!(incremental.len(), 1);
        assert_eq!(incremental[0].1.text_content(), "msg3");
    }

    #[test]
    fn clear_summary() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let session = backend.create_session("alice", None).unwrap();
        let session_id = &session.id;

        backend.append_message(session_id, &ChatMessage::user_text("a")).unwrap();
        let summary = SummaryRecord {
            id: 0,
            version: 1,
            summary: "test".to_string(),
            up_to_message: 1,
            token_estimate: None,
            created_at: Utc::now(),
        };
        backend.save_summary(session_id, &summary).unwrap();
        assert!(backend.load_latest_summary(session_id).is_some());

        backend.clear_summary(session_id).unwrap();
        assert!(backend.load_latest_summary(session_id).is_none());
    }

    #[test]
    fn ensure_session_idempotent() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let s1 = backend.create_session("alice", None).unwrap();
        let fetched = backend.get_session(&s1.id);
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().owner, "alice");

        // Creating another session should yield a different id (probabilistically).
        let s2 = backend.create_session("alice", None).unwrap();
        assert_ne!(s1.id, s2.id);
    }
}
