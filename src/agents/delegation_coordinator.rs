//! `DelegationCoordinator` — creates and runs specialized sub-agents on demand.
//!
//! RFC v2 §三.D: implements [`AgentDelegator`](crate::agents::AgentDelegator) by
//! running `Agent::run` invocations with restricted tool sets and
//! specialized system prompts. Also provides `delegate_async` for non-blocking
//! background execution.
//!
//! ## History persistence
//!
//! When a `parent_session_id` is supplied each sub-agent invocation gets its
//! own `JsonFileBackend` rooted at:
//!
//! ```text
//! sessions/{parent_session_id}/subagents/
//!   {sub_session_id}/
//!     meta.json
//!     history.jsonl
//!     ...          ← same structure as a top-level session, incl. compaction
//! ```
//!
//! Sub-agents therefore support context compaction and rotation identically
//! to the parent agent.  If storage cannot be opened the sub-agent runs
//! ephemerally (no history is saved).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agents::delegation::{
    AgentMail, AgentMessage, DelegationEvent, DelegationStatus, DelegationTimeout, SubAgentMailbox,
};
use crate::agents::session::{BackendPersistHook, PersistHook, SessionManager};
use crate::agents::SessionContext;
use crate::config::sub_agent::AgentIsolation;
use crate::ids::{Fqid, TYPE_SESSION, TYPE_TASK};

/// Default wall-clock timeout for a sub-agent when neither the tool caller
/// nor the SubAgentConfig specifies one.
const SUB_AGENT_TIMEOUT_DEFAULT_SECS: u64 = 600;

/// Hard ceiling: no delegation may run longer than this regardless of what
/// the tool caller or SubAgentConfig requests.
const SUB_AGENT_TIMEOUT_MAX_SECS: u64 = 1800;

/// Capacity of each running sub-agent's inbox (parent → sub messages).
const SUB_AGENT_INBOX_CAPACITY: usize = 64;

/// Resolve the effective timeout.
///
/// Priority: `tool_timeout` > `config_timeout` > `SUB_AGENT_TIMEOUT_DEFAULT_SECS`,
/// clamped to `SUB_AGENT_TIMEOUT_MAX_SECS`.
fn resolve_timeout(tool_timeout: Option<u64>, config_timeout: Option<u64>) -> u64 {
    let secs = tool_timeout.or(config_timeout).unwrap_or(SUB_AGENT_TIMEOUT_DEFAULT_SECS);
    secs.min(SUB_AGENT_TIMEOUT_MAX_SECS)
}

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
    pub session_id: String,
    pub spawned_at: std::time::Instant,
    /// Sub → parent messages delivered while running (counted in
    /// `AgentMessenger::send_to_parent`). Read at completion to decide
    /// whether the `Completed` note can skip the summary (④ summary
    /// de-duplication).
    pub messages_sent: std::sync::atomic::AtomicU64,
}

/// Snapshot view of a running-table entry for `agent_list` / logging.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunningAgentInfo {
    pub task_id: String,
    pub agent_name: String,
    pub status: DelegationStatus,
    /// Seconds since the sub-agent was spawned.
    pub elapsed_secs: u64,
}

/// Holds sub-agent configs and creates temporary `Agent::run` invocations
/// for delegation.
///
/// Implements [`AgentDelegator`](crate::agents::AgentDelegator); the legacy
/// `TaskDelegator` dual-impl was removed in H47 (callers now go through
/// `AgentDelegator`, which carries `&Session` so per-user context is
/// available end-to-end).
#[derive(Clone)]
pub struct DelegationCoordinator {
    /// Sub-agent configurations, indexed by name. Same Arc as
    /// `AgentRuntime.agents` so name → Agent lookups stay consistent.
    configs: Arc<super::AgentRegistry>,
    /// Shared SessionManager. Sub-sessions are flat peers of regular
    /// sessions (`meta.parent_session_id` is the link).
    session_manager: Arc<SessionManager>,
    /// Root directory for git worktrees (when isolation = worktree).
    worktrees_root: PathBuf,
    /// Parent AgentRuntime, set exactly once by the daemon after both this
    /// coordinator and the runtime have been built (they form a
    /// construction cycle: runtime → tools → DelegateTaskTool →
    /// coordinator → runtime). `delegate` reads the runtime from here and
    /// passes it (with workspace_dir overlaid when worktree-isolated) to
    /// `SessionContext::process_turn`. `OnceLock` encodes the set-once
    /// contract and gives lock-free reads on the hot delegate path.
    runtime_cell: Arc<OnceLock<crate::agents::AgentRuntime>>,
    /// In-flight background delegations (task_id → entry). Powers
    /// `/agent_list` (read snapshot with live status) and `/agent_kill`
    /// (abort by id).
    running: Arc<DashMap<String, RunningEntry>>,
    /// Parent → sub inboxes of running async sub-agents (task_id → sender).
    /// Registered by `spawn_delegate_async`, removed when the sub-agent
    /// finishes; powers the `send_message(recipient=task_id)` tool via
    /// `AgentMessenger::send_to_sub_agent`.
    mailboxes: Arc<DashMap<String, mpsc::Sender<AgentMail>>>,
    /// Namespace for generated FQIDs (`<ns>/t/<uuidv7>` task ids,
    /// `<ns>/s/<uuidv7>` ephemeral sub-session ids). Bound at construction
    /// from `[system] namespace`.
    namespace: String,
    /// Maximum delegation depth, counting the main agent as depth 1
    /// (RFC §6, `[delegation] max_depth`). A delegation whose child depth
    /// would exceed this limit is rejected at the tool layer before any
    /// pending task is registered or suspension created.
    max_depth: u32,
    /// Sender for `DelegationEvent`s, set once by the daemon via
    /// `set_event_sender` when wiring the orchestrator's `delegation_rx`.
    event_tx_cell: Arc<OnceLock<mpsc::Sender<DelegationEvent>>>,
}

impl DelegationCoordinator {
    pub fn new(
        configs: Arc<super::AgentRegistry>,
        session_manager: Arc<SessionManager>,
        worktrees_root: PathBuf,
        namespace: &str,
        max_depth: u32,
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
        }
    }

    /// Install the AgentRuntime that sub-agent turns will use. Called
    /// by the daemon after both the coordinator and the runtime are
    /// constructed (chicken-egg: runtime construction needs the
    /// AgentRegistry which the coordinator also references).
    pub fn set_runtime(&self, runtime: crate::agents::AgentRuntime) {
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
                    task_id: e.key().clone(),
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

    /// Wait for all background sub-agents to finish, or until `timeout` elapses.
    /// Called during hot switch drain so sub-agent sessions persist cleanly
    /// instead of being killed mid-turn by fork+execv.
    ///
    /// Unlike `TurnTracker::drain` which uses an atomic counter + Notify,
    /// this drains the `DashMap` of JoinHandles directly. Each handle is
    /// awaited with the remaining budget, so total wait is bounded by `timeout`.
    pub async fn drain(&self, timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Take ownership of handles by removing from the map. The sub-agent
            // task body normally self-removes on completion, but during drain we
            // need to own the handles to await them.
            let handles: Vec<(String, JoinHandle<()>)> = self
                .running
                .iter()
                .map(|e| e.key().clone())
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|id| {
                    self.running
                        .remove(&id)
                        .map(|(_, entry)| (id, entry.handle))
                })
                .collect();
            if handles.is_empty() {
                tracing::debug!("sub-agent drain: no running sub-agents");
                return;
            }
            let remaining = handles.len();
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if budget.is_zero() {
                tracing::warn!(
                    active_sub_agents = remaining,
                    "sub-agent drain timed out — proceeding to hot switch with running sub-agents"
                );
                return;
            }
            tracing::info!(
                active_sub_agents = remaining,
                "waiting for sub-agents to finish before hot switch"
            );
            let futs: Vec<_> = handles.into_iter().map(|(_, h)| h).collect();
            let _ = tokio::time::timeout_at(
                deadline,
                futures_util::future::join_all(futs),
            )
            .await;
        }
    }

    /// Checkpoint all running tasks to durable storage and cancel them.
    ///
    /// Called during daemon shutdown (before hot-switch fork or process exit).
    /// Each running task's checkpoint is updated to `status: "checkpointed"`
    /// so startup recovery knows the task was interrupted by shutdown, not a
    /// business failure. The tokio tasks are then aborted — their sub-session
    /// history is already persisted, so `scan_unfinished_subagents` will
    /// resume them on restart.
    ///
    /// Unlike `drain`, this does NOT wait for tasks to finish — it checkpoints
    /// and immediately aborts. The drain timeout is therefore not a business
    /// failure.
    pub fn checkpoint_and_cancel_all(&self) {
        let backend = self.session_manager.backend();
        let existing: Vec<crate::storage::DelegationCheckpoint> = backend.load_delegation_checkpoints();

        // Take ownership of all entries by removing from the map (same pattern
        // as `drain`). Each entry carries the JoinHandle we need to abort.
        let entries: Vec<(String, RunningEntry)> = self
            .running
            .iter()
            .map(|e| e.key().clone())
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|id| self.running.remove(&id).map(|(_, v)| (id, v)))
            .collect();

        for (task_id, entry) in &entries {
            let checkpoint = existing
                .iter()
                .find(|c| &c.task_id == task_id)
                .cloned()
                .map(|mut c| {
                    c.status = "checkpointed".to_string();
                    c.last_checkpoint = Some(chrono::Utc::now());
                    c
                })
                .unwrap_or_else(|| crate::storage::DelegationCheckpoint {
                    task_id: task_id.clone(),
                    parent_session_id: entry.session_id.clone(),
                    sub_session_id: String::new(),
                    agent_name: entry.agent_name.clone(),
                    status: "checkpointed".to_string(),
                    started_at: chrono::Utc::now(),
                    timeout_secs: SUB_AGENT_TIMEOUT_DEFAULT_SECS,
                    allowed_tools: None,
                    last_checkpoint: Some(chrono::Utc::now()),
                });
            if let Err(e) = backend.save_delegation_checkpoint(&checkpoint) {
                tracing::warn!(task_id = %task_id, err = %e, "shutdown checkpoint failed");
            }
        }

        // Abort all running tasks. The sub-session history is already on disk;
        // startup recovery will resume via `scan_unfinished_subagents`.
        for (_, entry) in &entries {
            entry.handle.abort();
        }
        if !entries.is_empty() {
            tracing::info!(count = entries.len(), "checkpointed and cancelled running sub-agents");
        }
    }

    /// Load all durable delegation checkpoints from the backend.
    ///
    /// Called at daemon startup to distinguish tasks interrupted by shutdown
    /// (checkpointed → resumable) from tasks that crashed without a checkpoint
    /// (potentially failed).
    pub fn load_checkpoints(&self) -> Vec<crate::storage::DelegationCheckpoint> {
        self.session_manager.backend().load_delegation_checkpoints()
    }

    /// Remove orphaned worktree directories left behind by crashed or
    /// timed-out sub-agent runs. Called once at daemon startup.
    pub fn cleanup_stale_worktrees(&self) {
        let entries = match std::fs::read_dir(&self.worktrees_root) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut cleaned = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            // Each worktree dir is named like `coder_<8hex>`.
            // `git worktree remove --force` also removes stale git worktree metadata.
            let out = std::process::Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&path)
                .current_dir(&self.worktrees_root)
                .output();
            let ok = out.as_ref().is_ok_and(|o| o.status.success());
            if !ok {
                // Fallback: remove directory directly if git doesn't know about it.
                let _ = std::fs::remove_dir_all(&path);
            }
            cleaned += 1;
        }
        // Also prune stale git worktree metadata.
        let _ = std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.worktrees_root)
            .output();
        if cleaned > 0 {
            tracing::info!(count = cleaned, "cleaned up stale sub-agent worktrees");
        }
    }

    /// Cancel a running background task by id.
    ///
    /// Marks the entry `Cancelled` before aborting so `agent_list` (or a
    /// concurrent snapshot) can observe the terminal state, then broadcasts
    /// `DelegationEvent::Failed { error: "cancelled" }` (no new variant —
    /// P1-3 decision) so a parent turn suspended on this task's terminal
    /// event resolves instead of hanging. The event send is awaited AFTER
    /// the running-table guard is dropped so the future stays `Send`.
    pub async fn cancel(&self, task_id: &str) -> bool {
        // Scope the DashMap `Ref` (shard read guard) so it drops BEFORE the
        // write-locking `remove` below — holding a `Ref` across `remove`
        // deadlocks (parking_lot RwLock is not reentrant; the writer waits
        // for the very read guard this task is still holding).
        let session_id = {
            let Some(entry) = self.running.get(task_id) else {
                return false;
            };
            let session_id = entry.session_id.clone();
            if let Ok(mut status) = entry.status.write() {
                *status = DelegationStatus::Cancelled;
            }
            entry.handle.abort();
            session_id
        };
        self.running.remove(task_id);
        // Delete the durable checkpoint — the task was cancelled by the user.
        if let Err(e) = self
            .session_manager
            .backend()
            .delete_delegation_checkpoint(task_id)
        {
            tracing::warn!(task_id, err = %e, "agent_kill: delete delegation checkpoint failed");
        }
        if let Some(tx) = self.event_sender() {
            if tx
                .send(DelegationEvent::Failed {
                    task_id: task_id.to_string(),
                    session_id,
                    error: "cancelled".to_string(),
                })
                .await
                .is_err()
            {
                tracing::warn!(task_id, "agent_kill: event channel closed, cancelled event dropped");
            }
        }
        true
    }

    fn runtime(&self) -> anyhow::Result<crate::agents::AgentRuntime> {
        self.runtime_cell
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("DelegationCoordinator: runtime not installed"))
    }

    fn find_agent(&self, name: &str) -> Option<Arc<crate::agents::Agent>> {
        self.configs.get(name)
    }

    /// Resolve a sub-session id. RFC v2: SessionManager.create_sub_session
    /// is the canonical path; an empty parent yields an ephemeral id so
    /// the caller can still run a one-shot turn without persistence.
    #[allow(dead_code)] // kept for delegate_async legacy path
    fn open_sub_session(
        &self,
        parent_session_id: &str,
        agent_name: &str,
    ) -> (String, Option<Arc<dyn PersistHook>>) {
        if parent_session_id.is_empty() {
            return (Fqid::new(&self.namespace, TYPE_SESSION).to_string(), None);
        }
        match self
            .session_manager
            .create_sub_session(parent_session_id, agent_name)
        {
            Ok(info) => {
                let hook = BackendPersistHook::new(Arc::clone(self.session_manager.backend()));
                (info.id, Some(Arc::new(hook) as Arc<dyn PersistHook>))
            }
            Err(e) => {
                tracing::warn!(parent = %parent_session_id, err = %e,
                    "failed to create sub-agent session, running ephemeral");
                (Fqid::new(&self.namespace, TYPE_SESSION).to_string(), None)
            }
        }
    }

    /// Compute the delegation depth of a session by walking the
    /// `parent_session_id` chain: the main agent session is depth 1, each
    /// sub-session hop adds 1 (RFC §6). Sessions that cannot be resolved
    /// (ephemeral, or a parent that no longer exists) are treated as depth
    /// 1 — matching the legacy guard, which only rejected when the chain
    /// was resolvable.
    fn session_depth(&self, session_id: &str) -> u32 {
        let mut depth = 1u32;
        let mut current: Option<String> = Some(session_id.to_string());
        // Defensive bound: a corrupted parent chain must not loop forever.
        for _ in 0..64 {
            let Some(id) = current else { break };
            match self.session_manager.get_by_id(&id) {
                Some(session) => match session.parent_session_id {
                    Some(parent) => {
                        depth += 1;
                        current = Some(parent);
                    }
                    None => break,
                },
                None => break,
            }
        }
        depth
    }

    /// Enforce `[delegation] max_depth` (RFC §6). The child depth of a
    /// delegation is `session_depth(parent) + 1`; when that exceeds
    /// `max_depth` the call is rejected at the tool layer — before any
    /// pending task is registered and before any suspension is created.
    fn check_depth(&self, parent_session_id: &str) -> anyhow::Result<()> {
        let child_depth = self.session_depth(parent_session_id).saturating_add(1);
        if child_depth > self.max_depth {
            anyhow::bail!(
                "maximum delegation depth exceeded: depth {} > max_depth {} \
                 (main agent = depth 1; raise [delegation] max_depth to allow deeper nesting)",
                child_depth,
                self.max_depth
            );
        }
        Ok(())
    }

    /// Core delegation logic — shared by sync and async paths.
    ///
    /// Returns a boxed future to break the async recursion cycle:
    /// delegate_with_parent → AgentLoop::run → compact_impl → summarize_inline
    /// → execute_tool → delegate_with_parent (nested sub-agent).
    #[allow(clippy::too_many_arguments)]
    pub fn delegate_with_parent<'a>(
        &'a self,
        agent_name: &'a str,
        task: &'a str,
        parent_session_id: &'a str,
        task_id_override: Option<&'a str>,
        session_key: Option<&'a str>,
        reply_target: Option<&'a str>,
        timeout_secs: u64,
        inbox: Option<SubAgentMailbox>,
        allowed_tools: Option<Vec<String>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>
    {
        Box::pin(async move {
            // Recursion depth guard (RFC §6): enforced via the
            // `parent_session_id` chain against `[delegation] max_depth`
            // (main agent = depth 1). Also a redundant backstop for the
            // async path, which pre-checks in `spawn_delegate_async` so no
            // pending task is registered for a rejected delegation.
            self.check_depth(parent_session_id)?;

            let agent = self.find_agent(agent_name).ok_or_else(|| {
                let available = self.configs.names();
                anyhow::anyhow!(
                    "Unknown sub-agent '{}'. Available: {}",
                    agent_name,
                    available.join(", ")
                )
            })?;
            let config = &agent.config;

            // H50: no marker file. Recovery scans SessionManager for sub-sessions
            // (by `meta.parent_session_id`) and checks their history shape —
            // see `agents::recovery::scan_unfinished_subagents`.
            let task_id = task_id_override
                .map(|s| s.to_string())
                .unwrap_or_else(|| Fqid::new(&self.namespace, TYPE_TASK).to_string());

            tracing::info!(
                agent = %config.name,
                task_id = %task_id,
                parent = %parent_session_id,
                tools = ?config.tools,
                task_len = task.len(),
                "creating sub-agent for delegation"
            );

            // --- worktree creation (moved BEFORE prompt so we can inject the path) ---
            let (worktree_path, cleanup_worktree, branch_name) = match config.isolation {
                AgentIsolation::Worktree => {
                    let task_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
                    let branch_name = format!("subagent/{}_{}", config.name, task_id);
                    let worktree_path = self
                        .worktrees_root
                        .join(format!("{}_{}", config.name, task_id));

                    if let Some(parent) = worktree_path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    if worktree_path.exists() {
                        let _ = std::fs::remove_dir_all(&worktree_path);
                    }

                    let output = std::process::Command::new("git")
                        .args([
                            "worktree",
                            "add",
                            "-b",
                            &branch_name,
                            &worktree_path.to_string_lossy(),
                            "HEAD",
                        ])
                        .output()
                        .map_err(|e| anyhow::anyhow!("failed to run git worktree add: {}", e))?;

                    if !output.status.success() {
                        anyhow::bail!(
                            "failed to create git worktree: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }

                    tracing::info!(
                        path = %worktree_path.display(),
                        branch = %branch_name,
                        "created git worktree for sub-agent"
                    );
                    (worktree_path, Some(task_id), Some(branch_name))
                }
                AgentIsolation::Shared => (PathBuf::new(), None, None),
            };

            let workspace_dir = if worktree_path.as_os_str().is_empty() {
                String::new()
            } else {
                worktree_path.to_string_lossy().to_string()
            };

            let workspace_section = if workspace_dir.is_empty() {
                String::new()
            } else {
                format!("\n\nWorking directory: {}", workspace_dir)
            };

            let identity = if config.system_prompt.is_empty() {
                format!(
                    "You are a specialized agent named '{}'.{}",
                    config.name, workspace_section
                )
            } else {
                format!("{}{}", config.system_prompt, workspace_section)
            };

            // session_key + reply_target args are still accepted (delegate_async
            // passes them) but no longer persisted to a marker file.
            let _ = (session_key, reply_target, &agent);

            // RFC §三.A line 404-419: sub-sessions flow through the unified
            // path — SessionManager builds a SessionContext (held only by this
            // function), session-level overrides carry the run_mode / model /
            // identity prompt, and `process_turn` does the rest.
            let sub_ctx = self
                .session_manager
                .create_sub_session_context(parent_session_id, &config.name)?;
            let sub_session_id = {
                let session = sub_ctx.session.lock().await;
                session.id.clone()
            };

            {
                let mut session = sub_ctx.session.lock().await;
                session.session_override.run_mode = Some(crate::config::agent::RunMode::Background);
                session.session_override.permission_mode =
                    Some(crate::agents::PermissionMode::Full);
                if let Some(ref m) = config.model {
                    session.session_override.model = Some(m.clone());
                }
                session.session_override.system_prompt_override = Some(identity.clone());
                // RFC agent-messaging §3: async sub-agents receive an inbox
                // (parent → sub messages) via `inbox`. The task_id identity
                // for sub→parent messages is set for BOTH sync and async
                // sub-agents (a sync sub-agent can message its parent, it
                // just cannot receive messages — no inbox).
                if let Some(mailbox) = inbox {
                    session.sub_agent_inbox = Some(Arc::new(mailbox));
                }
                session.sub_agent_task_id = Some(task_id.clone());
                session.turn_tool_allowlist = allowed_tools;
            }
            let allowlist_clone = {
                let session = sub_ctx.session.lock().await;
                session.turn_tool_allowlist.clone()
            };
            if let Err(e) = self
                .session_manager
                .backend()
                .save_delegation_args(&sub_session_id, timeout_secs, allowlist_clone.clone())
            {
                tracing::warn!(sub_session = %sub_session_id, err = %e, "persist sub-agent delegation args failed");
            }
            // P1-1: persist the task FQID so restart recovery
            // (`scan_unfinished_subagents`) can emit terminal events keyed by
            // the same id the parent's suspension recorded — without this the
            // sub-agent's hex session id can never match `pending` (FQID).
            if let Err(e) = self
                .session_manager
                .backend()
                .save_task_id(&sub_session_id, &task_id)
            {
                tracing::warn!(sub_session = %sub_session_id, task_id = %task_id, err = %e, "persist sub-agent task_id failed");
            }

            // Durable delegation checkpoint: persists task identity so the
            // daemon can resume (not mark Failed) on restart.
            let checkpoint = crate::storage::DelegationCheckpoint {
                task_id: task_id.clone(),
                parent_session_id: parent_session_id.to_string(),
                sub_session_id: sub_session_id.clone(),
                agent_name: agent_name.to_string(),
                status: "running".to_string(),
                started_at: chrono::Utc::now(),
                timeout_secs,
                allowed_tools: allowlist_clone.clone(),
                last_checkpoint: None,
            };
            if let Err(e) = self
                .session_manager
                .backend()
                .save_delegation_checkpoint(&checkpoint)
            {
                tracing::warn!(task_id = %task_id, err = %e, "persist delegation checkpoint failed");
            }

            // Snapshot the runtime; for worktree isolation, overlay the
            // working directory so file tools see the worktree path.
            let mut runtime = self.runtime()?;
            if !worktree_path.as_os_str().is_empty() {
                runtime.defaults.prompt.workspace_dir = worktree_path.to_string_lossy().to_string();
            }

            // Synthetic ChannelInboundMessage carries the delegated task. No
            // channel — sub-agent output is returned to the parent's tool call
            // via the TurnResult text.
            let synthetic = crate::channels::ChannelInboundMessage {
                id: format!("delegation:{}", task_id),
                sender: crate::channels::MessageSender::new(format!("agent:{}", config.name)),
                receiver: crate::channels::MessageReceiver::new(String::new()),
                content: crate::channels::ChannelMessageContent::text(task.to_string()),
                timestamp: chrono::Utc::now().timestamp() as u64,
                interruption_scope_id: None,
                silenced_override: None,
            };

            tracing::debug!(agent = %config.name, "sub-agent started");
            let turn_future = sub_ctx.process_turn(synthetic, None, runtime);
            let result = match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                turn_future,
            )
            .await
            {
                Ok(r) => r.map(|tr| tr.text),
                Err(_) => {
                    // Structured timeout error: the async wrapper downcasts
                    // this to emit `DelegationEvent::TimedOut` (distinct
                    // from a generic `Failed`). Dropping `turn_future` here
                    // cancels the sub-agent turn — the same cancellation
                    // semantics as aborting a spawned task.
                    tracing::warn!(
                        agent = %config.name,
                        timeout_secs,
                        "sub-agent timed out, cancelling turn"
                    );
                    Err(anyhow::Error::new(DelegationTimeout {
                        agent: config.name.clone(),
                        secs: timeout_secs,
                    }))
                }
            };

            match &result {
                Ok(text) => {
                    tracing::debug!(agent = %config.name, text_len = text.len(), "sub-agent completed")
                }
                Err(e) => tracing::warn!(agent = %config.name, err = %e, "sub-agent failed"),
            }

            // Merge sub-agent branch back into the main branch (if it committed anything).
            if let Some(ref branch_name) = branch_name {
                let diff = std::process::Command::new("git")
                    .args(["log", "--oneline", "HEAD..", branch_name])
                    .output();

                let has_commits = match diff {
                    Ok(d) => !d.stdout.is_empty(),
                    Err(_) => false,
                };

                if has_commits {
                    // Switch back to the previous branch.
                    let checkout = std::process::Command::new("git")
                        .args(["checkout", "@{-1}"])
                        .output();

                    if let Ok(co) = checkout {
                        if co.status.success() {
                            let merge = std::process::Command::new("git")
                                .args([
                                    "merge",
                                    "--no-ff",
                                    "-m",
                                    &format!("merge sub-agent: {}", config.name),
                                    branch_name,
                                ])
                                .output();

                            match merge {
                                Ok(m) if !m.status.success() => {
                                    // Capture conflicted files for diagnostics.
                                    let conflicts = std::process::Command::new("git")
                                        .args(["diff", "--name-only", "--diff-filter=U"])
                                        .output();
                                    let conflict_files = match conflicts {
                                        Ok(c) => String::from_utf8_lossy(&c.stdout)
                                            .trim()
                                            .to_string(),
                                        Err(_) => String::new(),
                                    };
                                    tracing::warn!(
                                        branch = %branch_name,
                                        stderr = %String::from_utf8_lossy(&m.stderr),
                                        conflict_files = %conflict_files,
                                        "merge conflict — aborting merge, worktree preserved"
                                    );
                                    let _ = std::process::Command::new("git")
                                        .args(["merge", "--abort"])
                                        .output();
                                    let detail = if conflict_files.is_empty() {
                                        String::new()
                                    } else {
                                        format!("\nConflicted files:\n{}", conflict_files)
                                    };
                                    return Err(anyhow::anyhow!(
                                        "sub-agent '{}' completed but merge failed (conflict). Worktree preserved at {}.{}",
                                        config.name,
                                        worktree_path.display(),
                                        detail
                                    ));
                                }
                                Err(e) => {
                                    tracing::warn!(branch = %branch_name, err = %e, "failed to run git merge");
                                }
                                _ => {
                                    tracing::debug!(branch = %branch_name, "merged sub-agent branch");
                                }
                            }
                        } else {
                            tracing::warn!(
                                branch = %branch_name,
                                stderr = %String::from_utf8_lossy(&co.stderr),
                                "failed to checkout previous branch"
                            );
                        }
                    }
                } else {
                    tracing::debug!(branch = %branch_name, "no new commits, skipping merge");
                }
            }

            // Cleanup worktree + branch on any exit path.
            // (Merge conflicts already returned early above, preserving the worktree.)
            if cleanup_worktree.is_some() {
                let _ = std::process::Command::new("git")
                    .args([
                        "worktree",
                        "remove",
                        "--force",
                        &worktree_path.to_string_lossy(),
                    ])
                    .output();
                if let Some(ref bn) = branch_name {
                    let _ = std::process::Command::new("git")
                        .args(["branch", "-D", bn])
                        .output();
                }
                tracing::debug!(path = %worktree_path.display(), "cleaned up worktree and branch");
            }

            // H50: no marker file to clean up. Sub-session completion clears
            // `incomplete_turn` via the standard turn-end persistence path; the
            // session is then no longer flagged as needing recovery.

            // RFC agent-messaging §3.4 (A5): messages that arrived after the
            // sub-agent stopped consuming its inbox must not be silently lost.
            // Drain whatever is left and attach it to the result — the async
            // path then carries it back in `DelegationEvent::Completed`.
            let undelivered: Vec<String> = {
                let session = sub_ctx.session.lock().await;
                match &session.sub_agent_inbox {
                    Some(mailbox) => {
                        let mut rx = mailbox.rx.lock().await;
                        let mut out = Vec::new();
                        while let Ok(mail) = rx.try_recv() {
                            out.push(mail.text);
                        }
                        out
                    }
                    None => Vec::new(),
                }
            };
            let result = match result {
                Ok(text) => {
                    if undelivered.is_empty() {
                        Ok(text)
                    } else {
                        tracing::warn!(
                            task_id = %task_id,
                            count = undelivered.len(),
                            "sub-agent finished with unread parent messages; attaching to result"
                        );
                        Ok(format!(
                            "{}\n\n[主 agent 有 {} 条消息在任务结束后到达，未处理]：\n{}",
                            text,
                            undelivered.len(),
                            undelivered.join("\n---\n")
                        ))
                    }
                }
                Err(e) => {
                    if undelivered.is_empty() {
                        Err(e)
                    } else {
                        Err(anyhow::anyhow!(
                            "{} (另有 {} 条主 agent 消息在任务结束后到达，未处理：{})",
                            e,
                            undelivered.len(),
                            undelivered.join(" | ")
                        ))
                    }
                }
            };

            // GC: delete the sub-session for sync delegations. The result is
            // already captured in `result` and returned to the parent's tool
            // call, so the sub-session history is no longer needed.
            if result.is_ok() {
                if let Err(e) = self.session_manager.backend().delete_session(&sub_session_id) {
                    tracing::debug!(sub_session = %sub_session_id, err = %e, "failed to GC sub-session");
                }
            }

            result
        }) // end Box::pin
    }

    /// Delegate a task asynchronously — spawns the sub-agent in a
    /// background tokio task whose JoinHandle is stashed in `running`
    /// so `/agent_list` and `/agent_kill` can see it.
    pub fn spawn_delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        parent_session_id: &str,
        reply_target: &str,
        timeout_secs: u64,
        allowed_tools: Option<Vec<String>>,
    ) -> anyhow::Result<String> {
        let agent = self.find_agent(agent_name).ok_or_else(|| {
            let available = self.configs.names();
            anyhow::anyhow!(
                "Unknown sub-agent '{}'. Available: {}",
                agent_name,
                available.join(", ")
            )
        })?;
        let config = &agent.config;

        let task_id = Fqid::new(&self.namespace, TYPE_TASK).to_string();

        // Recursion depth guard (RFC §6): reject synchronously at the tool
        // layer BEFORE registering the pending task — a rejected delegation
        // must not leave a hanging pending task / suspension behind. The
        // same check inside `delegate_with_parent` (async task body) is a
        // redundant backstop.
        self.check_depth(parent_session_id)?;

        // 方案 C (docs/turn-suspension-rfc.md): register the task against the
        // parent's registered SessionContext so the running turn knows to
        // suspend when the LLM ends it. Only the orchestrator's active
        // top-level sessions are registered; sub-agent sessions and
        // switched-away sessions return None and simply don't suspend.
        if let Some(parent_ctx) = self
            .session_manager
            .registered_context_by_session_id(parent_session_id)
        {
            parent_ctx.add_pending_task(task_id.clone());
        }

        tracing::info!(
            agent = %config.name,
            task_id = %task_id,
            task_len = task.len(),
            timeout_secs,
            "spawning sub-agent in background"
        );

        let sub_delegator = self.clone();
        let task_owned = task.to_string();
        let parent_session_id_owned = parent_session_id.to_string();
        let session_key_owned = parent_session_id.to_string();
        let reply_target_owned = reply_target.to_string();
        let event_tx = self.event_sender();
        let task_id_clone = task_id.clone();
        let agent_name_owned = agent_name.to_string();
        let running = Arc::clone(&self.running);
        let running_task_id = task_id.clone();

        // RFC agent-messaging §3: register the sub-agent's inbox so the
        // parent's `send_message(recipient=task_id)` can reach it. The
        // sender is dropped when the map entry is removed at completion.
        let (mail_tx, mail_rx) = mpsc::channel(SUB_AGENT_INBOX_CAPACITY);
        let mailbox = SubAgentMailbox {
            tx: mail_tx.clone(),
            rx: tokio::sync::Mutex::new(mail_rx),
        };
        self.mailboxes.insert(task_id.clone(), mail_tx);
        let mailboxes = Arc::clone(&self.mailboxes);

        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            let result = sub_delegator
                .delegate_with_parent(
                    &agent_name_owned,
                    &task_owned,
                    &parent_session_id_owned,
                    Some(&task_id_clone),
                    Some(&session_key_owned),
                    Some(&reply_target_owned),
                    timeout_secs,
                    Some(mailbox),
                    allowed_tools,
                )
                .await;

            let duration_secs = start_time.elapsed().as_secs();

            // Classify the outcome: a wall-clock timeout is distinct from a
            // generic failure — the parent must know the task was abandoned
            // mid-flight, not finished-with-error.
            let timed_out_secs = result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<DelegationTimeout>())
                .map(|t| t.secs);

            let session_id = parent_session_id_owned.clone();
            // Count of sub → parent messages delivered while running. The
            // parent session has already received them as `DelegationEvent::
            // Message`, so a non-zero count lets `wake` skip the summary in
            // the completion note (de-duplication, ④).
            let sent_message_count = running
                .get(&running_task_id)
                .map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);
            if let Some(tx) = event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        tracing::info!(task_id = %task_id_clone, duration_secs, sent_message_count, "sub-agent completed successfully");
                        let _ = tx
                            .send(DelegationEvent::Completed {
                                task_id: task_id_clone.clone(),
                                session_id,
                                summary: summary.clone(),
                                duration_secs,
                                sent_message_count,
                            })
                            .await;
                    }
                    (Err(_), Some(secs)) => {
                        tracing::warn!(task_id = %task_id_clone, timeout_secs = secs, duration_secs, "sub-agent timed out");
                        let _ = tx
                            .send(DelegationEvent::TimedOut {
                                task_id: task_id_clone.clone(),
                                session_id,
                                timeout_secs: secs,
                                duration_secs,
                            })
                            .await;
                    }
                    (Err(e), None) => {
                        tracing::warn!(task_id = %task_id_clone, duration_secs, err = %e, "sub-agent failed");
                        let _ = tx
                            .send(DelegationEvent::Failed {
                                task_id: task_id_clone.clone(),
                                session_id,
                                error: e.to_string(),
                            })
                            .await;
                    }
                }
            }

            // Mark the terminal status on the running-table entry (briefly
            // observable by a concurrent `agent_list`), then self-remove.
            let terminal = if timed_out_secs.is_some() {
                DelegationStatus::TimedOut
            } else if result.is_ok() {
                DelegationStatus::Completed
            } else {
                DelegationStatus::Failed
            };
            if let Some(entry) = running.get(&running_task_id) {
                if let Ok(mut status) = entry.status.write() {
                    *status = terminal;
                }
            }
            running.remove(&running_task_id);
            // Drop the mailbox sender: the sub-agent is gone, so
            // `send_message(recipient=task_id)` will now report the task_id
            // as unknown instead of queueing into a dead channel.
            mailboxes.remove(&running_task_id);
            // Delete the durable checkpoint — the task reached a terminal
            // state, so there's nothing to resume on restart.
            if let Err(e) = sub_delegator
                .session_manager
                .backend()
                .delete_delegation_checkpoint(&running_task_id)
            {
                tracing::warn!(task_id = %running_task_id, err = %e, "delete delegation checkpoint failed");
            }
        });

        self.running.insert(
            task_id.clone(),
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: agent_name.to_string(),
                session_id: parent_session_id.to_string(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
            },
        );
        Ok(task_id)
    }

    pub fn recover_async(
        &self,
        task_id: String,
        agent_name: String,
        parent_session_id: String,
        sub_ctx: Arc<SessionContext>,
        timeout_secs: u64,
        allowed_tools: Option<Vec<String>>,
    ) {
        let (mail_tx, mail_rx) = mpsc::channel(SUB_AGENT_INBOX_CAPACITY);
        let mailbox = SubAgentMailbox {
            tx: mail_tx.clone(),
            rx: tokio::sync::Mutex::new(mail_rx),
        };
        self.mailboxes.insert(task_id.clone(), mail_tx);
        let mailboxes = Arc::clone(&self.mailboxes);

        let running = Arc::clone(&self.running);
        let event_tx = self.event_sender();
        let running_task_id = task_id.clone();
        let task_id_clone = task_id.clone();
        let session_id = parent_session_id.clone();
        let backend = Arc::clone(self.session_manager.backend());

        let runtime = match self.runtime() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("recover_async failed to get runtime: {}", e);
                return;
            }
        };

        let agent_name_clone = agent_name.clone();
        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            {
                let mut session = sub_ctx.session.lock().await;
                session.sub_agent_inbox = Some(Arc::new(mailbox));
                session.turn_tool_allowlist = allowed_tools;
            }

            let turn_future = async {
                let _turn_guard = sub_ctx.turn_lock.lock().await;
                let mut session = sub_ctx.session.lock().await;
                let resolved = crate::agents::orchestrator::turn::ResolvedTurn::resolve(&session, &runtime);
                let turn_ctx = resolved.turn_context();
                sub_ctx.agent.run_recovery(&mut session, turn_ctx, &runtime).await
            };

            let result = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), turn_future).await {
                Ok(Ok(Some(tr))) if !tr.text.is_empty() => Ok(tr.text),
                Ok(Ok(_)) => Err(anyhow::anyhow!("no recovery needed or empty text")),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow::anyhow!(DelegationTimeout { agent: agent_name_clone, secs: timeout_secs })),
            };

            let duration_secs = start_time.elapsed().as_secs();
            let timed_out_secs = result.as_ref().err().and_then(|e| e.downcast_ref::<DelegationTimeout>()).map(|t| t.secs);
            let sent_message_count = running.get(&running_task_id).map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);

            if let Some(tx) = event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        let _ = tx.send(DelegationEvent::Completed { task_id: task_id_clone, session_id, summary: summary.clone(), duration_secs, sent_message_count }).await;
                    }
                    (Err(_), Some(secs)) => {
                        let _ = tx.send(DelegationEvent::TimedOut { task_id: task_id_clone, session_id, timeout_secs: secs, duration_secs }).await;
                    }
                    (Err(e), None) => {
                        let _ = tx.send(DelegationEvent::Failed { task_id: task_id_clone, session_id, error: e.to_string() }).await;
                    }
                }
            }

            let terminal = if timed_out_secs.is_some() { DelegationStatus::TimedOut } else if result.is_ok() { DelegationStatus::Completed } else { DelegationStatus::Failed };
            if let Some(entry) = running.get(&running_task_id) {
                if let Ok(mut status) = entry.status.write() { *status = terminal; }
            }
            running.remove(&running_task_id);
            mailboxes.remove(&running_task_id);
            // Delete the durable checkpoint — the recovered task reached a
            // terminal state.
            if let Err(e) = backend.delete_delegation_checkpoint(&running_task_id) {
                tracing::warn!(task_id = %running_task_id, err = %e, "delete delegation checkpoint failed");
            }
        });

        self.running.insert(
            task_id,
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name,
                session_id: parent_session_id,
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
            },
        );
    }
}

/// `DelegationCoordinator` implements the canonical [`AgentDelegator`] trait
/// (the legacy `TaskDelegator` dual-impl was removed in H47). The `delegate`
/// method here carries the parent `&Session` so callees can read
/// `reply_target`, `owner` (for per-user scoping), and `last_message` from
/// the same value.
#[async_trait::async_trait]
impl crate::agents::AgentDelegator for DelegationCoordinator {
    async fn delegate(
        &self,
        agent_name: &str,
        task: &str,
        parent_session: &super::session::Session,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
    ) -> anyhow::Result<String> {
        let reply_target = parent_session.reply_target().map(|s| s.to_string());
        let config_timeout = self
            .find_agent(agent_name)
            .and_then(|a| a.config.timeout);
        let timeout_secs = resolve_timeout(timeout, config_timeout);
        self.delegate_with_parent(
            agent_name,
            task,
            &parent_session.id,
            None,
            None,
            reply_target.as_deref(),
            timeout_secs,
            None,
            allowed_tools,
        )
        .await
    }

    fn delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        parent_session: &super::session::Session,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
    ) -> anyhow::Result<String> {
        let reply_target = parent_session
            .reply_target()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let config_timeout = self
            .find_agent(agent_name)
            .and_then(|a| a.config.timeout);
        let timeout_secs = resolve_timeout(timeout, config_timeout);
        self.spawn_delegate_async(agent_name, task, &parent_session.id, &reply_target, timeout_secs, allowed_tools)
    }

    fn list_available(&self) -> Vec<(String, Option<String>)> {
        self.configs
            .values_cloned()
            .into_iter()
            .map(|a| (a.config.name.clone(), a.config.description.clone()))
            .collect()
    }
}

/// Agent-to-agent message bus (RFC agent-messaging §3).
///
/// `send_to_sub_agent` routes through the per-task inbox registered by
/// `spawn_delegate_async`; `send_to_parent` reuses the `DelegationEvent`
/// channel so the orchestrator wakes the parent session exactly like a
/// completion event (queued behind the turn lock — never preempting).
#[async_trait::async_trait]
impl crate::agents::AgentMessenger for DelegationCoordinator {
    fn send_to_sub_agent(&self, task_id: &str, mail: AgentMail) -> Result<(), String> {
        match self.mailboxes.get(task_id) {
            Some(tx) => tx
                .try_send(mail)
                .map_err(|e| format!("消息投递失败（子代理收件箱已满或已关闭）：{}", e)),
            None => Err(format!(
                "task_id '{}' 不存在或子代理已结束（仅 async 子代理可接收消息）",
                task_id
            )),
        }
    }

    async fn send_to_parent(&self, event: AgentMessage) -> bool {
        match self.event_sender() {
            Some(tx) => {
                let task_id = event.task_id.clone();
                let delivered = tx.send(DelegationEvent::Message(event)).await.is_ok();
                if delivered {
                    // Bump the per-task message counter so the completion
                    // wrapper can de-duplicate the summary (④).
                    if let Some(entry) = self.running.get(&task_id) {
                        entry
                            .messages_sent
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                delivered
            }
            None => {
                tracing::warn!(
                    task_id = %event.task_id,
                    "cannot deliver sub-agent message: delegation event channel not wired"
                );
                false
            }
        }
    }
}

/// P1-4: DelegationCoordinator unit tests — depth gating, async spawn
/// registration, timeout resolution, and cancel-broadcast semantics. Tests
/// reach private internals (`running` table, `check_depth`, `session_depth`,
/// `resolve_timeout`) as descendants of this module.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::session::SessionManager;
    use crate::agents::AgentRegistry;
    use crate::config::sub_agent::SubAgentConfig;

    fn coder_config() -> SubAgentConfig {
        SubAgentConfig {
            name: "coder".to_string(),
            system_prompt: "You are a coding specialist.".to_string(),
            tools: crate::config::filters::ToolFilter::all(),
            skills: crate::config::filters::SkillFilter::all(),
            mcp: crate::config::filters::McpFilter::all(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::Shared,
            timeout: None,
        }
    }

    fn coordinator(max_depth: u32) -> (DelegationCoordinator, Arc<SessionManager>) {
        let registry = Arc::new(AgentRegistry::from_vec(vec![coder_config()]));
        let manager = Arc::new(SessionManager::in_memory());
        let dc = DelegationCoordinator::new(
            registry,
            Arc::clone(&manager),
            PathBuf::new(),
            "test",
            max_depth,
        );
        (dc, manager)
    }

    #[test]
    fn resolve_timeout_priority_fallback_and_clamp() {
        // default: system fallback 600s
        assert_eq!(resolve_timeout(None, None), 600);
        // tool value wins over config regardless of ordering
        assert_eq!(resolve_timeout(Some(100), Some(50)), 100);
        assert_eq!(resolve_timeout(Some(100), Some(200)), 100);
        // config used when tool doesn't specify
        assert_eq!(resolve_timeout(None, Some(300)), 300);
        // hard ceiling 1800s
        assert_eq!(resolve_timeout(Some(5000), None), 1800);
        assert_eq!(resolve_timeout(Some(5000), Some(9000)), 1800);
        // 0 passes through (no lower clamp)
        assert_eq!(resolve_timeout(Some(0), None), 0);
    }

    #[test]
    fn unknown_session_depth_falls_back_to_one() {
        let (dc, _m) = coordinator(3);
        assert_eq!(dc.session_depth("no-such-session"), 1);
        assert!(dc.check_depth("no-such-session").is_ok());
    }

    #[test]
    fn check_depth_three_level_chain_boundary() {
        let (dc, manager) = coordinator(3);
        let main = manager.get_or_create_context("mock:default:u1");
        let main_id = main.session_id.clone();
        assert!(dc.check_depth(&main_id).is_ok());
        let sub1 = manager.create_sub_session(&main_id, "coder").unwrap();
        assert!(dc.check_depth(&sub1.id).is_ok());
        let sub2 = manager.create_sub_session(&sub1.id, "coder").unwrap();
        let err = dc.check_depth(&sub2.id).unwrap_err();
        assert!(
            err.to_string().contains("maximum delegation depth exceeded"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn max_depth_one_rejects_all() {
        let (dc, manager) = coordinator(1);
        let main = manager.get_or_create_context("mock:default:u1");
        let err = dc.check_depth(&main.session_id).unwrap_err();
        assert!(err.to_string().contains("maximum delegation depth exceeded"));
    }

    #[tokio::test]
    async fn spawn_async_registers_pending_and_running() {
        let (dc, manager) = coordinator(3);
        let parent = manager.get_or_create_context("mock:default:u1");
        let parent_id = parent.session_id.clone();
        let task_id = dc
            .spawn_delegate_async("coder", "do the thing", &parent_id, "", 60, None)
            .unwrap();
        assert!(task_id.contains("/t/"), "task_id should be an FQID: {task_id}");
        // current_thread runtime: the spawned body has not been polled yet, so
        // both tables are deterministically populated. The test body never
        // awaits, so the background task is cancelled at runtime drop.
        assert_eq!(dc.running_snapshot(), vec![task_id.clone()]);
        assert_eq!(dc.running_count(), 1);
        let snap = parent.suspension_snapshot().unwrap();
        assert_eq!(snap.pending, vec![task_id]);
    }

    #[tokio::test]
    async fn spawn_async_depth_rejected_without_pending_or_running() {
        let (dc, manager) = coordinator(1);
        let parent = manager.get_or_create_context("mock:default:u1");
        let err = dc
            .spawn_delegate_async("coder", "task", &parent.session_id, "", 60, None)
            .unwrap_err();
        assert!(err.to_string().contains("maximum delegation depth exceeded"));
        assert!(parent.suspension_snapshot().is_none());
        assert!(dc.running_snapshot().is_empty());
    }

    #[tokio::test]
    async fn spawn_async_unknown_agent_is_rejected() {
        let (dc, manager) = coordinator(3);
        let parent = manager.get_or_create_context("mock:default:u1");
        let err = dc
            .spawn_delegate_async("nope", "task", &parent.session_id, "", 60, None)
            .unwrap_err();
        assert!(err.to_string().contains("Unknown sub-agent"));
        assert!(dc.running_snapshot().is_empty());
    }

    #[tokio::test]
    async fn cancel_broadcasts_failed_cancelled_to_parent_session() {
        let (dc, manager) = coordinator(3);
        let parent = manager.get_or_create_context("mock:default:u1");
        let (tx, mut rx) = mpsc::channel(8);
        dc.set_event_sender(tx);

        // Hand-instrument a running entry so the test doesn't depend on the
        // background spawn path.
        let task_id = Fqid::new("test", TYPE_TASK).to_string();
        dc.running.insert(
            task_id.clone(),
            RunningEntry {
                handle: tokio::spawn(async {}),
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                session_id: parent.session_id.clone(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
            },
        );

        assert!(dc.cancel(&task_id).await);
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Failed {
                task_id: t,
                session_id: s,
                error,
            } => {
                assert_eq!(t, task_id);
                assert_eq!(s, parent.session_id);
                assert_eq!(error, "cancelled");
            }
            other => panic!("expected Failed(cancelled), got {other:?}"),
        }
        assert!(dc.running_snapshot().is_empty());
    }

    #[tokio::test]
    async fn cancel_unknown_task_returns_false() {
        let (dc, _m) = coordinator(3);
        assert!(!dc.cancel("no-such-task").await);
    }

    #[test]
    fn checkpoint_roundtrip_via_backend() {
        let (dc, manager) = coordinator(3);
        let _ = dc; // not needed — we test the backend directly
        let backend = manager.backend();
        let cp = crate::storage::DelegationCheckpoint {
            task_id: "test/t/abc".to_string(),
            parent_session_id: "test/s/parent".to_string(),
            sub_session_id: "test/s/sub".to_string(),
            agent_name: "coder".to_string(),
            status: "running".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: 300,
            allowed_tools: Some(vec!["shell".to_string()]),
            last_checkpoint: None,
        };
        backend.save_delegation_checkpoint(&cp).unwrap();

        let loaded = backend.load_delegation_checkpoints();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].task_id, "test/t/abc");
        assert_eq!(loaded[0].agent_name, "coder");
        assert_eq!(loaded[0].timeout_secs, 300);

        backend.delete_delegation_checkpoint("test/t/abc").unwrap();
        assert!(backend.load_delegation_checkpoints().is_empty());
    }

    #[tokio::test]
    async fn checkpoint_and_cancel_all_empties_running_and_writes_checkpoints() {
        let (dc, manager) = coordinator(3);
        let backend = manager.backend();

        // Insert a hand-crafted running entry.
        let task_id = Fqid::new("test", TYPE_TASK).to_string();
        dc.running.insert(
            task_id.clone(),
            RunningEntry {
                handle: tokio::spawn(async {}),
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                session_id: "parent".to_string(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
            },
        );
        assert_eq!(dc.running_count(), 1);

        dc.checkpoint_and_cancel_all();

        // Running table should be empty.
        assert_eq!(dc.running_count(), 0);
        // Checkpoint should be persisted with status "checkpointed".
        let loaded = backend.load_delegation_checkpoints();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status, "checkpointed");
        assert_eq!(loaded[0].task_id, task_id);
    }

    #[test]
    fn load_checkpoints_returns_persisted_checkpoints() {
        let (dc, manager) = coordinator(3);
        let backend = manager.backend();
        let cp = crate::storage::DelegationCheckpoint {
            task_id: "test/t/xyz".to_string(),
            parent_session_id: "parent".to_string(),
            sub_session_id: "sub".to_string(),
            agent_name: "coder".to_string(),
            status: "checkpointed".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: 120,
            allowed_tools: None,
            last_checkpoint: Some(chrono::Utc::now()),
        };
        backend.save_delegation_checkpoint(&cp).unwrap();

        let loaded = dc.load_checkpoints();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].task_id, "test/t/xyz");
        assert_eq!(loaded[0].status, "checkpointed");
    }
}
