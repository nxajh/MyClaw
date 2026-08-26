//! Recovery family facade (#151 Phase 6).
//!
//! Three similarly-named modules used to live as flat `recovery.rs` files,
//! making "recovery" ambiguous across the codebase. They are now renamed by
//! domain and re-exported here so callers have one discovery point:
//!
//! - [`startup_recovery`] — scan unfinished sub-agents after a restart
//!   ([`UnfinishedSubAgent`], [`scan_unfinished_subagents`])
//! - [`orchestrator::turn_recovery`] — orchestrator startup pass: replay
//!   inbound spool, drain completion queue, resume unfinished sub-agents
//!   ([`run_startup`])
//! - [`session::breakpoint_detect`] — detect incomplete turns in persisted
//!   history ([`BreakpointItem`], [`identify_breakpoint`],
//!   [`detect_incomplete_turn`])

pub mod startup_recovery;

pub use startup_recovery::{UnfinishedSubAgent, scan_unfinished_subagents};

// Turn recovery entry point (`run_startup`) is `pub(in crate::agents)` in
// `orchestrator/turn_recovery.rs`; orchestrator-internal callers keep using
// `turn_recovery::run_startup` directly.
pub use crate::agents::orchestrator::turn_recovery::run_startup;
