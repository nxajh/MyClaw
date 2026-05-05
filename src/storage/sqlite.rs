//! SQLite-backed session storage.
//!
//! Stores conversation messages in per-session tables using a session_index
//! for global session tracking. Each session gets its own `messages_{key}`
//! and `summaries_{key}` tables.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use crate::storage::{ChatMessage, SessionBackend, SessionMetadata, SessionQuery, SummaryRecord};

/// Sanitize a session key into a valid SQL table name suffix.
fn sanitize_table_name(session_key: &str) -> String {
    session_key
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// Compute the per-session messages table name.
fn messages_table(key: &str) -> String {
    format!("messages_{}", sanitize_table_name(key))
}

/// Compute the per-session summaries table name.
fn summaries_table(key: &str) -> String {
    format!("summaries_{}", sanitize_table_name(key))
}

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
            "CREATE TABLE IF NOT EXISTS session_index (
                session_key    TEXT PRIMARY KEY,
                table_name     TEXT NOT NULL,
                created_at     TEXT NOT NULL DEFAULT (datetime('now')),
                last_activity  TEXT NOT NULL DEFAULT (datetime('now')),
                message_count  INTEGER DEFAULT 0,
                has_summary    INTEGER DEFAULT 0
            );",
        )?;
        Ok(())
    }

    /// Ensure a session exists in the session_index and its per-session
    /// messages table is created.
    fn ensure_session_internal(&self, key: &str) -> anyhow::Result<()> {
        let table = messages_table(key);
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO session_index (session_key, table_name) VALUES (?1, ?2)",
            params![key, table],
        )?;
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {} (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                seq         INTEGER NOT NULL UNIQUE,
                role        TEXT NOT NULL,
                content     TEXT NOT NULL,
                name        TEXT,
                created_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );",
            table,
        ))?;
        Ok(())
    }

    /// Ensure the summaries table exists for a session.
    fn ensure_summaries_table(&self, key: &str) -> anyhow::Result<()> {
        let table = summaries_table(key);
        let conn = self.conn.lock();
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {} (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                summary         TEXT NOT NULL,
                up_to_message   INTEGER NOT NULL,
                token_estimate  INTEGER,
                created_at      TEXT NOT NULL DEFAULT (datetime('now'))
            );",
            table,
        ))?;
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
        // Ensure the session tables exist (no-op if already created).
        if let Err(e) = self.ensure_session_internal(session_key) {
            tracing::warn!(session_key, error = %e, "ensure_session failed in load");
            return Vec::new();
        }

        let table = messages_table(session_key);
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(&format!(
            "SELECT content FROM {} ORDER BY seq ASC",
            table,
        )) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session_key, error = %e, "failed to prepare load query");
                return Vec::new();
            }
        };

        let rows = stmt.query_map([], |row| {
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
        self.ensure_session_internal(session_key)
            .map_err(std::io::Error::other)?;

        let json = Self::serialize_message(message)
            .map_err(std::io::Error::other)?;

        let table = messages_table(session_key);
        let conn = self.conn.lock();

        // Get next sequence number.
        let next_seq: i64 = conn
            .query_row(
                &format!("SELECT COALESCE(MAX(seq), 0) + 1 FROM {}", table),
                [],
                |row| row.get(0),
            )
            .unwrap_or(1);

        conn.execute(
            &format!(
                "INSERT INTO {} (seq, role, content, name) VALUES (?1, ?2, ?3, ?4)",
                table,
            ),
            params![
                next_seq,
                message.role,
                json,
                message.name,
            ],
        )
        .map_err(std::io::Error::other)?;

        // Touch activity timestamp and bump message_count.
        conn.execute(
            "UPDATE session_index SET last_activity = datetime('now'), message_count = message_count + 1 WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;

        Ok(())
    }

    fn remove_last(&self, session_key: &str) -> std::io::Result<bool> {
        let table = messages_table(session_key);
        let conn = self.conn.lock();

        // Find the max seq for this session.
        let max_seq: Option<i64> = conn
            .query_row(
                &format!("SELECT MAX(seq) FROM {}", table),
                [],
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
                &format!("DELETE FROM {} WHERE seq = ?1", table),
                params![seq],
            )
            .map_err(std::io::Error::other)?;

        if deleted > 0 {
            conn.execute(
                "UPDATE session_index SET message_count = MAX(message_count - 1, 0) WHERE session_key = ?1",
                params![session_key],
            )
            .map_err(std::io::Error::other)?;
        }

        Ok(deleted > 0)
    }

    fn list_sessions(&self) -> Vec<String> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT session_key FROM session_index ORDER BY last_activity DESC",
        ) {
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
            "SELECT session_key, created_at, last_activity, message_count
             FROM session_index
             ORDER BY last_activity DESC",
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

    fn compact(&self, _session_key: &str) -> std::io::Result<()> {
        // No-op: per-session tables use AUTOINCREMENT and don't have seq gaps.
        Ok(())
    }

    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize> {
        let conn = self.conn.lock();

        // Find stale sessions first so we can drop their tables.
        let mut stmt = conn
            .prepare(
                "SELECT session_key FROM session_index WHERE last_activity < datetime('now', ?1)",
            )
            .map_err(std::io::Error::other)?;

        let stale_keys: Vec<String> = stmt
            .query_map([format!("-{} hours", ttl_hours)], |row| row.get(0))
            .map_err(std::io::Error::other)?
            .filter_map(|r| r.ok())
            .collect();

        drop(stmt);

        let count = stale_keys.len();

        for key in &stale_keys {
            let table_m = messages_table(key);
            let table_s = summaries_table(key);
            let _ = conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS {}; DROP TABLE IF EXISTS {};",
                table_m, table_s,
            ));
        }

        conn.execute(
            "DELETE FROM session_index WHERE last_activity < datetime('now', ?1)",
            [format!("-{} hours", ttl_hours)],
        )
        .map_err(std::io::Error::other)?;

        Ok(count)
    }

    fn search(&self, query: &SessionQuery) -> Vec<SessionMetadata> {
        let keyword = match &query.keyword {
            Some(k) => k,
            None => return self.list_sessions_with_metadata(),
        };

        let limit = query.limit.unwrap_or(50);

        // Get all sessions from the index, then search each session's messages table.
        let sessions = self.list_sessions_with_metadata();
        let pattern = format!("%{}%", keyword);

        let conn = self.conn.lock();
        let mut results = Vec::new();

        for meta in sessions {
            let table = messages_table(&meta.key);
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {} WHERE content LIKE ?1",
                        table,
                    ),
                    params![pattern],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            if count > 0 {
                results.push(meta);
                if results.len() >= limit {
                    break;
                }
            }
        }

        results
    }

    // ── Session persistence + compaction methods ──────────────────────────

    fn ensure_session(&self, session_key: &str) -> std::io::Result<()> {
        self.ensure_session_internal(session_key)
            .map_err(std::io::Error::other)
    }

    fn touch_session(&self, session_key: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE session_index SET last_activity = datetime('now') WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn update_message_count(&self, session_key: &str, count: usize) -> std::io::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE session_index SET message_count = ?1 WHERE session_key = ?2",
            params![count as i64, session_key],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn save_summary(&self, session_key: &str, summary: &SummaryRecord) -> std::io::Result<()> {
        self.ensure_summaries_table(session_key)
            .map_err(std::io::Error::other)?;

        let table = summaries_table(session_key);
        let conn = self.conn.lock();

        conn.execute(
            &format!(
                "INSERT INTO {} (summary, up_to_message, token_estimate) VALUES (?1, ?2, ?3)",
                table,
            ),
            params![
                summary.summary,
                summary.up_to_message,
                summary.token_estimate.map(|t| t as i64),
            ],
        )
        .map_err(std::io::Error::other)?;

        conn.execute(
            "UPDATE session_index SET has_summary = 1 WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;

        Ok(())
    }

    fn load_latest_summary(&self, session_key: &str) -> Option<SummaryRecord> {
        // Check if summaries table exists first.
        let table = summaries_table(session_key);
        let conn = self.conn.lock();

        // Verify the table exists.
        let table_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                params![table],
                |row| {
                    let c: i64 = row.get(0)?;
                    Ok(c > 0)
                },
            )
            .unwrap_or(false);

        if !table_exists {
            return None;
        }

        let result = conn.query_row(
            &format!(
                "SELECT id, summary, up_to_message, token_estimate, created_at FROM {} ORDER BY id DESC LIMIT 1",
                table,
            ),
            [],
            |row| {
                let id: i64 = row.get(0)?;
                let summary: String = row.get(1)?;
                let up_to_message: i64 = row.get(2)?;
                let token_estimate: Option<i64> = row.get(3)?;
                let created_at: String = row.get(4)?;
                Ok(SummaryRecord {
                    id,
                    version: 0, // legacy records default to 0
                    summary,
                    up_to_message,
                    token_estimate: token_estimate.map(|t| t as u64),
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            },
        );

        result.ok()
    }

    fn load_incremental(&self, session_key: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)> {
        let table = messages_table(session_key);
        let conn = self.conn.lock();

        let mut stmt = match conn.prepare(&format!(
            "SELECT id, content FROM {} WHERE id > ?1 ORDER BY id ASC",
            table,
        )) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session_key, error = %e, "failed to prepare load_incremental query");
                return Vec::new();
            }
        };

        let rows = stmt.query_map(params![after_message_id], |row| {
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
                tracing::warn!(session_key, error = %e, "failed to load incremental messages");
                Vec::new()
            }
        }
    }

    fn clear_summary(&self, session_key: &str) -> std::io::Result<()> {
        let table = summaries_table(session_key);
        let conn = self.conn.lock();

        // Drop the summaries table entirely.
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", table))
            .map_err(std::io::Error::other)?;

        conn.execute(
            "UPDATE session_index SET has_summary = 0 WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;

        Ok(())
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
        {
            let conn = backend.conn.lock();
            conn.execute(
                "UPDATE session_index SET last_activity = datetime('now', '-100 hours') WHERE session_key = 's:old'",
                [],
            ).unwrap();
        }

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
    fn save_and_load_summary() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:sum", &ChatMessage::user_text("a")).unwrap();
        backend.append("s:sum", &ChatMessage::assistant_text("b")).unwrap();

        let summary = SummaryRecord {
            id: 0,
            version: 1,
            summary: "User said a, assistant replied b".to_string(),
            up_to_message: 2,
            token_estimate: Some(50),
            created_at: Utc::now(),
        };

        backend.save_summary("s:sum", &summary).unwrap();
        let loaded = backend.load_latest_summary("s:sum");
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.summary, "User said a, assistant replied b");
        assert_eq!(loaded.up_to_message, 2);
        assert_eq!(loaded.token_estimate, Some(50));
    }

    #[test]
    fn load_incremental_after_summary() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:inc", &ChatMessage::user_text("msg1")).unwrap();
        backend.append("s:inc", &ChatMessage::assistant_text("msg2")).unwrap();
        backend.append("s:inc", &ChatMessage::user_text("msg3")).unwrap();

        let summary = SummaryRecord {
            id: 0,
            version: 1,
            summary: "summary".to_string(),
            up_to_message: 2,
            token_estimate: Some(10),
            created_at: Utc::now(),
        };
        backend.save_summary("s:inc", &summary).unwrap();

        // load_incremental with after_message_id=2 should return only msg3
        let incremental = backend.load_incremental("s:inc", 2);
        assert_eq!(incremental.len(), 1);
        assert_eq!(incremental[0].1.text_content(), "msg3");
    }

    #[test]
    fn clear_summary() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.append("s:clr", &ChatMessage::user_text("a")).unwrap();
        let summary = SummaryRecord {
            id: 0,
            version: 1,
            summary: "test".to_string(),
            up_to_message: 1,
            token_estimate: None,
            created_at: Utc::now(),
        };
        backend.save_summary("s:clr", &summary).unwrap();
        assert!(backend.load_latest_summary("s:clr").is_some());

        backend.clear_summary("s:clr").unwrap();
        assert!(backend.load_latest_summary("s:clr").is_none());
    }

    #[test]
    fn ensure_session_idempotent() {
        let backend = SqliteSessionBackend::in_memory().unwrap();

        backend.ensure_session("s:idem").unwrap();
        backend.ensure_session("s:idem").unwrap();

        let sessions = backend.list_sessions();
        assert_eq!(sessions.len(), 1);
    }
}
