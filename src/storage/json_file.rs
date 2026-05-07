//! JSON-file-backed session storage.
//!
//! Directory layout (under `{workspace_dir}/sessions/`):
//!
//! ```text
//! sessions/
//!   active.json              # { "user_id": "session_id", ... }
//!   {session_id}/
//!     meta.json              # all session metadata (identity, counters, compaction state)
//!     history.jsonl          # active segment: one ChatMessage JSON per line, append-only
//!     archive/
//!       history.0000.jsonl   # segments archived on each compaction
//!       history.0001.jsonl
//!       ...
//! ```
//!
//! Message IDs are 1-based line numbers within the active `history.jsonl`.
//! Line numbers reset to 1 on each rotation.  `load_incremental(0)` therefore
//! always returns the full active segment, which is already post-compaction.
//!
//! Compaction state (version, token estimate) lives in `meta.json`; there is
//! no separate `compaction.json`.  The summary message text is stored as a
//! regular line in `history.jsonl` and does not need to be reconstructed.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::storage::{ChatMessage, SessionBackend, SessionInfo, SummaryRecord};

// ── On-disk types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMeta {
    id: String,
    owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    /// 1-based line count of the active history.jsonl; used as the next-ID base.
    message_count: usize,
    /// Number of completed rotations; used to name archive files.
    #[serde(default)]
    segment: u32,
    /// Compaction version (0 = never compacted).
    #[serde(default)]
    compact_version: u32,
    /// Token estimate from the last compaction summary, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compact_token_estimate: Option<u64>,
    /// Last known total token count (input + cached + output) from the API.
    /// Persisted after each response so the value survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ActiveMap {
    #[serde(flatten)]
    map: std::collections::HashMap<String, String>,
}

// ── Backend ───────────────────────────────────────────────────────────────────

/// JSON-file-backed session persistence.
pub struct JsonFileBackend {
    root: PathBuf,
}

impl JsonFileBackend {
    /// Open (or create) the sessions directory at `root`.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    // ── Paths ─────────────────────────────────────────────────────────────────

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("meta.json")
    }

    fn history_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("history.jsonl")
    }

    fn archive_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("archive")
    }

    fn active_path(&self) -> PathBuf {
        self.root.join("active.json")
    }

    // ── Atomic write helpers ──────────────────────────────────────────────────

    /// Write compact JSON atomically via a temp file + rename.
    fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let f = fs::File::create(&tmp)?;
            serde_json::to_writer(f, value).map_err(std::io::Error::other)?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    // ── Meta helpers ──────────────────────────────────────────────────────────

    fn read_meta(&self, session_id: &str) -> Option<SessionMeta> {
        let bytes = fs::read(self.meta_path(session_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_meta(&self, meta: &SessionMeta) -> std::io::Result<()> {
        let dir = self.session_dir(&meta.id);
        fs::create_dir_all(&dir)?;
        Self::write_json_atomic(&self.meta_path(&meta.id), meta)
    }

    // ── Active session map ────────────────────────────────────────────────────

    fn read_active(&self) -> ActiveMap {
        fs::read(self.active_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_active(&self, map: &ActiveMap) -> std::io::Result<()> {
        Self::write_json_atomic(&self.active_path(), map)
    }

    // ── ID generation ─────────────────────────────────────────────────────────

    fn generate_session_id() -> String {
        format!("{:08x}", rand::random::<u32>())
    }

    // ── JSONL helpers ─────────────────────────────────────────────────────────

    /// Read all (line_number, ChatMessage) pairs from the active history.jsonl.
    /// Line numbers are 1-based and reset to 1 after each rotation.
    fn read_history_with_ids(&self, session_id: &str) -> Vec<(i64, ChatMessage)> {
        let path = self.history_path(session_id);
        let Ok(f) = fs::File::open(&path) else { return vec![]; };
        BufReader::new(f)
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let line = line.ok()?;
                let line = line.trim();
                if line.is_empty() { return None; }
                let msg: ChatMessage = serde_json::from_str(line).ok()?;
                Some(((i + 1) as i64, msg))
            })
            .collect()
    }

    fn meta_to_info(meta: &SessionMeta) -> SessionInfo {
        SessionInfo {
            id: meta.id.clone(),
            owner: meta.owner.clone(),
            display_name: meta.display_name.clone(),
            created_at: meta.created_at,
            last_activity: meta.last_activity,
            message_count: meta.message_count,
        }
    }

    // ── History rotation ──────────────────────────────────────────────────────

    fn rotate_history_impl(
        &self,
        session_id: &str,
        surviving: &[(i64, ChatMessage)],
    ) -> std::io::Result<()> {
        let history_path = self.history_path(session_id);
        let mut meta = match self.read_meta(session_id) {
            Some(m) => m,
            None => return Ok(()),
        };

        // Archive the current active segment.
        if history_path.exists() {
            let archive_dir = self.archive_dir(session_id);
            fs::create_dir_all(&archive_dir)?;
            let archive_name = format!("history.{:04}.jsonl", meta.segment);
            fs::rename(&history_path, archive_dir.join(archive_name))?;
        }

        // Write surviving messages to the new active segment.
        if !surviving.is_empty() {
            let mut f = fs::File::create(&history_path)?;
            for (_, msg) in surviving {
                let json = serde_json::to_string(msg).map_err(std::io::Error::other)?;
                writeln!(f, "{json}")?;
            }
            f.flush()?;
            f.sync_all()?;
        }

        // Line numbers restart at 1; update the counter to match the new file.
        meta.message_count = surviving.len();
        meta.segment += 1;
        self.write_meta(&meta)?;
        Ok(())
    }
}

// ── SessionBackend implementation ─────────────────────────────────────────────

impl SessionBackend for JsonFileBackend {
    fn create_session(&self, owner: &str, display_name: Option<&str>) -> std::io::Result<SessionInfo> {
        let id = Self::generate_session_id();
        let now = Utc::now();
        let meta = SessionMeta {
            id: id.clone(),
            owner: owner.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            created_at: now,
            last_activity: now,
            message_count: 0,
            segment: 0,
            compact_version: 0,
            compact_token_estimate: None,
            last_total_tokens: None,
        };
        self.write_meta(&meta)?;

        let mut active = self.read_active();
        if !active.map.contains_key(owner) {
            active.map.insert(owner.to_string(), id.clone());
            self.write_active(&active)?;
        }

        Ok(Self::meta_to_info(&meta))
    }

    fn delete_session(&self, session_id: &str) -> std::io::Result<()> {
        let dir = self.session_dir(session_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }

        let mut active = self.read_active();
        let owners_to_fix: Vec<String> = active.map.iter()
            .filter(|(_, sid)| sid.as_str() == session_id)
            .map(|(uid, _)| uid.clone())
            .collect();

        for owner in owners_to_fix {
            let next = self.list_sessions(&owner)
                .into_iter()
                .find(|s| s.id != session_id)
                .map(|s| s.id);
            match next {
                Some(sid) => { active.map.insert(owner, sid); }
                None => { active.map.remove(&owner); }
            }
        }
        self.write_active(&active)
    }

    fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.display_name = Some(name.to_string());
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        self.read_meta(session_id).as_ref().map(Self::meta_to_info)
    }

    fn list_sessions(&self, owner: &str) -> Vec<SessionInfo> {
        let Ok(entries) = fs::read_dir(&self.root) else { return vec![]; };
        let mut sessions: Vec<SessionInfo> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| {
                let id = e.file_name().to_string_lossy().to_string();
                self.read_meta(&id)
            })
            .filter(|m| m.owner == owner)
            .map(|m| Self::meta_to_info(&m))
            .collect();
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        sessions
    }

    fn get_active_session(&self, user_id: &str) -> Option<String> {
        self.read_active().map.get(user_id).cloned()
    }

    fn set_active_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()> {
        let mut active = self.read_active();
        active.map.insert(user_id.to_string(), session_id.to_string());
        self.write_active(&active)
    }

    fn load_messages(&self, session_id: &str) -> Vec<ChatMessage> {
        self.read_history_with_ids(session_id)
            .into_iter()
            .map(|(_, msg)| msg)
            .collect()
    }

    fn append_message(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<i64> {
        let mut meta = self.read_meta(session_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "session not found")
        })?;
        let new_id = (meta.message_count as i64) + 1;

        let json = serde_json::to_string(message).map_err(std::io::Error::other)?;
        let path = self.history_path(session_id);
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{json}")?;
        f.flush()?;

        meta.message_count = new_id as usize;
        meta.last_activity = Utc::now();
        let _ = self.write_meta(&meta);

        Ok(new_id)
    }

    fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool> {
        let path = self.history_path(session_id);
        let Ok(content) = fs::read_to_string(&path) else { return Ok(false); };

        let mut lines: Vec<&str> = content.split('\n').collect();
        if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        if lines.is_empty() {
            return Ok(false);
        }
        lines.pop();

        let new_content = if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n") + "\n"
        };
        fs::write(&path, new_content)?;

        if let Some(mut meta) = self.read_meta(session_id) {
            meta.last_activity = Utc::now();
            let _ = self.write_meta(&meta);
        }

        Ok(true)
    }

    fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.compact_version = summary.version;
            meta.compact_token_estimate = summary.token_estimate;
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord> {
        let meta = self.read_meta(session_id)?;
        if meta.compact_version == 0 {
            return None;
        }
        Some(SummaryRecord {
            id: 0,
            version: meta.compact_version,
            summary: String::new(),
            up_to_message: 0,
            token_estimate: meta.compact_token_estimate,
            created_at: meta.last_activity,
        })
    }

    fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)> {
        self.read_history_with_ids(session_id)
            .into_iter()
            .filter(|(id, _)| *id > after_message_id)
            .collect()
    }

    fn clear_summary(&self, session_id: &str) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.compact_version = 0;
            meta.compact_token_estimate = None;
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn rotate_history(
        &self,
        session_id: &str,
        surviving: &[(i64, ChatMessage)],
    ) -> std::io::Result<()> {
        self.rotate_history_impl(session_id, surviving)
    }

    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize> {
        let cutoff = Utc::now() - chrono::Duration::hours(ttl_hours as i64);
        let Ok(entries) = fs::read_dir(&self.root) else { return Ok(0); };

        let mut count = 0;
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(ft) = entry.file_type() else { continue; };
            if !ft.is_dir() { continue; }
            let id = entry.file_name().to_string_lossy().to_string();
            if let Some(meta) = self.read_meta(&id) {
                if meta.last_activity < cutoff {
                    let _ = fs::remove_dir_all(entry.path());
                    count += 1;
                }
            }
        }

        let mut active = self.read_active();
        active.map.retain(|_, sid| self.session_dir(sid).exists());
        let _ = self.write_active(&active);

        Ok(count)
    }

    fn save_token_count(&self, session_id: &str, total: u64) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.last_total_tokens = Some(total);
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_token_count(&self, session_id: &str) -> Option<u64> {
        self.read_meta(session_id)?.last_total_tokens
    }
}
