//! Running-table registry: `RunningEntry` plus the coordinator's
//! construction/setters/snapshot/cancel accessors.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agents::delegation::{DelegationEvent, DelegationStatus};
use crate::agents::session::SessionManager;
use crate::api::delegation::RunningAgentInfo;

use super::DelegationCoordinator;

/// One entry in the coordinator's running table.
///
/// Status transitions:
/// - spawn: `Running`
/// - wrapper exit: `Completed` / `Failed` / `TimedOut` (marked just before
///   the entry is removed so the terminal state is observable)
/// - `cancel` (agent_kill): `Cancelled` before abort
///
/// `Idle` is reserved (see [`DelegationStatus`]) — no live entry takes it
/// today.
pub struct RunningEntry {
    pub handle: JoinHandle<()>,
    pub status: std::sync::RwLock<DelegationStatus>,
    pub agent_name: String,
    /// Parent session id (FQID) this task was spawned from — kept so
    /// `cancel` (agent_kill) can broadcast the terminal
    /// `DelegationEvent::Failed { error: "cancelled" }` to the right
    /// session (the parent may be suspended waiting on this task).
    pub parent_session_id: String,
    pub spawned_at: std::time::Instant,
    /// Sub → parent messages delivered while running (counted in
    /// `AgentMessenger::send_to_parent`). Read at completion to decide
    /// whether the `Completed` note can skip the summary (④ summary
    /// de-duplication).
    pub messages_sent: std::sync::atomic::AtomicU64,
    /// The effective timeout (seconds) requested for this delegation.
    /// Used by `checkpoint_and_cancel_all`'s fallback when no durable
    /// checkpoint exists, so the resumed task gets the same budget
    /// instead of a hardcoded 600 s default.
    pub timeout_secs: Option<u64>,
    /// Wall-clock UTC timestamp when the task was spawned. Used together
    /// with `timeout_secs` to compute remaining time on recovery.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Tool allowlist requested for this delegation; used by the
    /// `checkpoint_and_cancel_all` fallback so a resumed task keeps the
    /// same toolset instead of `None`.
    pub allowed_tools: Option<Vec<String>>,
}

impl DelegationCoordinator {
    pub fn new(
        configs: Arc<crate::agents::AgentRegistry>,
        session_manager: Arc<SessionManager>,
        worktrees_root: PathBuf,
        namespace: &str,
        max_depth: u32,
        shell_registry: crate::tools::shell::ShellRegistry,
    ) -> Self {
        Self {
            configs,
            session_manager,
            worktrees_root,
            runtime_cell: Arc::new(OnceLock::new()),
            running: Arc::new(DashMap::new()),
            mailboxes: Arc::new(DashMap::new()),
            namespace: namespace.to_string(),
            max_depth,
            event_tx_cell: Arc::new(OnceLock::new()),
            sent_messages: Arc::new(DashMap::new()),
            shell_registry,
        }
    }

    /// Install the AgentRuntime that sub-agent turns will use. Called
    /// by the daemon after both the coordinator and the runtime are
    /// constructed (chicken-egg: runtime construction needs the
    /// AgentRegistry which the coordinator also references).
    pub fn set_runtime(&self, runtime: crate::agents::runtime::AgentRuntime) {
        // Set-once; a second call (should not happen) is ignored.
        let _ = self.runtime_cell.set(runtime);
    }

    /// Install the mpsc sender for `DelegationEvent`s. Called by daemon
    /// when wiring the orchestrator's `delegation_rx` receiver.
    pub fn set_event_sender(&self, tx: mpsc::Sender<DelegationEvent>) {
        // Set-once; a second call (should not happen) is ignored.
        let _ = self.event_tx_cell.set(tx);
    }

    /// Get a clone of the installed event sender, if any. Used by
    /// background spawns (including startup recovery) to emit
    /// `DelegationEvent::Completed` / `Failed` to the orchestrator
    /// event loop.
    pub fn event_sender(&self) -> Option<mpsc::Sender<DelegationEvent>> {
        self.event_tx_cell.get().cloned()
    }

    /// Snapshot of currently-running background task ids.
    pub fn running_snapshot(&self) -> Vec<String> {
        self.running.iter().map(|e| e.key().clone()).collect()
    }

    /// Snapshot of running-table entries with live status, agent name and
    /// elapsed time. Backs `/agent_list`.
    pub fn running_records(&self) -> Vec<RunningAgentInfo> {
        self.running
            .iter()
            .map(|e| {
                let entry = e.value();
                RunningAgentInfo {
                    sub_session_id: e.key().clone(),
                    agent_name: entry.agent_name.clone(),
                    status: match entry.status.read() {
                        Ok(guard) => *guard,
                        Err(_) => DelegationStatus::Running,
                    },
                    elapsed_secs: entry.spawned_at.elapsed().as_secs(),
                }
            })
            .collect()
    }

    /// Number of currently-running background sub-agents.
    pub fn running_count(&self) -> usize {
        self.running.len()
    }



    /// Cancel a running background task by id.
    ///
    /// Marks the entry `Cancelled` before aborting so `agent_list` (or a
    /// concurrent snapshot) can observe the terminal state, then broadcasts
    /// `DelegationEvent::Failed { error: "cancelled" }` (no new variant —
    /// P1-3 decision) so a parent turn suspended on this task's terminal
    /// event resolves instead of hanging. The event send is awaited AFTER
    /// the running-table guard is dropped so the future stays `Send`.
    pub async fn cancel(&self, sub_session_id: &str) -> bool {
        // Scope the DashMap `Ref` (shard read guard) so it drops BEFORE the
        // write-locking `remove` below — holding a `Ref` across `remove`
        // deadlocks (parking_lot RwLock is not reentrant; the writer waits
        // for the very read guard this task is still holding).
        let parent_session_id = {
            let Some(entry) = self.running.get(sub_session_id) else {
                return false;
            };
            let parent_session_id = entry.parent_session_id.clone();
            if let Ok(mut status) = entry.status.write() {
                *status = DelegationStatus::Cancelled;
            }
            // Tombstone BEFORE abort: a crash in the window between abort and
            // the tombstone write would leave a "running" checkpoint that
            // restart resumes (duplicate execution). The update is idempotent
            // on the checkpoint created at spawn.
            self.persist_terminal_checkpoint(sub_session_id, DelegationStatus::Cancelled);
            entry.handle.abort();
            parent_session_id
        };
        // `abort()` only reaches the task driving the sub-agent's LLM loop —
        // any shell process it started keeps running otherwise (see
        // `kill_processes_for_session` docs).
        crate::tools::shell::kill_processes_for_session(&self.shell_registry, sub_session_id).await;
        self.running.remove(sub_session_id);
        if let Some(tx) = self.event_sender() {
            if tx
                .send(DelegationEvent::Failed {
                    sub_session_id: sub_session_id.to_string(),
                    parent_session_id,
                    error: "cancelled".to_string(),
                })
                .await
                .is_err()
            {
                tracing::warn!(sub_session_id, "agent_kill: event channel closed, cancelled event dropped");
            }
        }
        true
    }

    pub fn runtime(&self) -> anyhow::Result<crate::agents::runtime::AgentRuntime> {
        self.runtime_cell
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("DelegationCoordinator: runtime not installed"))
    }

    pub(super) fn find_agent(&self, name: &str) -> Option<Arc<crate::agents::Agent>> {
        self.configs.get(name)
    }
}
