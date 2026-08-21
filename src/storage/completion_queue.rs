//! Persistent delivery queue for delegation completion notices (P2, RFC
//! delegation-notice-queue §5).
//!
//! At-least-once across daemon restarts: a notice is written to disk BEFORE it
//! enters the in-memory queue (`route_notice`), and marked delivered only
//! after the notice turn's content is persisted to session history
//! (`process_turn` returned `Ok`). A crash in between leaves the entry
//! `Pending`; startup recovery (`recovery::recover_completion_queue`)
//! re-enqueues it.
//!
//! Storage is one JSON file per entry, `{seq}.json`, under
//! `{workspace_dir}/.state/completion_queue/`. Files are written via
//! tmp-file + rename with `fsync` (same pattern as `session.rs` file saves).
//! Delivered entries are DELETED (self-compacting; unlike the inbound-spool
//! RFC there is no cross-restart dedup need that tombstones would serve — the
//! dedup key `id` is unique per terminal event / sub-message, and re-delivery
//! is governed by the at-least-once contract). A `seq` counter keeps ids
//! monotonic; it is rebuilt as `max(file seqs) + 1` at open.
//!
//! This is the "minimal self-built version" of the inbound-spool RFC's file
//! infra (docs/inbound-spool-rfc.md) — the spool itself is not implemented,
//! and a dedicated directory avoids colliding with its future compaction
//! (which rewrites its own directory, keeping only its Pending entries).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Delivery state of a persisted completion notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Written at wake time; the notice turn has not (yet) persisted its
    /// content to session history.
    Pending,
    /// The notice turn returned `Ok` — content is in session history. Stored
    /// entries are deleted on delivery, so this state only exists on disk if
    /// written by an older version.
    Delivered,
}

/// One persisted delegation completion notice (RFC §5.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionNoticeEntry {
    /// Monotonic sequence assigned by the store (caller passes 0).
    pub seq: u64,
    /// Unique synthetic id — dedup key, equals the `ChannelInboundMessage.id`
    /// of the notice turn (`delegation:{sub_session_id}` /
    /// `delegation-msg:{msg_id}`).
    pub id: String,
    /// The sub-agent session id the notice is about.
    pub sub_session_id: String,
    /// The parent (main agent) session to deliver into.
    pub parent_session_id: String,
    /// Terminal status: "completed" | "failed" | "timed_out"; `None` for
    /// sub-agent `Message` events (not terminal).
    pub status: Option<String>,
    /// Rendered notice text (same content the in-memory queue carries).
    pub content: String,
    /// Wake-time silence intent snapshot.
    pub silenced_override: Option<bool>,
    /// Sub→parent `Message` events delivered while running (degrades the
    /// completion note to metadata when > 0).
    pub sent_message_count: u64,
    /// Unix seconds at wake time.
    pub enqueued_at: u64,
    pub delivery_state: DeliveryState,
}

/// File-backed delivery queue. Cheap to clone? No — shared via `Arc` on
/// `OrchestratorCtx`; the fields are interior-mutable.
pub struct CompletionNoticeStore {
    dir: PathBuf,
    seq: AtomicU64,
    /// All ids ever appended this process (Pending + delivered) — idempotent
    /// re-append within a process returns `None`.
    seen: Mutex<HashSet<String>>,
    /// Pending entries, seq-ascending (kept in sync with disk).
    pending: Mutex<Vec<CompletionNoticeEntry>>,
}

impl CompletionNoticeStore {
    /// Open (or create) the queue directory, rebuilding the seq counter and
    /// the pending set from disk. Corrupt entries are skipped with a warning
    /// (same tolerance as suspension files); their seq is still counted from
    /// the filename so future appends never collide.
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut seq = 0u64;
        let mut seen = HashSet::new();
        let mut pending = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // seq from the filename keeps monotonicity even when a file is
            // corrupt (its content cannot be parsed).
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Ok(n) = stem.parse::<u64>() {
                    seq = seq.max(n);
                }
            }
            let json = match fs::read_to_string(&path) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(
                        file = %path.display(),
                        err = %e,
                        "completion queue: unreadable entry skipped"
                    );
                    continue;
                }
            };
            let parsed: CompletionNoticeEntry = match serde_json::from_str(&json) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        file = %path.display(),
                        err = %e,
                        "completion queue: corrupt entry skipped"
                    );
                    continue;
                }
            };
            seen.insert(parsed.id.clone());
            match parsed.delivery_state {
                DeliveryState::Pending => pending.push(parsed),
                // Stale tombstone from an older version — clean up.
                DeliveryState::Delivered => {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        pending.sort_by_key(|e| e.seq);
        Ok(Self {
            dir,
            seq: AtomicU64::new(seq),
            seen: Mutex::new(seen),
            pending: Mutex::new(pending),
        })
    }

    /// Append a `Pending` entry (fsync'd) and return its assigned seq, or
    /// `None` when an entry with the same `id` is already known (dedup).
    /// `entry.seq` is ignored and overwritten.
    pub fn append(&self, mut entry: CompletionNoticeEntry) -> std::io::Result<Option<u64>> {
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.insert(entry.id.clone()) {
                // Issue #106: this is NOT a dropped notification — a notice
                // with this id was already appended (and its in-memory
                // delivery already queued/in flight, e.g. a resume's second
                // terminal event reusing the same synthetic id); only the
                // *on-disk persistence* is skipped, since re-writing it would
                // be a redundant duplicate entry on top of one already
                // tracked. The notice still gets delivered normally.
                tracing::warn!(
                    notice_id = %entry.id,
                    "completion queue: duplicate id, skipping on-disk persist only (delivery already in flight, not dropped)"
                );
                return Ok(None);
            }
        }
        entry.seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        entry.delivery_state = DeliveryState::Pending;
        let seq = entry.seq;
        Self::write_entry(&self.entry_path(seq), &entry)?;
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry);
        Ok(Some(seq))
    }

    /// Mark a Pending entry delivered: delete its file and drop it from the
    /// in-memory pending set. The id stays in the seen set (no re-append
    /// within this process). Returns `false` when the id is unknown (already
    /// delivered or never appended) — not an error.
    pub fn mark_delivered(&self, id: &str) -> std::io::Result<bool> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let Some(idx) = pending.iter().position(|e| e.id == id) else {
            return Ok(false);
        };
        let entry = pending.remove(idx);
        match fs::remove_file(self.entry_path(entry.seq)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Snapshot of all Pending entries, seq-ascending (startup recovery).
    pub fn pending(&self) -> Vec<CompletionNoticeEntry> {
        let mut v = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        v.sort_by_key(|e| e.seq);
        v
    }

    /// Number of Pending entries (tests / diagnostics).
    pub fn len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn entry_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.json"))
    }

    /// tmp-file + rename + fsync (durable against crash between write and
    /// rename; same pattern as `session.rs` file saves).
    fn write_entry(path: &Path, entry: &CompletionNoticeEntry) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            serde_json::to_writer(&mut f, entry).map_err(std::io::Error::other)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> CompletionNoticeEntry {
        CompletionNoticeEntry {
            seq: 0,
            id: id.to_string(),
            sub_session_id: "myclaw/s/sub".to_string(),
            parent_session_id: "myclaw/s/parent".to_string(),
            status: Some("completed".to_string()),
            content: "notice".to_string(),
            silenced_override: Some(true),
            sent_message_count: 0,
            enqueued_at: 0,
            delivery_state: DeliveryState::Pending,
        }
    }

    #[test]
    fn append_persists_pending_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompletionNoticeStore::open(dir.path().join("completion_queue")).unwrap();
        let seq = store.append(entry("delegation:s1")).unwrap().unwrap();
        assert_eq!(seq, 1);
        assert_eq!(store.len(), 1);
        // File on disk, seq-ascending pending snapshot.
        assert!(dir.path().join("completion_queue/1.json").exists());
        let pending = store.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "delegation:s1");
        assert_eq!(pending[0].seq, 1);
        assert_eq!(pending[0].delivery_state, DeliveryState::Pending);
    }

    #[test]
    fn append_dedupes_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompletionNoticeStore::open(dir.path().join("completion_queue")).unwrap();
        assert!(store.append(entry("delegation:s1")).unwrap().is_some());
        assert!(store.append(entry("delegation:s1")).unwrap().is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn mark_delivered_removes_pending_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompletionNoticeStore::open(dir.path().join("completion_queue")).unwrap();
        let seq = store.append(entry("delegation:s1")).unwrap().unwrap();
        assert!(store.mark_delivered("delegation:s1").unwrap());
        assert!(store.is_empty());
        assert!(!dir.path().join("completion_queue").join(format!("{seq}.json")).exists());
        // Unknown id → false, not an error.
        assert!(!store.mark_delivered("delegation:s1").unwrap());
        // Same process: re-append is still deduped.
        assert!(store.append(entry("delegation:s1")).unwrap().is_none());
    }

    #[test]
    fn reopen_recovers_pending_and_continues_seq() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = CompletionNoticeStore::open(dir.path().join("completion_queue")).unwrap();
            store.append(entry("delegation:s1")).unwrap();
            store.append(entry("delegation:s2")).unwrap();
            store.mark_delivered("delegation:s1").unwrap();
        }
        let store = CompletionNoticeStore::open(dir.path().join("completion_queue")).unwrap();
        let pending = store.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "delegation:s2");
        // seq continues past the max seen on disk.
        let seq = store.append(entry("delegation:s3")).unwrap().unwrap();
        assert_eq!(seq, 3);
        // Delivered files are cleaned up on open (none left here).
        assert!(!dir.path().join("completion_queue/1.json").exists());
    }

    #[test]
    fn open_skips_corrupt_entries_but_counts_their_seq() {
        let dir = tempfile::tempdir().unwrap();
        let qdir = dir.path().join("completion_queue");
        fs::create_dir_all(&qdir).unwrap();
        fs::write(qdir.join("5.json"), "not json{{").unwrap();
        let store = CompletionNoticeStore::open(qdir).unwrap();
        assert!(store.is_empty());
        let seq = store.append(entry("delegation:s1")).unwrap().unwrap();
        assert_eq!(seq, 6, "seq must not collide with the corrupt 5.json");
    }
}
