//! agent_lifecycle — L0 facade for the sub-agent lifecycle tools
//! (`agent_kill` / `agent_resume` / `agent_list`).
//!
//! #151 Phase 8+: the tools layer (L3) must not reference the agents layer (L4).
//! The tools only need a thin method surface of `DelegationCoordinator`;
//! this trait is that surface. `DelegationCoordinator` implements it inside
//! the agents layer (L4→L0 is the legal direction), and the composition
//! root keeps passing the concrete `Arc` — the tool constructors are
//! generic over the trait and coerce to `Arc<dyn AgentLifecycle>`
//! internally, so no call site changes.

use crate::api::delegation::RunningAgentInfo;

/// Snapshot of a timed-out sub-agent that `agent_resume` can revive
/// (listing companion to `timed_out_resumables`). Only the fields the
/// not-found listing renders — `started_at` is pre-formatted RFC 3339 so
/// the api layer needs no chrono dependency.
#[derive(Debug, Clone)]
pub struct ResumableAgent {
    pub sub_session_id: String,
    pub agent_name: String,
    pub started_at_rfc3339: String,
}

/// Facade over `agents::DelegationCoordinator` as used by the L3 tools.
#[async_trait::async_trait]
pub trait AgentLifecycle: Send + Sync {
    /// Terminate a running sub-agent; `false` if unknown/already done.
    async fn cancel(&self, sub_session_id: &str) -> bool;

    /// Running sub-agents, unscoped by session (same as `agent_list`).
    fn running_records(&self) -> Vec<RunningAgentInfo>;

    /// Revive a timed-out sub-agent with a fresh budget; returns the
    /// (possibly new) sub-session id to resume.
    fn resume_timed_out(
        &self,
        sub_session_id: &str,
        extra_secs: Option<u64>,
    ) -> anyhow::Result<String>;

    /// Timed-out sub-agents that are currently resumable.
    fn timed_out_resumables(&self) -> Vec<ResumableAgent>;
}
