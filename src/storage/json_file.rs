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
//!
//! ## Image blob externalization (multimodal Stage 1)
//!
//! Large inline images bloat `history.jsonl` (base64 is huge and appears on
//! every line that carries the message).  To keep the JSONL compact and
//! deduplicate identical images, `append_message` *externalizes* each large
//! `ContentPart::ImageB64` into a content-addressed blob under
//! `sessions/{session_id}/blobs/{sha256}.bin` (raw decoded bytes) and replaces
//! the part with a `ContentPart::ImageRef { hash, .. }`.  `load_messages` (and
//! every other history read path) *hydrates* `ImageRef` back into `ImageB64`
//! by reading the blob, so the in-memory render path never sees `ImageRef`.
//!
//! Blob lifecycle is mark-and-sweep: on `rotate_history` / `truncate_messages`
//! we collect the set of `ImageRef.hash` referenced by surviving (and archived)
//! messages and delete any `blobs/*.bin` not in that set.

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::providers::capability_chat::sha256_hex;
use crate::providers::ContentPart;
use crate::storage::{ChatMessage, SessionBackend, SessionInfo, SummaryRecord};

/// Inline `ImageB64` parts whose base64 string is at most this many bytes stay
/// inline in `history.jsonl`; larger ones are externalized into a blob.
const INLINE_IMAGE_MAX_B64_LEN: usize = 8 * 1024;

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
    /// Per-session runtime overrides (JSON-encoded SessionOverride).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_override: Option<String>,
    /// Last incoming ChannelMessage. Carries sender / reply_target /
    /// attachments / images so startup recovery can replay the routing
    /// context. RFC v2 §三.A made this the canonical replacement for
    /// the older standalone `last_reply_target` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_message: Option<crate::channels::ChannelMessage>,
    /// Owning agent name. "main" for top-level sessions; sub-agent name for
    /// delegate-spawned sessions. Skipped when absent for forward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    /// Parent session ID for sub-sessions. None for top-level user sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
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
    ///
    /// Every message is hydrated (`ImageRef` → `ImageB64`) before being returned
    /// so callers never observe the disk-only `ImageRef` variant.
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
                let msg = self.hydrate(session_id, &msg);
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

        // Write surviving messages to the new active segment. Re-externalize so
        // large images live as blobs + `ImageRef` again (the `surviving` slice
        // arrives hydrated from the read path). Track both the live blob hashes
        // (for the `*.bin` sweep) and the live description keys (for the `*.txt`
        // sweep) so we can drop orphans afterwards. Description keys are computed
        // from the *hydrated* `msg` (before externalization, while the b64 is
        // present); externalization is fingerprint-invariant so the key is the
        // same either way.
        let mut live_hashes: HashSet<String> = HashSet::new();
        let mut live_desc: HashSet<String> = HashSet::new();
        if !surviving.is_empty() {
            let mut f = fs::File::create(&history_path)?;
            for (_, msg) in surviving {
                collect_description_keys(msg, &mut live_desc);
                let msg = self.externalize(session_id, msg)?;
                collect_blob_hashes(&msg, &mut live_hashes);
                let json = serde_json::to_string(&msg).map_err(std::io::Error::other)?;
                writeln!(f, "{json}")?;
            }
            f.flush()?;
            f.sync_all()?;
        }

        // Archived segments are externalized too; keep their blobs + descriptions
        // alive. One scan fills both live sets.
        self.extend_archived_live_sets(session_id, &mut live_hashes, &mut live_desc);
        // Mark-and-sweep: drop any blob / description not referenced by a live
        // message.
        self.sweep_blobs(session_id, &live_hashes);
        self.sweep_descriptions(session_id, &live_desc);

        // Line numbers restart at 1; update the counter to match the new file.
        meta.message_count = surviving.len();
        meta.segment += 1;
        self.write_meta(&meta)?;
        Ok(())
    }

    // ── Image blob store ──────────────────────────────────────────────────────

    fn blobs_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("blobs")
    }

    fn blob_path(&self, session_id: &str, hash: &str) -> PathBuf {
        self.blobs_dir(session_id).join(format!("{hash}.bin"))
    }

    /// Write `bytes` to `blobs/{hash}.bin`. Content-addressed and idempotent:
    /// if the blob already exists the write is skipped (dedup).
    fn write_blob(&self, session_id: &str, hash: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.blob_path(session_id, hash);
        if path.exists() {
            return Ok(()); // already present — content-addressed dedup
        }
        let dir = self.blobs_dir(session_id);
        fs::create_dir_all(&dir)?;
        // Atomic: write to a temp file then rename so a partial write can never
        // masquerade as a complete (and wrongly-hashed) blob.
        let tmp = path.with_extension("bin.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.flush()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Read the raw decoded bytes of `blobs/{hash}.bin`.
    fn read_blob(&self, session_id: &str, hash: &str) -> std::io::Result<Vec<u8>> {
        fs::read(self.blob_path(session_id, hash))
    }

    /// Externalize large inline images in `message` before serialization.
    ///
    /// For each `ImageB64` whose base64 length exceeds `INLINE_IMAGE_MAX_B64_LEN`,
    /// decode the bytes, hash them (sha256), write the blob, and replace the part
    /// with `ImageRef { hash, media_type, detail }`. Small images stay inline.
    /// Returns an externalized clone (the input is never mutated).
    fn externalize(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<ChatMessage> {
        // Fast path: nothing to externalize.
        let needs = message.parts.iter().any(|p| matches!(
            p,
            ContentPart::ImageB64 { b64_json, .. } if b64_json.len() > INLINE_IMAGE_MAX_B64_LEN
        ));
        if !needs {
            return Ok(message.clone());
        }

        let mut out = message.clone();
        for part in &mut out.parts {
            if let ContentPart::ImageB64 { b64_json, media_type, detail } = part {
                if b64_json.len() <= INLINE_IMAGE_MAX_B64_LEN {
                    continue; // small image stays inline
                }
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64_json.as_bytes())
                else {
                    // Undecodable base64: leave inline rather than lose data.
                    continue;
                };
                let hash = sha256_hex(&bytes);
                self.write_blob(session_id, &hash, &bytes)?;
                *part = ContentPart::ImageRef {
                    hash,
                    media_type: media_type.take(),
                    detail: *detail,
                };
            }
        }
        Ok(out)
    }

    /// Hydrate externalized images in `message` after deserialization.
    ///
    /// For each `ImageRef`, read the blob and replace it with an `ImageB64`
    /// carrying the re-encoded base64. A missing or corrupt blob degrades to a
    /// `Text { text: "[image unavailable]" }` placeholder so a single lost blob
    /// never fails the whole history load. Returns a hydrated clone.
    fn hydrate(&self, session_id: &str, message: &ChatMessage) -> ChatMessage {
        if !message.parts.iter().any(|p| matches!(p, ContentPart::ImageRef { .. })) {
            return message.clone();
        }
        let mut out = message.clone();
        for part in &mut out.parts {
            if let ContentPart::ImageRef { hash, media_type, detail } = part {
                match self.read_blob(session_id, hash) {
                    Ok(bytes) => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        *part = ContentPart::ImageB64 {
                            b64_json: b64,
                            media_type: media_type.take(),
                            detail: *detail,
                        };
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id, hash = %hash, err = %e,
                            "image blob missing/corrupt; degrading to placeholder"
                        );
                        *part = ContentPart::Text { text: "[image unavailable]".into() };
                    }
                }
            }
        }
        out
    }

    /// Mark-and-sweep blob GC: delete any `blobs/*.bin` whose hash is not
    /// referenced by a `live` message. `live` should include both surviving
    /// active messages and any archived segments that are also externalized.
    fn sweep_blobs(&self, session_id: &str, live: &HashSet<String>) {
        let dir = self.blobs_dir(session_id);
        let Ok(entries) = fs::read_dir(&dir) else { return; };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(hash) = name.strip_suffix(".bin") else { continue; };
            if !live.contains(hash) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Mark-and-sweep description GC: delete any `descriptions/*.txt` whose key
    /// is not in `live` (the content fingerprints of all live media). Mirrors
    /// [`sweep_blobs`]; `*.tmp` write-through scratch files are ignored (they do
    /// not end in `.txt`). The descriptions dir is written by
    /// `PersistentDescriptionCache`; this is the in-session reclamation point for
    /// images dropped by compaction (full reclamation happens on session delete).
    fn sweep_descriptions(&self, session_id: &str, live: &HashSet<String>) {
        let dir = self.session_dir(session_id).join("descriptions");
        let Ok(entries) = fs::read_dir(&dir) else { return; };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(key) = name.strip_suffix(".txt") else { continue; };
            if !live.contains(key) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Scan the archive segments once, extending both live sets: `blob_hashes`
    /// with every `ImageRef.hash` (for the `*.bin` sweep) and `desc_keys` with
    /// every media part's content fingerprint (for the `*.txt` sweep). Archived
    /// segments are externalized the same way as the active segment, so their
    /// referenced blobs and descriptions must be kept alive too.
    fn extend_archived_live_sets(
        &self,
        session_id: &str,
        blob_hashes: &mut HashSet<String>,
        desc_keys: &mut HashSet<String>,
    ) {
        let archive_dir = self.archive_dir(session_id);
        let Ok(entries) = fs::read_dir(&archive_dir) else { return; };
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(f) = fs::File::open(entry.path()) else { continue; };
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let line = line.trim();
                if line.is_empty() { continue; }
                let Ok(msg) = serde_json::from_str::<ChatMessage>(line) else { continue; };
                collect_blob_hashes(&msg, blob_hashes);
                collect_description_keys(&msg, desc_keys);
            }
        }
    }
}

/// Append every `ImageRef.hash` in `msg` into `set` — the live set for the
/// blob (`*.bin`) sweep. Only externalized images have a blob, so inline
/// `ImageB64` parts are intentionally excluded here.
fn collect_blob_hashes(msg: &ChatMessage, set: &mut HashSet<String>) {
    for part in &msg.parts {
        if let ContentPart::ImageRef { hash, .. } = part {
            set.insert(hash.clone());
        }
    }
}

/// Append the content fingerprint of every media part in `msg` into `set` — the
/// live set for the description (`*.txt`) sweep. Unlike [`collect_blob_hashes`]
/// this covers *all* live images (inline `ImageB64`, externalized `ImageRef`,
/// and `ImageUrl`), since a small inline image still has a cached description
/// that must not be swept. `content_fingerprint` yields the same key for an
/// image whether it is inline or externalized, so this is a superset of the
/// blob-hash set.
fn collect_description_keys(msg: &ChatMessage, set: &mut HashSet<String>) {
    for part in &msg.parts {
        if let Some(key) = part.content_fingerprint() {
            set.insert(key);
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
            segment: 0,
            compact_version: 0,
            compact_token_estimate: None,
            last_total_tokens: None,
            session_override: None,
            last_message: None,
            agent_name: None,
            parent_session_id: None,
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

    fn list_all_sessions(&self) -> Vec<SessionInfo> {
        let Ok(entries) = fs::read_dir(&self.root) else { return vec![]; };
        let mut sessions: Vec<SessionInfo> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| {
                let id = e.file_name().to_string_lossy().to_string();
                self.read_meta(&id)
            })
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

        // Externalize large inline images into content-addressed blobs and
        // persist the lightweight `ImageRef` form instead.
        let message = self.externalize(session_id, message)?;
        let json = serde_json::to_string(&message).map_err(std::io::Error::other)?;
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

    fn truncate_messages(&self, session_id: &str, keep_count: usize) -> std::io::Result<()> {
        let path = self.history_path(session_id);
        let content = fs::read_to_string(&path)?;
        let lines: Vec<&str> = content.lines().collect();
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

        // Mark-and-sweep blob + description GC over the surviving (kept) lines +
        // archives. The kept lines are the on-disk (externalized) form, so
        // `content_fingerprint` reads `ImageRef.hash` directly and decodes any
        // inline `ImageB64` — yielding the same description keys either way.
        let mut live_hashes: HashSet<String> = HashSet::new();
        let mut live_desc: HashSet<String> = HashSet::new();
        for line in &kept {
            let line = line.trim();
            if line.is_empty() { continue; }
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(line) {
                collect_blob_hashes(&msg, &mut live_hashes);
                collect_description_keys(&msg, &mut live_desc);
            }
        }
        self.extend_archived_live_sets(session_id, &mut live_hashes, &mut live_desc);
        self.sweep_blobs(session_id, &live_hashes);
        self.sweep_descriptions(session_id, &live_desc);

        if let Some(mut meta) = self.read_meta(session_id) {
            meta.message_count = keep_count;
            meta.last_activity = Utc::now();
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

    fn save_session_override(&self, session_id: &str, json: &str) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.session_override = if json.is_empty() { None } else { Some(json.to_string()) };
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
        msg: &crate::channels::ChannelMessage,
    ) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.last_message = Some(msg.clone());
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_last_message(&self, session_id: &str) -> Option<crate::channels::ChannelMessage> {
        self.read_meta(session_id)?.last_message
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ChatMessage, ContentPart, ImageDetail};

    fn b64_of(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// Build a JsonFileBackend rooted in a fresh temp dir with one session.
    fn backend_with_session() -> (tempfile::TempDir, JsonFileBackend, String) {
        let dir = tempfile::tempdir().unwrap();
        let backend = JsonFileBackend::open(dir.path()).unwrap();
        let info = backend.create_session("owner", None).unwrap();
        (dir, backend, info.id)
    }

    fn img_msg(b64: String) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            parts: vec![
                ContentPart::ImageB64 { b64_json: b64, media_type: Some("image/png".into()), detail: ImageDetail::Auto },
                ContentPart::Text { text: "hello".into() },
            ],
            name: None, tool_call_id: None, tool_calls: None, is_error: None,
        }
    }

    #[test]
    fn blob_roundtrip_externalize_hydrate() {
        let (_dir, backend, sid) = backend_with_session();
        // A large image (> threshold) round-trips through blob + hydrate.
        let raw = vec![7u8; 16 * 1024];
        let b64 = b64_of(&raw);
        backend.append_message(&sid, &img_msg(b64.clone())).unwrap();

        // On-disk line must NOT contain the base64 payload (externalized).
        let line = fs::read_to_string(backend.history_path(&sid)).unwrap();
        assert!(line.contains("image_ref"), "expected ImageRef on disk: {line}");
        assert!(!line.contains(&b64), "base64 must not be inline on disk");

        // Loading hydrates back to the original ImageB64.
        let loaded = backend.load_messages(&sid);
        assert_eq!(loaded.len(), 1);
        match &loaded[0].parts[0] {
            ContentPart::ImageB64 { b64_json, media_type, .. } => {
                assert_eq!(b64_json, &b64);
                assert_eq!(media_type.as_deref(), Some("image/png"));
            }
            other => panic!("expected hydrated ImageB64, got {other:?}"),
        }
    }

    #[test]
    fn small_image_stays_inline() {
        let (_dir, backend, sid) = backend_with_session();
        let raw = vec![1u8; 64]; // tiny -> base64 well under 8KB
        let b64 = b64_of(&raw);
        backend.append_message(&sid, &img_msg(b64.clone())).unwrap();

        let line = fs::read_to_string(backend.history_path(&sid)).unwrap();
        assert!(line.contains("image_b64"), "small image should stay inline: {line}");
        assert!(!line.contains("image_ref"));
        // No blob written.
        assert!(!backend.blobs_dir(&sid).exists() ||
            fs::read_dir(backend.blobs_dir(&sid)).map(|mut e| e.next().is_none()).unwrap_or(true));
    }

    #[test]
    fn dedup_writes_single_blob() {
        let (_dir, backend, sid) = backend_with_session();
        let raw = vec![9u8; 20 * 1024];
        let b64 = b64_of(&raw);
        // Append the same large image twice.
        backend.append_message(&sid, &img_msg(b64.clone())).unwrap();
        backend.append_message(&sid, &img_msg(b64.clone())).unwrap();

        let blobs: Vec<_> = fs::read_dir(backend.blobs_dir(&sid)).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "bin").unwrap_or(false))
            .collect();
        assert_eq!(blobs.len(), 1, "identical images must dedup to one blob");
    }

    #[test]
    fn gc_sweeps_orphan_blobs_on_truncate() {
        let (_dir, backend, sid) = backend_with_session();
        let raw_a = vec![2u8; 20 * 1024];
        let raw_b = vec![3u8; 20 * 1024];
        backend.append_message(&sid, &img_msg(b64_of(&raw_a))).unwrap();
        backend.append_message(&sid, &img_msg(b64_of(&raw_b))).unwrap();

        let count_blobs = |b: &JsonFileBackend| fs::read_dir(b.blobs_dir(&sid))
            .map(|e| e.filter_map(|x| x.ok())
                .filter(|x| x.path().extension().map(|y| y == "bin").unwrap_or(false))
                .count())
            .unwrap_or(0);
        assert_eq!(count_blobs(&backend), 2);

        // Keep only the first message; the second image's blob is orphaned.
        backend.truncate_messages(&sid, 1).unwrap();
        assert_eq!(count_blobs(&backend), 1, "orphan blob should be swept");
    }

    #[test]
    fn gc_sweeps_orphan_descriptions_on_truncate() {
        let (_dir, backend, sid) = backend_with_session();
        let raw_a = vec![2u8; 20 * 1024];
        let raw_b = vec![3u8; 20 * 1024];
        backend.append_message(&sid, &img_msg(b64_of(&raw_a))).unwrap();
        backend.append_message(&sid, &img_msg(b64_of(&raw_b))).unwrap();

        // Simulate PersistentDescriptionCache having persisted a description for
        // each image, keyed by content fingerprint (== decoded-bytes sha256 ==
        // blob hash, so it matches the on-disk ImageRef the sweep sees).
        let desc_dir = backend.session_dir(&sid).join("descriptions");
        fs::create_dir_all(&desc_dir).unwrap();
        let key_a = sha256_hex(&raw_a);
        let key_b = sha256_hex(&raw_b);
        fs::write(desc_dir.join(format!("{key_a}.txt")), "desc a").unwrap();
        fs::write(desc_dir.join(format!("{key_b}.txt")), "desc b").unwrap();

        // Keep only the first message; image B's description is now orphaned.
        backend.truncate_messages(&sid, 1).unwrap();

        assert!(
            desc_dir.join(format!("{key_a}.txt")).exists(),
            "description of a live image must be kept"
        );
        assert!(
            !desc_dir.join(format!("{key_b}.txt")).exists(),
            "description of a dropped image must be swept"
        );
    }

    #[test]
    fn missing_blob_degrades_to_placeholder() {
        let (_dir, backend, sid) = backend_with_session();
        let raw = vec![5u8; 20 * 1024];
        backend.append_message(&sid, &img_msg(b64_of(&raw))).unwrap();

        // Delete every blob to simulate corruption/loss.
        for e in fs::read_dir(backend.blobs_dir(&sid)).unwrap().filter_map(|e| e.ok()) {
            fs::remove_file(e.path()).unwrap();
        }

        let loaded = backend.load_messages(&sid);
        assert_eq!(loaded.len(), 1);
        match &loaded[0].parts[0] {
            ContentPart::Text { text } => assert_eq!(text, "[image unavailable]"),
            other => panic!("expected placeholder text, got {other:?}"),
        }
        // The trailing text part must survive too.
        assert!(matches!(&loaded[0].parts[1], ContentPart::Text { text } if text == "hello"));
    }
}
