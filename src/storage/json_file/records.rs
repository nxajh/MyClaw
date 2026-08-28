use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── On-disk types ─────────────────────────────────────────────────────────────


/// A compaction summary that replaced a range of messages, stored in meta
/// rather than as a line in history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CompactionEntry {
    /// Position (0-based) among real messages where the summary should be
    /// inserted when reconstructing the in-memory history.
    pub(super) position: usize,
    /// The full summary text (including the `[CONTEXT COMPACTION...]` prefix).
    pub(super) text: String,
    /// Compaction version number.
    pub(super) version: u32,
}

/// Index entry mapping a segment file to its global message ID range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SegmentRecord {
    /// Segment number (matches archive filename: history.{segment:04}.jsonl).
    /// The active segment uses `meta.segment`.
    pub(super) segment: u32,
    /// Global ID of the first real message in this segment.
    pub(super) start_id: i64,
    /// Number of real messages (excluding compaction entries) in this segment.
    pub(super) count: usize,
    /// Compaction summaries within this segment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) compactions: Vec<CompactionEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SessionMeta {
    pub(super) id: String,
    pub(super) owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) display_name: Option<String>,
    pub(super) created_at: DateTime<Utc>,
    pub(super) last_activity: DateTime<Utc>,
    /// 1-based line count of the active history.jsonl; used as the next-ID base.
    pub(super) message_count: usize,
    /// Number of completed rotations; used to name archive files.
    #[serde(default)]
    pub(super) segment: u32,
    /// Compaction version (0 = never compacted).
    #[serde(default)]
    pub(super) compact_version: u32,
    /// Token estimate from the last compaction summary, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) compact_token_estimate: Option<u64>,
    /// Last known total token count (input + cached + output) from the API.
    /// Persisted after each response so the value survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_total_tokens: Option<u64>,
    /// Per-session runtime overrides (JSON-encoded SessionOverride).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) session_override: Option<String>,
    /// Last incoming message context. Carries sender / receiver / text so
    /// startup recovery can replay the routing context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_message: Option<crate::api::message::PersistedChannelMessage>,
    /// Optional generated summary of this session's purpose/outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) summary: Option<String>,
    /// Owning agent name. "main" for top-level sessions; sub-agent name for
    /// delegate-spawned sessions. Skipped when absent for forward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent_name: Option<String>,
    /// Parent session ID for sub-sessions. None for top-level user sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) delegation_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) delegation_allowed_tools: Option<Vec<String>>,
    /// Global segment index for message ID → file lookup.
    /// Empty for legacy sessions (pre-migration); populated on first rotate or migration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) segments: Vec<SegmentRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct ActiveMap {
    #[serde(flatten)]
    pub(super) map: std::collections::HashMap<String, String>,
}
