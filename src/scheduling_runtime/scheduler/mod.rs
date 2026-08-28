//! Scheduler — Cron job scheduling, storage, and execution events.
//!
//! The Scheduler is the single owner of all cron job data. It:
//!   - Loads and persists jobs from `{jobs_root}/{id}/meta.json` (P1-B2)
//!   - Hot-reloads when the file changes on disk
//!   - Sends timing events (cron, distill) via mpsc channel
//!   - Provides CRUD methods for cronjob_tool
//!   - Records run results
//!
//! External code interacts through `SharedScheduler` (Arc<Scheduler>).
//!
//! Directory layout (P2 split, pure move from the former 1986-line
//! scheduler.rs; see `refactor/split-p2-engines`):
//!   - `core` — `Scheduler`/`SharedScheduler`/`OrchestratorHook`/
//!     `DistillConfig`/`JobsFile` types, `SchedulerApi` facade, the event
//!     loop, CRUD, delivery resolution, persistence impls
//!   - `jobs_file` — jobs-store loading + legacy-format folding
//!     (per-job meta.json dirs, legacy jobs.json fallback)
//!   - `timing` — `parse_interval` / `compute_next_run`
//!   - `tests` — the original test module (kept at `scheduler::tests`)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::{Mutex as ParkMutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::scheduling_types::cron_types::{
    DeliveryConfig, DeliveryMode, RunRecord, RunStatus, ScheduleKind, ScheduleSpec,
};
use crate::scheduling_types::event::SchedulerEvent;
use crate::api::message::Channel;

use super::webhook::{WebhookJobDef, is_route_slug};

// #151 Phase 8+：契约类型/纯函数已下沉 scheduling_types::job_types，此处 re-export 保持既有路径。
pub use crate::scheduling_types::job_types::{
    cron_delivery_fields, is_active_hours, normalize_weekday_unix, parse_target_string, resolve_tz,
    scan_prompt_injection, validate_active_hours, validate_at_timestamp, validate_schedule,
    validate_tz, JobEntry, JobRemovalAudit, JobUpdate, WebhookDef, WebhookFilter,
};

mod core;
mod jobs_file;
mod timing;

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests;

// 子模块经 `use super::*` 复用上方共享导入（原单文件头部，纯移动）。
// 对外转发保持 `crate::scheduling_runtime::scheduler::*` 既有路径零改动：
//   - daemon/mod.rs：Scheduler / DistillConfig / OrchestratorHook
//   - agents/mod.rs：JobEntry / JobUpdate / Scheduler / SharedScheduler /
//     is_active_hours / resolve_tz / scan_prompt_injection
//   - scheduling_runtime/webhook.rs：OrchestratorHook / SharedScheduler /
//     WebhookDef / WebhookFilter / parse_target_string
// （`self::` 前缀避免与内建 `core` crate 路径歧义。）
pub use self::core::{DistillConfig, OrchestratorHook, Scheduler, SharedScheduler};
pub use self::timing::{compute_next_run, parse_interval};

#[cfg(test)]
use self::core::JobsFile; // tests::jobs_store_* 直接构造 JobsFile 序列化
