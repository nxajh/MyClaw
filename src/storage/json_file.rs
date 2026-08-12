//! JSON-file-backed session storage.
//!
//! Directory layout (under `{workspace_dir}/sessions/`):
//!
//! ```text
//! sessions/
//!   active.json              # { "user_id": "session_id", ... }
//!   {dir_name(session_id)}/  # `/` → `_`、`_` → `__`（可逆，见 ids::dir_name）
//!     meta.json              # all session metadata (identity, counters, compaction state)
//!     history.jsonl          # active segment: one ChatMessage JSON per line, append-only
//!     archive/
//!       history.0000.jsonl   # segments archived on each compaction
//!       history.0001.jsonl
//!       ...
//! ```
//!
//! Message IDs are globally monotonically increasing across all segments.
//! The active segment's `start_id` (stored in `meta.segments`) is the base
//! for line→ID mapping.  Compaction summaries are stored in the segment
//! record rather than as lines in the file.
//!
//! ## Session files (multimodal)
//!
//! New inbound media is stored as session-local files under
//! `sessions/{session_id}/files/`. History stores only path metadata via
//! `ContentPart::File`; base64 is generated only by provider renderers when a
//! wire protocol requires it.
//!

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{dir_name, id_from_dir, Fqid, DEFAULT_NAMESPACE, TYPE_SESSION};
use crate::storage::{ChatMessage, SavedSessionFile, SessionBackend, SessionInfo, SummaryRecord};

// ── On-disk types ─────────────────────────────────────────────────────────────


/// A compaction summary that replaced a range of messages, stored in meta
/// rather than as a line in history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactionEntry {
    /// Position (0-based) among real messages where the summary should be
    /// inserted when reconstructing the in-memory history.
    position: usize,
    /// The full summary text (including the `[CONTEXT COMPACTION...]` prefix).
    text: String,
    /// Compaction version number.
    version: u32,
}

/// Index entry mapping a segment file to its global message ID range.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentRecord {
    /// Segment number (matches archive filename: history.{segment:04}.jsonl).
    /// The active segment uses `meta.segment`.
    segment: u32,
    /// Global ID of the first real message in this segment.
    start_id: i64,
    /// Number of real messages (excluding compaction entries) in this segment.
    count: usize,
    /// Compaction summaries within this segment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    compactions: Vec<CompactionEntry>,
}

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
    /// Last incoming message context. Carries sender / receiver / text so
    /// startup recovery can replay the routing context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_message: Option<crate::channels::PersistedChannelMessage>,
    /// Optional generated summary of this session's purpose/outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// Owning agent name. "main" for top-level sessions; sub-agent name for
    /// delegate-spawned sessions. Skipped when absent for forward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    /// Parent session ID for sub-sessions. None for top-level user sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    /// Sub-agent task FQID (`<ns>/t/<uuidv7>`) for sub-sessions (P1-1:
    /// restart recovery needs the same id the parent's suspension recorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delegation_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delegation_allowed_tools: Option<Vec<String>>,
    /// Global segment index for message ID → file lookup.
    /// Empty for legacy sessions (pre-migration); populated on first rotate or migration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    segments: Vec<SegmentRecord>,
}

/// A compaction summary that replaced a range of messages, stored in meta
/// rather than as a line in history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactionEntry {
    /// Position (0-based) among real messages where the summary should be
    /// inserted when reconstructing the in-memory history.
    position: usize,
    /// The full summary text (including the `[CONTEXT COMPACTION...]` prefix).
    text: String,
    /// Compaction version number.
    version: u32,
}

/// Index entry mapping a segment file to its global message ID range.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentRecord {
    /// Segment number (matches archive filename: history.{segment:04}.jsonl).
    /// The active segment uses `meta.segment`.
    segment: u32,
    /// Global ID of the first real message in this segment.
    start_id: i64,
    /// Number of real messages (excluding compaction entries) in this segment.
    count: usize,
    /// Compaction summaries within this segment. Usually 0 or 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    compactions: Vec<CompactionEntry>,
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
    namespace: String,
}

impl JsonFileBackend {
    /// Open (or create) the sessions directory at `root` (default namespace).
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::open_with_namespace(root, DEFAULT_NAMESPACE)
    }

    /// Open (or create) the sessions directory at `root`, generating session
    /// FQIDs (`<ns>/s/<uuidv7>`) under `namespace`.
    pub fn open_with_namespace(
        root: impl Into<PathBuf>,
        namespace: &str,
    ) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            namespace: namespace.to_string(),
        })
    }

    // ── Paths ─────────────────────────────────────────────────────────────────

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(dir_name(session_id))
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

    /// 方案 C (RFC §5): turn-suspension state, independent of meta.json.
    fn suspension_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("suspension.json")
    }

    /// Durable delegation checkpoint file: `delegations/<task_id>.json`.
    fn delegation_checkpoint_path(&self, task_id: &str) -> PathBuf {
        self.root.join("delegations").join(format!("{}.json", dir_name(task_id)))
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

    fn generate_session_id(&self) -> String {
        Fqid::new(&self.namespace, TYPE_SESSION).to_string()
    }

    // ── JSONL helpers ─────────────────────────────────────────────────────────

    /// Read all (id, ChatMessage) pairs from the active history.jsonl.
    /// IDs are globally monotonic across all segments (segment.start_id + offset).
    ///
    /// Compaction summaries (stored in the segment record, not in the file) are
    /// inserted at their recorded positions with id=0.
    fn read_history_with_ids(&self, session_id: &str) -> Vec<(i64, ChatMessage)> {
        let meta = match self.read_meta(session_id) {
            Some(m) => m,
            None => return vec![],
        };

        // Find the active segment record for global ID base.
        let active_seg = meta.segments.iter().find(|s| s.segment == meta.segment);
        let start_id = active_seg.map(|s| s.start_id).unwrap_or(1);

        let path = self.history_path(session_id);
        let Ok(f) = fs::File::open(&path) else {
            // File missing — still insert compaction entries if any.
            let mut result: Vec<(i64, ChatMessage)> = vec![];
            if let Some(seg) = active_seg {
                for comp in &seg.compactions {
                    if comp.position <= result.len() {
                        let summary_msg = ChatMessage::user_text(comp.text.clone());
                        result.insert(comp.position, (0, summary_msg));
                    }
                }
            }
            return result;
        };

        let mut result: Vec<(i64, ChatMessage)> = BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| {
                let msg: ChatMessage = serde_json::from_str(&line).ok()?;
                Some(self.hydrate(session_id, &msg))
            })
            .enumerate()
            .map(|(i, msg)| (start_id + i as i64, msg))
            .collect();

        // Insert compaction summaries at their recorded positions.
        if let Some(seg) = active_seg {
            for comp in &seg.compactions {
                if comp.position <= result.len() {
                    let summary_msg = ChatMessage::user_text(comp.text.clone());
                    result.insert(comp.position, (0, summary_msg));
                }
            }
        }

        result
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

        let current_segment = meta.segment;

        // Count real lines in the current file before archiving.
        let archived_count = if history_path.exists() {
            fs::read_to_string(&history_path)
                .map(|c| c.lines().filter(|l| !l.is_empty()).count())
                .unwrap_or(0)
        } else {
            0
        };

        // Compute the archived segment's start_id from existing records.
        let archived_start_id = meta
            .segments
            .iter()
            .find(|s| s.segment == current_segment)
            .map(|s| s.start_id)
            .unwrap_or_else(|| {
                let mut id = 1i64;
                for rec in meta.segments.iter().filter(|s| s.segment < current_segment) {
                    id = rec.start_id + rec.count as i64;
                }
                id
            });

        // Archive the current active segment.
        if history_path.exists() {
            let archive_dir = self.archive_dir(session_id);
            fs::create_dir_all(&archive_dir)?;
            let archive_name = format!("history.{:04}.jsonl", current_segment);
            fs::rename(&history_path, archive_dir.join(archive_name))?;
        }

        // Build archived segment record.
        let archived_record = SegmentRecord {
            segment: current_segment,
            start_id: archived_start_id,
            count: archived_count,
            compactions: Vec::new(),
        };

        // Write only real messages (id != 0) to the new active segment, and
        // collect compaction summaries (id == 0) into the new segment record.
        let mut live_hashes: HashSet<String> = HashSet::new();
        let mut real_count = 0usize;
        let mut new_compactions: Vec<CompactionEntry> = Vec::new();
        let mut new_start_id = archived_start_id + archived_count as i64;

        if !surviving.is_empty() {
            let mut f = fs::File::create(&history_path)?;
            for &(id, ref msg) in surviving {
                if id == 0 {
                    // Compaction summary — record its position, don't write to file.
                    new_compactions.push(CompactionEntry {
                        position: real_count,
                        text: msg.text_content(),
                        version: meta.compact_version,
                    });
                } else {
                    let externalized = self.externalize(session_id, msg)?;
                    collect_blob_hashes(&externalized, &mut live_hashes);
                    let json =
                        serde_json::to_string(&externalized).map_err(std::io::Error::other)?;
                    writeln!(f, "{json}")?;
                    if real_count == 0 {
                        new_start_id = id;
                    }
                    real_count += 1;
                }
            }
            f.flush()?;
            f.sync_all()?;
        }

        // Archived segments are externalized too; keep their blobs alive.
        self.extend_archived_live_sets(session_id, &mut live_hashes);
        // Mark-and-sweep: drop any blob not referenced by a live message.
        self.sweep_blobs(session_id, &live_hashes);

        // Build the new active segment record.
        let new_segment = current_segment + 1;
        let new_active_record = SegmentRecord {
            segment: new_segment,
            start_id: new_start_id,
            count: real_count,
            compactions: new_compactions,
        };

        // Update segments: remove stale records for the affected segment numbers.
        meta.segments
            .retain(|s| s.segment != current_segment && s.segment != new_segment);
        meta.segments.push(archived_record);
        meta.segments.push(new_active_record);

        // message_count stays global — do NOT reset to surviving.len().
        meta.segment = new_segment;
        self.write_meta(&meta)?;
        Ok(())
    }

    // ── Image blob store ──────────────────────────────────────────────────────

    fn blobs_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("blobs")
    }

    /// Path-only content no longer needs externalization before serialization.
    fn externalize(
        &self,
        _session_id: &str,
        message: &ChatMessage,
    ) -> std::io::Result<ChatMessage> {
        Ok(message.clone())
    }

    /// Path-only content no longer needs hydration after deserialization.
    fn hydrate(&self, _session_id: &str, message: &ChatMessage) -> ChatMessage {
        message.clone()
    }

    /// Mark-and-sweep legacy blob GC: delete any `blobs/*.bin` whose hash is not
    /// referenced by a `live` message. `live` should include both surviving
    /// active messages and any archived segments that are also externalized.
    fn sweep_blobs(&self, session_id: &str, live: &HashSet<String>) {
        let dir = self.blobs_dir(session_id);
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(hash) = name.strip_suffix(".bin") else {
                continue;
            };
            if !live.contains(hash) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Scan archive segments once, extending `blob_hashes` with legacy blob refs.
    /// Path-only histories add nothing here.
    fn extend_archived_live_sets(&self, session_id: &str, blob_hashes: &mut HashSet<String>) {
        let archive_dir = self.archive_dir(session_id);
        let Ok(entries) = fs::read_dir(&archive_dir) else {
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(f) = fs::File::open(entry.path()) else {
                continue;
            };
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<ChatMessage>(line) else {
                    continue;
                };
                collect_blob_hashes(&msg, blob_hashes);
            }
        }
    }

    /// Process a single segment file for migration.
    /// Reads lines, identifies compaction summaries, rewrites the file without them,
    /// and returns a SegmentRecord + the next global ID.
    fn migrate_segment_file(
        &self,
        path: &Path,
        segment: u32,
        start_id: i64,
    ) -> std::io::Result<(SegmentRecord, i64)> {
        let content = fs::read_to_string(path)?;
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();

        let mut real_messages: Vec<String> = Vec::new();
        let mut compactions: Vec<CompactionEntry> = Vec::new();
        let mut real_idx: usize = 0;

        for line in &lines {
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(line) {
                let text = msg.text_content();
                if msg.role == "user" && text.starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]") {
                    compactions.push(CompactionEntry {
                        position: real_idx,
                        text,
                        version: 0,
                    });
                    continue; // Don't write to file
                }
            }
            real_messages.push((*line).to_string());
            real_idx += 1;
        }

        // Rewrite file without compaction lines.
        if real_messages.is_empty() {
            fs::write(path, "")?;
        } else {
            let new_content = real_messages.join("\n") + "\n";
            fs::write(path, new_content)?;
        }

        let count = real_messages.len();
        let next_id = start_id + count as i64;

        Ok((SegmentRecord {
            segment,
            start_id,
            count,
            compactions,
        }, next_id))
    }

    /// One-time migration: convert legacy per-segment IDs to global IDs.
    /// Idempotent — skips sessions that already have a non-empty `segments` field.
    pub fn migrate_global_message_ids(&self) -> std::io::Result<usize> {
        let mut migrated = 0;
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Ok(0);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let meta_path = path.join("meta.json");
            if !meta_path.exists() {
                continue;
            }

            let Ok(bytes) = fs::read(&meta_path) else {
                continue;
            };
            let mut meta: SessionMeta = match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Skip if already migrated.
            if !meta.segments.is_empty() {
                continue;
            }

            let dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let _session_id = id_from_dir(dir_name);

            // Walk segments: archive 0000, 0001, ..., then active.
            let mut segments: Vec<SegmentRecord> = Vec::new();
            let mut global_id: i64 = 1;

            let archive_dir = path.join("archive");
            if archive_dir.exists() {
                let mut archive_files: Vec<(u32, PathBuf)> = fs::read_dir(&archive_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        // Parse "history.NNNN.jsonl"
                        if name.starts_with("history.") && name.ends_with(".jsonl") {
                            let num_str = &name[8..name.len() - 6];
                            num_str.parse::<u32>().ok().map(|n| (n, e.path()))
                        } else {
                            None
                        }
                    })
                    .collect();
                archive_files.sort_by_key(|(n, _)| *n);

                for (seg_num, file_path) in archive_files {
                    let (seg_record, next_id) =
                        self.migrate_segment_file(&file_path, seg_num, global_id)?;
                    global_id = next_id;
                    segments.push(seg_record);
                }
            }

            // Active segment.
            let active_path = path.join("history.jsonl");
            if active_path.exists() {
                let (seg_record, next_id) =
                    self.migrate_segment_file(&active_path, meta.segment, global_id)?;
                global_id = next_id;
                segments.push(seg_record);
            }

            meta.segments = segments;
            meta.message_count = (global_id - 1).max(0) as usize;

            Self::write_json_atomic(&meta_path, &meta)?;
            migrated += 1;
        }
        Ok(migrated)
    }
}

/// Path-only history has no inline media blobs to track.
fn collect_blob_hashes(_msg: &ChatMessage, _set: &mut HashSet<String>) {}

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
            task_id: None,
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
            .filter_map(|e| {
                let id = id_from_dir(&e.file_name().to_string_lossy());
                self.read_meta(&id)
            })
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
            .filter_map(|e| {
                let id = id_from_dir(&e.file_name().to_string_lossy());
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
            let id = id_from_dir(&entry.file_name().to_string_lossy());
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
        msg: &crate::channels::PersistedChannelMessage,
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
    ) -> Option<crate::channels::PersistedChannelMessage> {
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

    fn save_task_id(&self, session_id: &str, task_id: &str) -> std::io::Result<()> {
        if let Some(mut meta) = self.read_meta(session_id) {
            meta.task_id = if task_id.is_empty() {
                None
            } else {
                Some(task_id.to_string())
            };
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    fn load_task_id(&self, session_id: &str) -> Option<String> {
        self.read_meta(session_id)?.task_id
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
        let path = self.delegation_checkpoint_path(&checkpoint.task_id);
        fs::create_dir_all(path.parent().unwrap_or(&self.root))?;
        Self::write_json_atomic(&path, checkpoint)
    }

    fn delete_delegation_checkpoint(&self, task_id: &str) -> std::io::Result<()> {
        let path = self.delegation_checkpoint_path(task_id);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn load_delegation_checkpoints(&self) -> Vec<crate::storage::DelegationCheckpoint> {
        let dir = self.root.join("delegations");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(bytes) = fs::read(entry.path()) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ChatMessage, ContentPart};

    fn backend_with_session() -> (tempfile::TempDir, JsonFileBackend, String) {
        let dir = tempfile::tempdir().unwrap();
        let backend = JsonFileBackend::open(dir.path()).unwrap();
        let info = backend.create_session("owner", None).unwrap();
        (dir, backend, info.id)
    }

    #[test]
    fn path_file_roundtrip_stays_path_only() {
        let (_dir, backend, sid) = backend_with_session();
        let msg = ChatMessage {
            role: "user".into(),
            parts: vec![
                ContentPart::File {
                    path: "sessions/s/files/image.png".into(),
                    mime_type: Some("image/png".into()),
                    name: Some("image.png".into()),
                    size_bytes: Some(12),
                },
                ContentPart::Text {
                    text: "hello".into(),
                },
            ],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
            model: None,
            usage: None,
        };
        backend.append_message(&sid, &msg).unwrap();

        let line = fs::read_to_string(backend.history_path(&sid)).unwrap();
        assert!(line.contains("\"type\":\"file\""));
        assert!(!line.contains("base64"));
        assert!(!line.contains("image_b64"));
        assert_eq!(backend.load_messages(&sid)[0].parts.len(), 2);
    }

    #[test]
    fn delegation_checkpoint_roundtrip() {
        let (_dir, backend, _sid) = backend_with_session();
        let cp = crate::storage::DelegationCheckpoint {
            task_id: "test/t/abc123".to_string(),
            parent_session_id: "parent".to_string(),
            sub_session_id: "sub".to_string(),
            agent_name: "coder".to_string(),
            status: "checkpointed".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: 600,
            allowed_tools: Some(vec!["shell".to_string(), "file_edit".to_string()]),
            last_checkpoint: Some(chrono::Utc::now()),
        };
        backend.save_delegation_checkpoint(&cp).unwrap();

        let loaded = backend.load_delegation_checkpoints();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].task_id, "test/t/abc123");
        assert_eq!(loaded[0].status, "checkpointed");
        assert_eq!(loaded[0].timeout_secs, 600);
        assert_eq!(
            loaded[0].allowed_tools.as_deref(),
            Some(&["shell".to_string(), "file_edit".to_string()][..])
        );

        // Delete works.
        backend.delete_delegation_checkpoint("test/t/abc123").unwrap();
        assert!(backend.load_delegation_checkpoints().is_empty());
    }

    #[test]
    fn delegation_checkpoint_multiple_and_corrupt_skips() {
        let (_dir, backend, _sid) = backend_with_session();

        for i in 0..3 {
            let cp = crate::storage::DelegationCheckpoint {
                task_id: format!("test/t/task-{i}"),
                parent_session_id: format!("parent-{i}"),
                sub_session_id: format!("sub-{i}"),
                agent_name: "coder".to_string(),
                status: "running".to_string(),
                started_at: chrono::Utc::now(),
                timeout_secs: 300,
                allowed_tools: None,
                last_checkpoint: None,
            };
            backend.save_delegation_checkpoint(&cp).unwrap();
        }

        // Write a corrupt checkpoint file.
        let corrupt_path = backend.root.join("delegations").join("corrupt.json");
        fs::write(&corrupt_path, b"{not valid json").unwrap();

        let loaded = backend.load_delegation_checkpoints();
        assert_eq!(loaded.len(), 3, "corrupt file should be skipped");
    }
}
