//! JSON-file-backed session storage.
//!
//! Directory layout (under `{workspace_dir}/sessions/`):
//!
//! ```text
//! sessions/
//!   active.json              # { "user_id": "session_id", ... }
//!   {session_id}/
//!     meta.json              # SessionMeta (id, owner, name, timestamps, count)
//!     history.jsonl          # one ChatMessage JSON per line, append-only
//!     compaction.json        # latest SummaryRecord (overwritten each time)
//! ```
//!
//! `append_message` returns the 1-based line number of the new line in
//! history.jsonl; this is used as the `message_id` for `load_incremental`.

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
    message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ActiveMap {
    #[serde(flatten)]
    map: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactionRecord {
    id: i64,
    version: u32,
    summary: String,
    up_to_message: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_estimate: Option<u64>,
    created_at: DateTime<Utc>,
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

    fn compaction_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("compaction.json")
    }

    fn active_path(&self) -> PathBuf {
        self.root.join("active.json")
    }

    // ── Atomic write helpers ──────────────────────────────────────────────────

    /// Write JSON atomically via a temp file + rename.
    fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let f = fs::File::create(&tmp)?;
            serde_json::to_writer_pretty(f, value)
                .map_err(std::io::Error::other)?;
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

    /// Read all (line_number, ChatMessage) pairs from history.jsonl.
    /// Line numbers are 1-based.
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

    fn meta_to_info(meta: SessionMeta) -> SessionInfo {
        SessionInfo {
            id: meta.id,
            owner: meta.owner,
            display_name: meta.display_name,
            created_at: meta.created_at,
            last_activity: meta.last_activity,
            message_count: meta.message_count,
        }
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
        };
        self.write_meta(&meta)?;

        // Set as active if user has no active session yet.
        let mut active = self.read_active();
        if !active.map.contains_key(owner) {
            active.map.insert(owner.to_string(), id.clone());
            self.write_active(&active)?;
        }

        Ok(Self::meta_to_info(meta))
    }

    fn delete_session(&self, session_id: &str) -> std::io::Result<()> {
        let dir = self.session_dir(session_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }

        // Fix active map: remove or switch to another session.
        let mut active = self.read_active();
        let owners_to_fix: Vec<String> = active.map.iter()
            .filter(|(_, sid)| sid.as_str() == session_id)
            .map(|(uid, _)| uid.clone())
            .collect();

        for owner in owners_to_fix {
            // Find another session for this owner.
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
        self.read_meta(session_id).map(Self::meta_to_info)
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
            .map(Self::meta_to_info)
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
        let path = self.history_path(session_id);
        let json = serde_json::to_string(message).map_err(std::io::Error::other)?;

        let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{}", json)?;
        f.flush()?;

        // Count lines to get the 1-based line number of the appended message.
        let line_num = BufReader::new(fs::File::open(&path)?)
            .lines()
            .count() as i64;

        // Update metadata.
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.message_count = line_num as usize;
            meta.last_activity = Utc::now();
            let _ = self.write_meta(&meta);
        }

        Ok(line_num)
    }

    fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool> {
        let path = self.history_path(session_id);
        let Ok(content) = fs::read_to_string(&path) else { return Ok(false); };

        let mut lines: Vec<&str> = content.split('\n').collect();
        // Remove trailing empty element from final newline.
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
            meta.message_count = meta.message_count.saturating_sub(1);
            meta.last_activity = Utc::now();
            let _ = self.write_meta(&meta);
        }

        Ok(true)
    }

    fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()> {
        let record = CompactionRecord {
            id: summary.id,
            version: summary.version,
            summary: summary.summary.clone(),
            up_to_message: summary.up_to_message,
            token_estimate: summary.token_estimate,
            created_at: summary.created_at,
        };
        Self::write_json_atomic(&self.compaction_path(session_id), &record)
    }

    fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord> {
        let bytes = fs::read(self.compaction_path(session_id)).ok()?;
        let rec: CompactionRecord = serde_json::from_slice(&bytes).ok()?;
        Some(SummaryRecord {
            id: rec.id,
            version: rec.version,
            summary: rec.summary,
            up_to_message: rec.up_to_message,
            token_estimate: rec.token_estimate,
            created_at: rec.created_at,
        })
    }

    fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)> {
        self.read_history_with_ids(session_id)
            .into_iter()
            .filter(|(id, _)| *id > after_message_id)
            .collect()
    }

    fn clear_summary(&self, session_id: &str) -> std::io::Result<()> {
        let path = self.compaction_path(session_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
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

        // Clean up active map pointing to deleted sessions.
        let mut active = self.read_active();
        active.map.retain(|_, sid| self.session_dir(sid).exists());
        let _ = self.write_active(&active);

        Ok(count)
    }
}
