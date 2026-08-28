use std::collections::HashSet;
use std::fs;
use std::io::Write;

use chrono::Utc;

use super::backend::JsonFileBackend;
use super::records::{SegmentRecord, SessionMeta};
use crate::storage::{ChatMessage, SavedSessionFile, SessionBackend, SessionInfo, SummaryRecord};

// ── SessionBackend implementation ─────────────────────────────────────────────

impl SessionBackend for JsonFileBackend {
    fn create_session(
        &self,
        owner: &str,
        display_name: Option<&str>,
    ) -> std::io::Result<SessionInfo> {
        let id = self.generate_session_id();
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
            session_override: None,
            last_message: None,
            summary: None,
            agent_name: None,
            parent_session_id: None,
            delegation_timeout_secs: None,
            delegation_allowed_tools: None,
            segments: vec![SegmentRecord {
                segment: 0,
                start_id: 1,
                count: 0,
                compactions: Vec::new(),
            }],
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
        let owners_to_fix: Vec<String> = active
            .map
            .iter()
            .filter(|(_, sid)| sid.as_str() == session_id)
            .map(|(uid, _)| uid.clone())
            .collect();

        for owner in owners_to_fix {
            let next = self
                .list_sessions(&owner)
                .into_iter()
                .find(|s| s.id != session_id)
                .map(|s| s.id);
            match next {
                Some(sid) => {
                    active.map.insert(owner, sid);
                }
                None => {
                    active.map.remove(&owner);
                }
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
        let Ok(entries) = fs::read_dir(&self.root) else {
            return vec![];
        };
        let mut sessions: Vec<SessionInfo> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| Self::read_meta_at(&e.path()))
            .filter(|m| m.owner == owner)
            .map(|m| Self::meta_to_info(&m))
            .collect();
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        sessions
    }

    fn list_all_sessions(&self) -> Vec<SessionInfo> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return vec![];
        };
        let mut sessions: Vec<SessionInfo> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| Self::read_meta_at(&e.path()))
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
        active
            .map
            .insert(user_id.to_string(), session_id.to_string());
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

        // Persist path-only message content as-is.
        let message = self.externalize(session_id, message)?;
        let json = serde_json::to_string(&message).map_err(std::io::Error::other)?;
        let path = self.history_path(session_id);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{json}")?;
        f.flush()?;

        meta.message_count = new_id as usize;
        meta.last_activity = Utc::now();
        // Update active segment count.
        if let Some(seg) = meta.segments.iter_mut().find(|s| s.segment == meta.segment) {
            seg.count += 1;
        }
        let _ = self.write_meta(&meta);

        Ok(new_id)
    }

    fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool> {
        let path = self.history_path(session_id);
        let Ok(content) = fs::read_to_string(&path) else {
            return Ok(false);
        };

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
            // Decrement active segment count and global message_count.
            if let Some(seg) = meta.segments.iter_mut().find(|s| s.segment == meta.segment) {
                seg.count = seg.count.saturating_sub(1);
            }
            meta.message_count = meta.message_count.saturating_sub(1);
            let _ = self.write_meta(&meta);
        }

        Ok(true)
    }

    fn delete_message_by_id(&self, session_id: &str, message_id: i64) -> std::io::Result<bool> {
        let path = self.history_path(session_id);
        let Ok(content) = fs::read_to_string(&path) else {
            return Ok(false);
        };

        // Compute the line index relative to the active segment's start_id.
        let meta = self.read_meta(session_id);
        let active_seg = meta
            .as_ref()
            .and_then(|m| m.segments.iter().find(|s| s.segment == m.segment));
        let start_id = active_seg.map(|s| s.start_id).unwrap_or(1);

        let mut lines: Vec<&str> = content.split('\n').collect();
        // Remove trailing empty line if present
        if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        // message_id is a global ID; convert to segment-relative line index.
        if message_id < start_id {
            return Ok(false);
        }
        let idx = (message_id - start_id) as usize;
        if idx >= lines.len() {
            return Ok(false);
        }
        lines.remove(idx);

        let new_content = if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n") + "\n"
        };
        fs::write(&path, new_content)?;

        if let Some(mut meta) = self.read_meta(session_id) {
            meta.last_activity = Utc::now();
            // Decrement active segment count.
            if let Some(seg) = meta.segments.iter_mut().find(|s| s.segment == meta.segment) {
                seg.count = seg.count.saturating_sub(1);
            }
            let _ = self.write_meta(&meta);
        }

        Ok(true)
    }

    fn update_message(
        &self,
        session_id: &str,
        message_id: i64,
        message: &ChatMessage,
    ) -> std::io::Result<bool> {
        let path = self.history_path(session_id);
        let Ok(content) = fs::read_to_string(&path) else {
            return Ok(false);
        };

        // Compute the line index relative to the active segment's start_id.
        let meta = self.read_meta(session_id);
        let active_seg = meta
            .as_ref()
            .and_then(|m| m.segments.iter().find(|s| s.segment == m.segment));
        let start_id = active_seg.map(|s| s.start_id).unwrap_or(1);

        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        if message_id < start_id {
            return Ok(false);
        }
        let idx = (message_id - start_id) as usize;
        if idx >= lines.len() {
            return Ok(false);
        }
        let externalized = self.externalize(session_id, message)?;
        let json = serde_json::to_string(&externalized).map_err(std::io::Error::other)?;
        lines[idx] = json;

        fs::write(&path, lines.join("\n") + "\n")?;

        if let Some(mut meta) = self.read_meta(session_id) {
            meta.last_activity = Utc::now();
            let _ = self.write_meta(&meta);
        }

        Ok(true)
    }

    fn truncate_messages(&self, session_id: &str, keep_count: usize) -> std::io::Result<()> {
        let path = self.history_path(session_id);
        let Ok(content) = fs::read_to_string(&path) else {
            return Ok(());
        };
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        if keep_count >= lines.len() {
            return Ok(()); // nothing to truncate
        }
        let kept: Vec<&str> = lines.into_iter().take(keep_count).collect();
        let new_content = if keep_count == 0 {
            String::new()
        } else {
            kept.join("\n") + "\n"
        };
        fs::write(&path, new_content)?;

        // Mark-and-sweep blob GC over the surviving (kept) lines + archives.
        let mut live_hashes: HashSet<String> = HashSet::new();
        for line in &kept {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(line) {
                collect_blob_hashes(&msg, &mut live_hashes);
            }
        }
        self.extend_archived_live_sets(session_id, &mut live_hashes);
        self.sweep_blobs(session_id, &live_hashes);

        if let Some(mut meta) = self.read_meta(session_id) {
            meta.last_activity = Utc::now();
            // Update active segment count.
            if let Some(seg) = meta.segments.iter_mut().find(|s| s.segment == meta.segment) {
                seg.count = keep_count;
            }
            // Adjust global message_count: archived segments' total + keep_count.
            let archived_total: usize = meta
                .segments
                .iter()
                .filter(|s| s.segment != meta.segment)
                .map(|s| s.count)
                .sum();
            meta.message_count = archived_total + keep_count;
            let _ = self.write_meta(&meta);
        }
        Ok(())
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
        // Pull the actual summary text from the active segment's compaction entry.
        let active_seg = meta.segments.iter().find(|s| s.segment == meta.segment);
        let summary_text = active_seg
            .and_then(|s| s.compactions.last())
            .map(|c| c.text.clone())
            .unwrap_or_default();
        Some(SummaryRecord {
            id: 0,
            version: meta.compact_version,
            summary: summary_text,
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

    fn save_session_file(
        &self,
        session_id: &str,
        preferred_name: Option<&str>,
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> std::io::Result<SavedSessionFile> {
        let file_name = crate::storage::session_file_name(preferred_name, bytes, mime_type);
        crate::storage::write_session_file(&self.root, session_id, &file_name, bytes, mime_type)
    }

    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize> {
        let cutoff = Utc::now() - chrono::Duration::hours(ttl_hours as i64);
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Ok(0);
        };

        let mut count = 0;
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            if let Some(meta) = Self::read_meta_at(&entry.path()) {
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

    fn save_session_override(&self, session_id: &str, json: &str) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.session_override = if json.is_empty() {
                None
            } else {
                Some(json.to_string())
            };
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_session_override(&self, session_id: &str) -> Option<String> {
        self.read_meta(session_id)?.session_override
    }

    fn save_last_message(
        &self,
        session_id: &str,
        msg: &crate::api::message::PersistedChannelMessage,
    ) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.last_message = Some(msg.clone());
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_last_message(
        &self,
        session_id: &str,
    ) -> Option<crate::api::message::PersistedChannelMessage> {
        self.read_meta(session_id)?.last_message
    }

    fn save_suspension(&self, session_id: &str, json: &str) -> std::io::Result<()> {
        let path = self.suspension_path(session_id);
        if json.is_empty() {
            if path.exists() {
                fs::remove_file(&path)?;
            }
            return Ok(());
        }
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir)?;
        // Atomic write (temp + rename), mirroring write_json_atomic.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, json.as_bytes())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn load_suspension(&self, session_id: &str) -> Option<String> {
        fs::read_to_string(self.suspension_path(session_id)).ok()
    }

    fn save_agent_name(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.agent_name = if name.is_empty() || name == "main" {
                None
            } else {
                Some(name.to_string())
            };
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_agent_name(&self, session_id: &str) -> Option<String> {
        self.read_meta(session_id)?.agent_name
    }

    fn save_parent_session_id(&self, session_id: &str, parent: &str) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.parent_session_id = if parent.is_empty() {
                None
            } else {
                Some(parent.to_string())
            };
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_parent_session_id(&self, session_id: &str) -> Option<String> {
        self.read_meta(session_id)?.parent_session_id
    }

    fn save_delegation_args(
        &self,
        session_id: &str,
        timeout_secs: u64,
        allowed_tools: Option<Vec<String>>,
    ) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.delegation_timeout_secs = Some(timeout_secs);
            meta.delegation_allowed_tools = allowed_tools;
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_delegation_args(
        &self,
        session_id: &str,
    ) -> Option<(u64, Option<Vec<String>>)> {
        let meta = self.read_meta(session_id)?;
        let timeout = meta.delegation_timeout_secs?;
        Some((timeout, meta.delegation_allowed_tools))
    }

    fn save_delegation_checkpoint(
        &self,
        checkpoint: &crate::storage::DelegationCheckpoint,
    ) -> std::io::Result<()> {
        let path = self.delegation_checkpoint_path(&checkpoint.sub_session_id);
        fs::create_dir_all(path.parent().unwrap_or(&self.root))?;
        Self::write_json_atomic(&path, checkpoint)
    }

    fn delete_delegation_checkpoint(&self, sub_session_id: &str) -> std::io::Result<()> {
        let path = self.delegation_checkpoint_path(sub_session_id);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn load_delegation_checkpoint(&self, sub_session_id: &str) -> Option<crate::storage::DelegationCheckpoint> {
        let path = self.delegation_checkpoint_path(sub_session_id);
        if !path.exists() {
            return None;
        }
        let bytes = fs::read(&path).ok()?;
        match serde_json::from_slice::<crate::storage::DelegationCheckpoint>(&bytes) {
            Ok(cp) => Some(cp),
            Err(e) => {
                tracing::warn!(
                    path = ?path,
                    err = %e,
                    "load delegation checkpoint: corrupt entry, ignoring"
                );
                None
            }
        }
    }

    fn update_delegation_checkpoint_status(
        &self,
        sub_session_id: &str,
        status: &str,
    ) -> std::io::Result<()> {
        let Some(mut cp) = self.load_delegation_checkpoint(sub_session_id) else {
            // No checkpoint (e.g. the crash happened before spawn finished) —
            // nothing to tombstone; the recovery path treats missing
            // checkpoints as crash remnants and resumes them.
            return Ok(());
        };
        cp.status = status.to_string();
        self.save_delegation_checkpoint(&cp)
    }

    fn load_delegation_checkpoints(&self) -> Vec<crate::storage::DelegationCheckpoint> {
        // P1: checkpoints live inside each sub-session dir (`delegation.json`),
        // not a shared `delegations/` dir. Scan session dirs.
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let cp_path = entry.path().join("delegation.json");
            let Ok(bytes) = fs::read(&cp_path) else {
                continue;
            };
            match serde_json::from_slice::<crate::storage::DelegationCheckpoint>(&bytes) {
                Ok(cp) => out.push(cp),
                Err(e) => {
                    tracing::warn!(
                        path = ?entry.path(),
                        err = %e,
                        "delegation checkpoint parse failed; skipping"
                    );
                }
            }
        }
        out
    }

    fn list_sessions_for_owner(&self, owner: &str) -> Vec<SessionInfo> {
        // Delegate to the existing owner-filtered list_sessions implementation.
        SessionBackend::list_sessions(self, owner)
    }

    fn query_message(&self, session_id: &str, message_id: i64) -> Option<(String, String)> {
        let meta = self.read_meta(session_id)?;

        // Find the segment containing this global message ID.
        let seg = meta.segments.iter().find(|s| {
            message_id >= s.start_id && message_id < s.start_id + s.count as i64
        })?;

        // Determine the file path for this segment.
        let file_path = if seg.segment == meta.segment {
            self.history_path(session_id)
        } else {
            self.archive_dir(session_id)
                .join(format!("history.{:04}.jsonl", seg.segment))
        };

        // Read the specific line by segment-relative index.
        let line_idx = (message_id - seg.start_id) as usize;
        let content = fs::read_to_string(&file_path).ok()?;
        let line = content
            .lines()
            .filter(|l| !l.is_empty())
            .nth(line_idx)?;

        let msg: ChatMessage = serde_json::from_str(line).ok()?;
        let text = msg.text_content();
        let preview: String = if text.chars().count() > 200 {
            text.chars().take(200).collect()
        } else {
            text
        };
        Some((msg.role, preview))
    }
}
