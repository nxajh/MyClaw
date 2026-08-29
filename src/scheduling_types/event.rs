//! Scheduler event contract (L1).
//!
//! `SchedulerEvent` / `CronTrigger` moved from `agents/orchestrator/mod.rs`
//! (#151 Phase 3d) to break the scheduling_runtime→agents edge of the
//! agents↔scheduling_runtime SCC: the scheduler (L4) produces these events,
//! the orchestrator (L4) consumes them — both now depend on this L1 module
//! instead of on each other.

/// A cron job that fired and needs a turn run. The fields the scheduled path
/// actually consumes — delivery/enabled_tools/disabled_tools/provider, which
/// the cron turn ignores, are deliberately not carried here.
#[derive(Debug)]
pub struct CronTrigger {
    pub session_key: String,
    pub prompt: String,
    pub target_channel: Option<String>,
    pub target_account: Option<String>,
    pub target_recipient: Option<String>,
    /// Thread/topic ID (#78 — `DeliveryConfig.thread_id`, previously dead).
    pub target_thread: Option<String>,
    /// True when the job's delivery mode is `None` (#78) — the turn still
    /// runs, but its output must not be sent to any channel.
    pub delivery_suppressed: bool,
    pub job_id: String,
    pub model: Option<String>,
    /// Context policy: inject into user session or run isolated.
    pub context_policy: crate::config::scheduler::ContextPolicy,
    /// Creator identity (user FQID) carried from `JobEntry.creator`
    /// (#101 P2) — the scheduled turn's session adopts it when its owner
    /// is unattributed (Isolated `_job_*` sessions).
    pub creator: Option<String>,
}

/// Events from the Scheduler (cron triggers, distill checks).
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SchedulerEvent {
    /// Cron job matched — run agent with specific prompt.
    Cron(CronTrigger),
    /// Idle-time memory distillation check (system idle + maybe new
    /// user memories — the orchestrator verifies before running).
    Distill,
    /// Idle-time skill internalization proposer check (system idle + maybe
    /// changed user-layer skills — the orchestrator verifies before running).
    ProposeSkills,
}
