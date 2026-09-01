use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::records::{ActiveMap, CompactionEntry, SegmentRecord, SessionMeta};
use crate::ids::{bare_dir_name, id_from_dir, Fqid, DEFAULT_NAMESPACE, TYPE_SESSION};
use crate::storage::{ChatMessage, SessionInfo};

// ── Backend ───────────────────────────────────────────────────────────────────

/// JSON-file-backed session persistence.
pub struct JsonFileBackend {
    pub(super) root: PathBuf,
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

    /// P1: 目录名裸化 —— FQID session 用裸 uuid 段，遗留 key 回退 `dir_name`。
    pub(super) fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(bare_dir_name(session_id))
    }

    pub(super) fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("meta.json")
    }

    pub(super) fn history_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("history.jsonl")
    }

    pub(super) fn archive_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("archive")
    }

    fn active_path(&self) -> PathBuf {
        self.root.join("active.json")
    }

    /// 方案 C (RFC §5): turn-suspension state, independent of meta.json.
    pub(super) fn suspension_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("suspension.json")
    }

    /// Durable delegation checkpoint file: `{session_dir(sub)}/delegation.json`
    /// (P1: checkpoint 依附宿主 sub-session 目录，脐带与宿主同生共死)。
    pub(super) fn delegation_checkpoint_path(&self, sub_session_id: &str) -> PathBuf {
        self.session_dir(sub_session_id).join("delegation.json")
    }

    // ── Atomic write helpers ──────────────────────────────────────────────────

    /// Write compact JSON atomically via a uniquely-named temp file + rename.
    ///
    /// The temp file is created in the same directory as `path` (required for
    /// the final rename to be atomic) with a process- and call-unique name, so
    /// concurrent writers to the same `path` never share (and truncate) a
    /// single fixed `.tmp` file.
    pub(super) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut tmp, value).map_err(std::io::Error::other)?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    // ── Meta helpers ──────────────────────────────────────────────────────────

    pub(super) fn read_meta(&self, session_id: &str) -> Option<SessionMeta> {
        let bytes = fs::read(self.meta_path(session_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Read `meta.json` directly under a known session directory path.
    ///
    /// Unlike `read_meta`, this does **not** round-trip through an FQID and
    /// `bare_dir_name` — it trusts the directory the caller already has.
    /// That distinction matters for directory-scan callers (`list_sessions`,
    /// `list_all_sessions`): re-deriving the path from `meta.id` via
    /// `bare_dir_name` assumes the P1-A bare-uuid layout, but pre-P1-A
    /// session dirs are still named `myclaw_s_<uuid>` on disk. Round-tripping
    /// through the FQID for those looks up a bare-uuid directory that never
    /// existed, silently dropping every legacy session from the scan.
    pub(super) fn read_meta_at(dir: &Path) -> Option<SessionMeta> {
        let bytes = fs::read(dir.join("meta.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub(super) fn write_meta(&self, meta: &SessionMeta) -> std::io::Result<()> {
        let dir = self.session_dir(&meta.id);
        fs::create_dir_all(&dir)?;
        Self::write_json_atomic(&self.meta_path(&meta.id), meta)
    }

    // ── Active session map ────────────────────────────────────────────────────

    pub(super) fn read_active(&self) -> ActiveMap {
        fs::read(self.active_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub(super) fn write_active(&self, map: &ActiveMap) -> std::io::Result<()> {
        Self::write_json_atomic(&self.active_path(), map)
    }

    // ── ID generation ─────────────────────────────────────────────────────────

    pub(super) fn generate_session_id(&self) -> String {
        Fqid::new(&self.namespace, TYPE_SESSION).to_string()
    }

    // ── JSONL helpers ─────────────────────────────────────────────────────────

    /// Read all (id, ChatMessage) pairs from the active history.jsonl.
    /// IDs are globally monotonic across all segments (segment.start_id + offset).
    ///
    /// Compaction summaries (stored in the segment record, not in the file) are
    /// inserted at their recorded positions with id=0.
    pub(super) fn read_history_with_ids(&self, session_id: &str) -> Vec<(i64, ChatMessage)> {
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

        // issue #213: a process killed mid-write can leave a single torn line
        // (invalid UTF-8, or valid UTF-8 but unparseable JSON). Previously
        // `.lines().map_while(Result::ok)` stopped reading entirely on the
        // first such line, silently discarding every line after it. Skip and
        // warn on bad lines instead, so one torn line costs one message, not
        // the rest of the file.
        let mut hydrated = Vec::new();
        let mut dropped = 0u32;
        for line_result in BufReader::new(f).lines() {
            let line = match line_result {
                Ok(line) => line,
                Err(err) => {
                    dropped += 1;
                    tracing::warn!(
                        session_id, %err,
                        "history.jsonl: skipping unreadable line (likely a torn write)"
                    );
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ChatMessage>(&line) {
                Ok(msg) => hydrated.push(self.hydrate(session_id, &msg)),
                Err(err) => {
                    dropped += 1;
                    tracing::warn!(
                        session_id, %err,
                        "history.jsonl: skipping unparseable line (likely a torn write)"
                    );
                }
            }
        }
        if dropped > 0 {
            tracing::warn!(session_id, dropped, "history.jsonl: dropped corrupted line(s) while loading");
        }
        let mut result: Vec<(i64, ChatMessage)> = hydrated
            .into_iter()
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

    pub(super) fn meta_to_info(meta: &SessionMeta) -> SessionInfo {
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

    pub(super) fn rotate_history_impl(
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
        //
        // The new segment always starts at the computed value
        // (archived_start_id + archived_count) so the segment chain stays
        // continuous and non-overlapping. Never fall back to a survivor's
        // in-memory id: when meta.json was rebuilt externally (e.g. the
        // message-id migration) while the daemon kept old in-memory ids, that
        // override wrote stale ids into new segments and broke the chain.
        // Surviving messages keep their in-memory ids for the rest of the run;
        // on reload they are renumbered from the recorded start_id.
        let mut live_hashes: HashSet<String> = HashSet::new();
        let mut real_count = 0usize;
        let mut new_compactions: Vec<CompactionEntry> = Vec::new();
        let new_start_id = archived_start_id + archived_count as i64;

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
    pub(super) fn externalize(
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
    pub(super) fn sweep_blobs(&self, session_id: &str, live: &HashSet<String>) {
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
    pub(super) fn extend_archived_live_sets(&self, session_id: &str, blob_hashes: &mut HashSet<String>) {
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

            let _dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            // P1 裸 uuid 目录：id 重建走 meta.json / namespace 组装，此处仅为迁移遍历占位。
            let _session_id = id_from_dir(_dir_name);

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
pub(super) fn collect_blob_hashes(_msg: &ChatMessage, _set: &mut HashSet<String>) {}
