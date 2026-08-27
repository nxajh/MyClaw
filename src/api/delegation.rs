//! delegation — sub-agent delegation contracts, L0.
//!
//! [`AgentDelegator`] (the trait `agent_delegate` depends on),
//! [`DelegationStatus`], [`RunningAgentInfo`] and the
//! [`SUB_AGENT_TIMEOUT_MAX_SECS`] hard ceiling. Sunk from
//! `agents::delegator` / `agents::delegation` / `agents::delegation_coordinator`
//! in #151 phase8+ so the tools layer can hold the contract without
//! reaching into L4; the old `agents::` re-export paths stay alive (compat).

use async_trait::async_trait;

/// Invokes sub-agents on demand.
#[async_trait]
pub trait AgentDelegator: Send + Sync {
    /// Run the named sub-agent on `task`, returning its summary text.
    ///
    /// `parent_session` provides channel / reply_target / sender via
    /// `parent.channel` and `parent.last_message`—the sub-session inherits
    /// these so its streaming events and ask_user calls land on the same UI.
    async fn delegate(
        &self,
        agent_name: &str,
        task: &str,
        parent_ctx: &crate::api::tool::ToolContext,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
    ) -> anyhow::Result<String>;

    /// Spawn the sub-agent in the background and return its `session_id`
    /// immediately.
    ///
    /// Completion or failure is reported asynchronously via `DelegationEvent`.
    /// The default implementation returns an error (sync-only delegators).
    fn delegate_async(
        &self,
        _agent_name: &str,
        _task: &str,
        _parent_ctx: &crate::api::tool::ToolContext,
        _timeout: Option<u64>,
        _allowed_tools: Option<Vec<String>>,
        _workspace: Option<&str>,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("async delegation not supported"))
    }

    /// List sub-agents the `DelegateTool` may target. Used to construct the
    /// tool's JSON schema (the `agent` parameter's enum).
    ///
    /// Returns `(name, description)` pairs. `description` is the
    /// AGENT.md front-matter description (Markdown stripped).
    fn list_available(&self) -> Vec<(String, Option<String>)>;
}

/// Delegation lifecycle status for a sub-agent task.
///
/// Consumed by the coordinator's running table and surfaced through
/// `agent_list` so the parent agent can distinguish a healthy in-flight
/// task from one killed by the wall-clock timeout or `agent_kill`.
///
/// `Idle` is reserved for the future "parked waiting for parent message"
/// mode (RFC agent-messaging §3): async sub-agents currently run to
/// completion without parking, so no live entry ever transitions to
/// `Idle` today. The variant exists so the state machine is complete and
/// callers (`agent_list`) can render it once the parked mode lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    /// Spawned into the background and processing (the only live state
    /// today — entries are removed from the running table on exit).
    Running,
    /// Parked waiting for a parent message (reserved; see enum doc).
    Idle,
    /// Finished successfully (transient — recorded in the event, the
    /// entry is removed from the running table immediately after).
    Completed,
    /// Finished with an error (transient).
    Failed,
    /// Killed by the wall-clock timeout (transient).
    TimedOut,
    /// Cancelled by the parent via `agent_kill` (transient).
    Cancelled,
    /// Persisted to disk during shutdown; the task is resumable on restart.
    /// On startup, checkpointed tasks are resumed (not marked Failed).
    Checkpointed,
}

impl DelegationStatus {
    /// Whether this status means the task is no longer executing.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DelegationStatus::Completed
                | DelegationStatus::Failed
                | DelegationStatus::TimedOut
                | DelegationStatus::Cancelled
        )
    }

    /// Snake-case string form — matches the serde representation and the
    /// `DelegationCheckpoint.status` field, so persisted statuses can be
    /// compared/written without going through serde.
    pub fn as_str(self) -> &'static str {
        match self {
            DelegationStatus::Running => "running",
            DelegationStatus::Idle => "idle",
            DelegationStatus::Completed => "completed",
            DelegationStatus::Failed => "failed",
            DelegationStatus::TimedOut => "timed_out",
            DelegationStatus::Cancelled => "cancelled",
            DelegationStatus::Checkpointed => "checkpointed",
        }
    }
}

/// Snapshot view of a running-table entry for `agent_list` / logging.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunningAgentInfo {
    /// The sub-agent's session FQID (`<ns>/s/<uuidv7>`) — its identity and
    /// the addressing key for `agent_kill` / `send_message`.
    pub sub_session_id: String,
    pub agent_name: String,
    pub status: DelegationStatus,
    /// Seconds since the sub-agent was spawned.
    pub elapsed_secs: u64,
}

/// Hard ceiling: no delegation may run longer than this, tool-call `timeout`
/// included — there is no per-agent override. `pub(crate)` so
/// `tools::delegate::AgentDelegateTool::preferred_timeout_secs` can use it
/// as the floor for the *generic* per-tool-call timeout `ToolExecutor`
/// applies to every tool — without that override, that outer wrapper
/// (`[agent] tool_timeout_secs`, default far below this) would silently
/// drop the whole `agent_delegate` call before a delegation ever gets to
/// run for the `timeout` its own caller actually asked for.
pub const SUB_AGENT_TIMEOUT_MAX_SECS: u64 = 1800;
