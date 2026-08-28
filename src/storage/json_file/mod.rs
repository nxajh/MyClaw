//! JSON-file-backed session storage.
//!
//! Directory layout (under `{workspace_dir}/sessions/`):
//!
//! ```text
//! sessions/
//!   active.json              # { "user_id": "session_id", ... }
//!   {bare_dir_name(session_id)}/  # P1: 裸 uuid（FQID 解析失败回退 dir_name）
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

mod backend;
mod records;
mod session_backend;

pub use backend::JsonFileBackend;

// Test-import forwarding（P0 webui/client 批 3 模式）：tests.rs 的
// `use super::*` 从本模块命名空间取绑定；cfg(test) 门控避免非测试构建
// 下的 unused import（clippy -D warnings）。
#[cfg(test)]
use std::fs;
#[cfg(test)]
use records::SegmentRecord;

#[cfg(test)]
mod tests;
