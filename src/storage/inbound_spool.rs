//! Persistent inbound-message spool (RFC inbound-spool).
//!
//! At-least-once delivery of channel messages across daemon restarts (hot
//! switch / crash / SIGKILL): a message is written to disk BEFORE it enters
//! the orchestrator's in-memory event loop, and marked `Done` only after
//! `inbound::dispatch` returned (the message is in session history — session
//! recovery owns everything after that point). A crash in between leaves the
//! entry `Pending`; startup recovery (`recovery::recover_inbound_spool`)
//! replays it.
//!
//! Storage is one JSON file per message, `{seq}.json`, under
//! `{workspace_dir}/.state/inbound_spool/`. Files are written via
//! tmp-file + rename with `fsync` (same pattern as `completion_queue.rs`).
//!
//! Unlike the completion queue (which DELETES delivered files and needs no
//! tombstones — its dedup id is unique per event), the spool KEEPS a `Done`
//! tombstone: WeChat's getupdates buf can roll back and re-deliver already
//! processed messages, so the cross-restart dedup index (`seen`, rebuilt from
//! all files including tombstones) must know "this (channel, account, msg.id)
//! was already handled". Tombstones are bounded by `compact_if_needed`
//! (startup; removes tombstones older than 7 days or when file count exceeds
//! 5000 — old-enough re-deliveries are assumed impossible, same trade-off as
//! the RFC §4.2 threshold).
//!
//! Attachment messages are handled by the CALLER (`spawn_listener` bypasses
//! `append` when `msg.content.files` is non-empty; RFC §6.3) — this module
//! only ever sees text messages, and `Ok(None)` from `append` unambiguously
//! means "dedup hit, do not deliver".
//!
//! `baseline_seq` (max seq at open) is the replay watermark: `pending()`
//! returns only entries with `seq <= baseline`. During a hot switch the new
//! process keeps appending new messages (seq > baseline) while waiting for
//! the old process to exit; those must NOT be replayed (they will flow
//! through the live event loop), otherwise they would be double-processed.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::channels::{ChannelInboundMessage, PersistedChannelMessage};

/// Spool state of one persisted inbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpoolStatus {
    /// Written at receive time; `inbound::dispatch` has not (yet) returned.
    Pending,
    /// `inbound::dispatch` returned — the message is in session history.
    /// Kept as a tombstone so cross-restart dedup (WeChat buf rollback) can
    /// still see "already handled".
    Done,
}

/// One persisted inbound message (RFC §4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolEntry {
    /// Monotonic sequence assigned by the store (global, per process + on
    /// disk across restarts).
    pub seq: u64,
    /// Channel type: "qqbot" | "telegram" | "wechat".
    pub channel: String,
    /// Account id within the channel.
    pub account: String,
    /// The message body (serializable form; file bodies are runtime-only and
    /// never spooled — attachment messages skip persistence entirely).
    pub msg: PersistedChannelMessage,
    pub status: SpoolStatus,
    /// Unix seconds when the entry was appended.
    pub created_at: u64,
}

/// File-backed inbound spool. Shared via `Arc` on `OrchestratorCtx`; the
/// fields are interior-mutable.
pub struct InboundSpool {
    dir: PathBuf,
    seq: AtomicU64,
    /// Replay watermark: max seq at open. `pending()` filters `seq <= baseline`
    /// so a hot-switch successor never replays messages it appended while
    /// waiting for the old process (RFC §8.3).
    baseline_seq: u64,
    /// All dedup keys ever seen this process (Pending + Done tombstones) —
    /// idempotent re-append within a process returns `None`.
    seen: Mutex<HashSet<String>>,
    /// Pending entries, seq-ascending (kept in sync with disk).
    pending: Mutex<Vec<SpoolEntry>>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Dedup key: NUL-separated so channel/account strings can never collide.
fn dedup_key(channel: &str, account: &str, msg_id: &str) -> String {
    format!("{channel}\u{0}{account}\u{0}{msg_id}")
}

impl InboundSpool {
    /// Open (or create) the spool directory, rebuilding the seq counter, the
    /// baseline watermark and the seen/pending sets from disk. Corrupt entries
    /// are skipped with a warning; their seq is still counted from the
    /// filename so future appends never collide (same tolerance as the
    /// completion queue).
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
                        "inbound spool: unreadable entry skipped"
                    );
                    continue;
                }
            };
            let parsed: SpoolEntry = match serde_json::from_str(&json) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        file = %path.display(),
                        err = %e,
                        "inbound spool: corrupt entry skipped"
                    );
                    continue;
                }
            };
            seen.insert(dedup_key(&parsed.channel, &parsed.account, &parsed.msg.id));
            if parsed.status == SpoolStatus::Pending {
                pending.push(parsed);
            }
        }
        pending.sort_by_key(|e| e.seq);
        let baseline_seq = seq;
        Ok(Self {
            dir,
            seq: AtomicU64::new(seq),
            baseline_seq,
            seen: Mutex::new(seen),
            pending: Mutex::new(pending),
        })
    }

    /// Append a `Pending` entry (fsync'd) and return its assigned seq, or
    /// `None` when the same `(channel, account, msg.id)` is already known
    /// (Pending or Done) — the caller must NOT deliver again.
    ///
    /// Attachment messages never reach this method: the caller
    /// (`spawn_listener`) checks `msg.content.files.is_empty()` and bypasses
    /// spooling entirely (RFC §6.3 — file bodies are runtime-only), so an
    /// unsupported message is always delivered live with `seq: 0` and is
    /// never added to the seen set (a WeChat buf rollback re-delivering it
    /// must still reach the live path).
    pub fn append(
        &self,
        channel: &str,
        account: &str,
        msg: &ChannelInboundMessage,
    ) -> std::io::Result<Option<u64>> {
        let key = dedup_key(channel, account, &msg.id);
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.insert(key) {
                tracing::warn!(
                    channel = %channel,
                    account = %account,
                    msg_id = %msg.id,
                    "inbound spool: duplicate key; skipping persist"
                );
                return Ok(None);
            }
        }
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let entry = SpoolEntry {
            seq,
            channel: channel.to_string(),
            account: account.to_string(),
            msg: msg.to_persisted(),
            status: SpoolStatus::Pending,
            created_at: unix_now(),
        };
        Self::write_entry(&self.entry_path(seq), &entry)?;
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry);
        Ok(Some(seq))
    }

    /// Mark a Pending entry done: rewrite the file as a `Done` tombstone
    /// (fsync'd), then drop it from the in-memory pending set. The dedup key
    /// stays in the seen set (no re-append within this process) and survives
    /// restarts via the tombstone file. Returns `false` when the seq is
    /// unknown or already `Done` — not an error. Disk is the source of truth:
    /// the tombstone is written BEFORE the in-memory set is updated, so a
    /// failed write keeps the entry Pending for the next replay attempt
    /// (at-least-once; a duplicate replay beats a lost message).
    pub fn mark_done(&self, seq: u64) -> std::io::Result<bool> {
        let path = self.entry_path(seq);
        let json = match fs::read_to_string(&path) {
            Ok(j) => j,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut entry: SpoolEntry = serde_json::from_str(&json).map_err(std::io::Error::other)?;
        if entry.status == SpoolStatus::Done {
            return Ok(false);
        }
        entry.status = SpoolStatus::Done;
        Self::write_entry(&path, &entry)?;
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|e| e.seq != seq);
        Ok(true)
    }

    /// Snapshot of Pending entries eligible for replay: seq-ascending, filtered
    /// to `seq <= baseline_seq` (the watermark at open) so a hot-switch
    /// successor never replays messages it appended while waiting.
    pub fn pending(&self) -> Vec<SpoolEntry> {
        let mut v = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        v.retain(|e| e.seq <= self.baseline_seq);
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

    /// Startup maintenance: when the directory holds more than 5000 files or
    /// the oldest `Done` tombstone is older than 7 days, delete every
    /// tombstone (Pending entries are kept, seq numbering continues — holes
    /// are harmless since seq is only a monotonic id). Returns whether a
    /// compaction ran. Deviation from RFC §4.2: seq numbers are NOT re-packed
    /// (rewriting Pending files buys nothing; monotonicity is preserved).
    pub fn compact_if_needed(&self) -> std::io::Result<bool> {
        let mut files: Vec<(PathBuf, SpoolEntry)> = Vec::new();
        let mut tombstone_count = 0usize;
        let mut oldest_done = u64::MAX;
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let json = match fs::read_to_string(&path) {
                Ok(j) => j,
                Err(_) => continue,
            };
            let Ok(parsed) = serde_json::from_str::<SpoolEntry>(&json) else {
                continue;
            };
            if parsed.status == SpoolStatus::Done {
                tombstone_count += 1;
                oldest_done = oldest_done.min(parsed.created_at);
            }
            files.push((path, parsed));
        }
        let now = unix_now();
        let needs = files.len() > 5000
            || (tombstone_count > 0 && now.saturating_sub(oldest_done) > 7 * 24 * 3600);
        if !needs {
            return Ok(false);
        }
        for (path, entry) in files {
            if entry.status == SpoolStatus::Done {
                let _ = fs::remove_file(path);
            }
        }
        Ok(true)
    }

    fn entry_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.json"))
    }

    /// tmp-file + rename + fsync (durable against crash between write and
    /// rename; same pattern as the completion queue).
    fn write_entry(path: &Path, entry: &SpoolEntry) -> std::io::Result<()> {
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
    use crate::channels::{ChannelMessageContent, MessageReceiver, MessageSender};

    fn msg(id: &str) -> ChannelInboundMessage {
        ChannelInboundMessage {
            id: id.to_string(),
            sender: MessageSender::new("user"),
            receiver: MessageReceiver::new("bot"),
            content: ChannelMessageContent::text("hello"),
            timestamp: 0,
            interruption_scope_id: None,
            silenced_override: None,
        }
    }

    #[test]
    fn append_persists_pending_entry() {
        let dir = tempfile::tempdir().unwrap();
        let spool = InboundSpool::open(dir.path().join("inbound_spool")).unwrap();
        let seq = spool.append("telegram", "acc1", &msg("m1")).unwrap().unwrap();
        assert_eq!(seq, 1);
        assert_eq!(spool.len(), 1);
        // File on disk, parseable, seq-ascending pending snapshot.
        let path = dir.path().join("inbound_spool/1.json");
        assert!(path.exists());
        let on_disk: SpoolEntry = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(on_disk.status, SpoolStatus::Pending);
        assert_eq!(on_disk.msg.id, "m1");
        assert_eq!(on_disk.channel, "telegram");
        // In-memory pending set holds it (post-open appends are not returned
        // by pending() — that's the baseline watermark, covered by
        // pending_respects_baseline_watermark / reopen tests).
        assert_eq!(spool.len(), 1);
    }

    #[test]
    fn append_dedupes_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let spool = InboundSpool::open(dir.path().join("inbound_spool")).unwrap();
        assert!(spool.append("telegram", "acc1", &msg("m1")).unwrap().is_some());
        // Same (channel, account, id) → deduped.
        assert!(spool.append("telegram", "acc1", &msg("m1")).unwrap().is_none());
        assert_eq!(spool.len(), 1);
        // Same id, different account / channel → distinct key.
        assert!(spool.append("telegram", "acc2", &msg("m1")).unwrap().is_some());
        assert!(spool.append("wechat", "acc1", &msg("m1")).unwrap().is_some());
        assert_eq!(spool.len(), 3);
    }

    #[test]
    fn mark_done_writes_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let spool = InboundSpool::open(dir.path().join("inbound_spool")).unwrap();
        let seq = spool.append("telegram", "acc1", &msg("m1")).unwrap().unwrap();
        assert!(spool.mark_done(seq).unwrap());
        // pending() no longer returns it…
        assert!(spool.pending().is_empty());
        assert_eq!(spool.len(), 0);
        // …but the file survives as a Done tombstone (cross-restart dedup).
        let path = dir.path().join("inbound_spool").join(format!("{seq}.json"));
        assert!(path.exists());
        let on_disk: SpoolEntry = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(on_disk.status, SpoolStatus::Done);
        // Already done → false, not an error.
        assert!(!spool.mark_done(seq).unwrap());
        // Same process: re-append is still deduped.
        assert!(spool.append("telegram", "acc1", &msg("m1")).unwrap().is_none());
    }

    #[test]
    fn reopen_recovers_pending_and_seq_and_seen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = InboundSpool::open(dir.path().join("inbound_spool")).unwrap();
            spool.append("telegram", "acc1", &msg("m1")).unwrap();
            spool.append("telegram", "acc1", &msg("m2")).unwrap();
            spool.mark_done(spool.append("telegram", "acc1", &msg("m0")).unwrap().unwrap())
                .unwrap();
        }
        let spool = InboundSpool::open(dir.path().join("inbound_spool")).unwrap();
        let pending = spool.pending();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].msg.id, "m1");
        assert_eq!(pending[1].msg.id, "m2");
        // seq continues past the max seen on disk.
        let seq = spool.append("telegram", "acc1", &msg("m3")).unwrap().unwrap();
        assert_eq!(seq, 4);
        // Tombstone survives reopen → same key is still deduped.
        assert!(spool.append("telegram", "acc1", &msg("m0")).unwrap().is_none());
    }

    #[test]
    fn pending_respects_baseline_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let spool = InboundSpool::open(dir.path().join("inbound_spool")).unwrap();
        // Baseline = 0 (empty dir). Entries appended after open have seq > 0
        // and must NOT be returned by pending() — hot-switch successor would
        // otherwise replay messages it received while waiting.
        spool.append("telegram", "acc1", &msg("m1")).unwrap();
        spool.append("telegram", "acc1", &msg("m2")).unwrap();
        assert!(spool.pending().is_empty(), "post-open appends are not replayable");
        // But they still occupy the pending set (len) and can be marked done.
        assert_eq!(spool.len(), 2);
        let entries = spool
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn compact_removes_tombstones_keeps_pending() {
        let dir = tempfile::tempdir().unwrap();
        let spool_dir = dir.path().join("inbound_spool");
        let spool = InboundSpool::open(spool_dir.clone()).unwrap();
        spool.append("telegram", "acc1", &msg("m1")).unwrap();
        let done_seq = spool.append("telegram", "acc1", &msg("m2")).unwrap().unwrap();
        spool.mark_done(done_seq).unwrap();
        // Not yet compacted: tombstone present, pending intact (in-memory set).
        assert!(spool_dir.join(format!("{done_seq}.json")).exists());
        assert_eq!(spool.len(), 1);
        // Force compaction by faking an old tombstone (created_at in the past).
        let tomb_path = spool_dir.join(format!("{done_seq}.json"));
        let mut entry: SpoolEntry =
            serde_json::from_str(&fs::read_to_string(&tomb_path).unwrap()).unwrap();
        entry.created_at = unix_now() - 8 * 24 * 3600;
        InboundSpool::write_entry(&tomb_path, &entry).unwrap();
        assert!(spool.compact_if_needed().unwrap());
        assert!(!tomb_path.exists(), "tombstone removed");
        assert!(spool_dir.join("1.json").exists(), "pending kept");
        assert_eq!(spool.len(), 1);
        // No-op when there is nothing to compact.
        assert!(!spool.compact_if_needed().unwrap());
    }

    #[test]
    fn open_skips_corrupt_entries_but_counts_their_seq() {
        let dir = tempfile::tempdir().unwrap();
        let sdir = dir.path().join("inbound_spool");
        fs::create_dir_all(&sdir).unwrap();
        fs::write(sdir.join("5.json"), "not json{{").unwrap();
        let spool = InboundSpool::open(sdir).unwrap();
        assert!(spool.is_empty());
        let seq = spool.append("telegram", "acc1", &msg("m1")).unwrap().unwrap();
        assert_eq!(seq, 6, "seq must not collide with the corrupt 5.json");
    }
}
