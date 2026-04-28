//! SQLite-backed session storage.
//!
//! Stores conversation messages as JSON in a SQLite table.
//! Each session is identified by a unique key (e.g. `telegram:12345`).

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use crate::storage::{ChatMessage, SessionBackend, SessionMetadata, SessionQuery};

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
                session_key  TEXT PRIMARY KEY,
                created_at   TEXT NOT NULL DEFAULT (datetime('now')),
                last_activity TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS messages (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key  TEXT NOT NULL,
                seq          INTEGER NOT NULL,
                role         TEXT NOT NULL,
                content      TEXT NOT NULL,
                name         TEXT,
                created_at   TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_key) REFERENCES sessions(session_key) ON DELETE CASCADE,
                UNIQUE(session_key, seq)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_key, seq);
            ",
        )?;
        Ok(())
    }

    /// Ensure a session row exists.
    fn ensure_session(&self, key: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (session_key) VALUES (?1)",
            params![key],
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
}

impl SessionBackend for SqliteSessionBackend {
    fn load(&self, session_key: &str) -> Vec<ChatMessage> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT content FROM messages WHERE session_key = ?1 ORDER BY seq ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session_key, error = %e, "failed to prepare load query");
                return Vec::new();
            }
        };

        let rows = stmt.query_map(params![session_key], |row| {
            let content: String = row.get(0)?;
            Ok(content)
        });

        match rows {
            Ok(iter) => iter
                .filter_map(|r| r.ok())
                .filter_map(|json| Self::deserialize_message(&json).ok())
                .collect(),
            Err(e) => {
                tracing::warn!(session_key, error = %e, "failed to load messages");
                Vec::new()
            }
        }
    }

    fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()> {
        // Ensure session exists.
        self.ensure_session(session_key)
            .map_err(std::io::Error::other)?;

        let json = Self::serialize_message(message)
            .map_err(std::io::Error::other)?;

        let conn = self.conn.lock();

        // Get next sequence number.
        let next_seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_key = ?1",
                params![session_key],
                |row| row.get(0),
            )
            .unwrap_or(1);

        conn.execute(
            "INSERT INTO messages (session_key, seq, role, content, name) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_key,
                next_seq,
                message.role,
                json,
                message.name,
            ],
        )
        .map_err(std::io::Error::other)?;

        // Touch activity timestamp.
        conn.execute(
            "UPDATE sessions SET last_activity = datetime('now') WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;

        Ok(())
    }

    fn remove_last(&self, session_key: &str) -> std::io::Result<bool> {
        let conn = self.conn.lock();

        // Find the max seq for this session.
        let max_seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq) FROM messages WHERE session_key = ?1",
                params![session_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(std::io::Error::other)?
            .flatten();

        let Some(seq) = max_seq else {
            return Ok(false);
        };

        let deleted = conn.execute(
            "DELETE FROM messages WHERE session_key = ?1 AND seq = ?2",
            params![session_key, seq],
        )
        .map_err(std::io::Error::other)?;

        Ok(deleted > 0)
    }

    fn list_sessions(&self) -> Vec<String> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare("SELECT session_key FROM sessions ORDER BY last_activity DESC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |row| row.get(0));
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn list_sessions_with_metadata(&self) -> Vec<SessionMetadata> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT s.session_key, s.created_at, s.last_activity, COUNT(m.id) as msg_count
             FROM sessions s
             LEFT JOIN messages m ON s.session_key = m.session_key
             GROUP BY s.session_key
             ORDER BY s.last_activity DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = stmt.query_map([], |row| {
            let key: String = row.get(0)?;
            let created_at: String = row.get(1)?;
            let last_activity: String = row.get(2)?;
            let message_count: usize = row.get(3)?;
            Ok((key, created_at, last_activity, message_count))
        });

        match rows {
            Ok(iter) => iter
                .filter_map(|r| r.ok())
                .map(|(key, created_at, last_activity, message_count)| {
                    SessionMetadata {
                        key,
                        name: None,
                        created_at: DateTime::parse_from_rfc3339(&created_at)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        last_activity: DateTime::parse_from_rfc3339(&last_activity)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        message_count,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn compact(&self, session_key: &str) -> std::io::Result<()> {
        // Renumber seq to be contiguous.
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE messages SET seq = sub.new_seq
             FROM (
                SELECT id, ROW_NUMBER() OVER (ORDER BY seq) as new_seq
                FROM messages WHERE session_key = ?1
             ) sub
             WHERE messages.id = sub.id",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize> {
        let conn = self.conn.lock();
        // Delete sessions (and their messages via CASCADE) older than TTL.
        let deleted = conn
            .execute(
                "DELETE FROM sessions WHERE last_activity < datetime('now', ?1)",
                [format!("-{} hours", ttl_hours)],
            )
            .map_err(std::io::Error::other)?;
        Ok(deleted)
    }

    fn search(&self, query: &SessionQuery) -> Vec<SessionMetadata> {
        let keyword = match &query.keyword {
            Some(k) => k,
            None => return self.list_sessions_with_metadata(),
        };

        let limit = query.limit.unwrap_or(50);

        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT DISTINCT s.session_key, s.created_at, s.last_activity, COUNT(m.id) as msg_count
             FROM sessions s
             JOIN messages m ON s.session_key = m.session_key
             WHERE m.content LIKE ?1
             GROUP BY s.session_key
             ORDER BY s.last_activity DESC
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let pattern = format!("%{}%", keyword);
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            let key: String = row.get(0)?;
            let created_at: String = row.get(1)?;
            let last_activity: String = row.get(2)?;
            let message_count: usize = row.get(3)?;
            Ok((key, created_at, last_activity, message_count))
        });

        match rows {
            Ok(iter) => iter
                .filter_map(|r| r.ok())
                .map(|(key, created_at, last_activity, message_count)| {
                    SessionMetadata {
                        key,
                        name: None,
                        created_at: DateTime::parse_from_rfc3339(&created_at)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        last_activity: DateTime::parse_from_rfc3339(&last_activity)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        message_count,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory() {
        let backend = SqliteSessionBackend::in_memory().unwrap();
        let sessions = backend.list_sessions();
        assert!(sessions.is_empty());
    }

    #[test]
    fn append_and_load() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        let msg1 = ChatMessage::user_text("hello");
        let msg2 = ChatMessage::assistant_text("hi there");

        backend.append("test:1", &msg1).unwrap();
        backend.append("test:1", &msg2).unwrap();

        let loaded = backend.load("test:1");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text_content(), "hello");
        assert_eq!(loaded[1].text_content(), "hi there");
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let backend = SqliteSessionBackend::in_memory().unwrap();
        let loaded = backend.load("no:such:key");
        assert!(loaded.is_empty());
    }

    #[test]
    fn remove_last() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("test:2", &ChatMessage::user_text("a")).unwrap();
        backend.append("test:2", &ChatMessage::assistant_text("b")).unwrap();
        backend.append("test:2", &ChatMessage::user_text("c")).unwrap();

        let removed = backend.remove_last("test:2").unwrap();
        assert!(removed);

        let loaded = backend.load("test:2");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].text_content(), "b");
    }

    #[test]
    fn list_sessions() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:1", &ChatMessage::user_text("a")).unwrap();
        backend.append("s:2", &ChatMessage::user_text("b")).unwrap();

        let sessions = backend.list_sessions();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn list_sessions_with_metadata() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:1", &ChatMessage::user_text("a")).unwrap();
        backend.append("s:1", &ChatMessage::assistant_text("b")).unwrap();

        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].message_count, 2);
        assert_eq!(meta[0].key, "s:1");
    }

    #[test]
    fn search_by_keyword() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:1", &ChatMessage::user_text("discuss rust programming")).unwrap();
        backend.append("s:2", &ChatMessage::user_text("talk about python")).unwrap();

        let results = backend.search(&SessionQuery {
            keyword: Some("rust".to_string()),
            limit: Some(10),
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "s:1");
    }

    #[test]
    fn cleanup_stale() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:old", &ChatMessage::user_text("old")).unwrap();

        // Manually set last_activity to far past.
        let conn = backend.conn.lock();
        conn.execute(
            "UPDATE sessions SET last_activity = datetime('now', '-100 hours') WHERE session_key = 's:old'",
            [],
        ).unwrap();
        drop(conn);

        let deleted = backend.cleanup_stale(24).unwrap();
        assert_eq!(deleted, 1);
        assert!(backend.load("s:old").is_empty());
    }

    #[test]
    fn persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db_str = db_path.to_string_lossy().to_string();

        {
            let backend = SqliteSessionBackend::open(&db_str).unwrap();
            backend.append("s:1", &ChatMessage::user_text("persistent")).unwrap();
        }

        // Reopen.
        let backend = SqliteSessionBackend::open(&db_str).unwrap();
        let loaded = backend.load("s:1");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text_content(), "persistent");
    }

    #[test]
    fn compact_renumbers_seq() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:1", &ChatMessage::user_text("a")).unwrap();
        backend.append("s:1", &ChatMessage::assistant_text("b")).unwrap();
        backend.append("s:1", &ChatMessage::user_text("c")).unwrap();

        // Remove middle message.
        backend.remove_last("s:1").unwrap(); // removes "c"
        backend.remove_last("s:1").unwrap(); // removes "b"

        backend.append("s:1", &ChatMessage::assistant_text("d")).unwrap();

        // Compact should renumber and load should return correct order.
        backend.compact("s:1").unwrap();
        let loaded = backend.load("s:1");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text_content(), "a");
        assert_eq!(loaded[1].text_content(), "d");
    }
}
