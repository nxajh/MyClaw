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

pub(crate) mod checkpoint;
pub(crate) mod worktree;
use worktree::worktree_branch_name;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agents::delegation::{
    AgentMail, AgentMessage, DelegationEvent, DelegationStatus, DelegationTimeout, MessageKind,
    SubAgentMailbox,
};
use crate::agents::session::{BackendPersistHook, PersistHook, SessionManager};
use crate::agents::SessionContext;
use crate::config::sub_agent::AgentIsolation;
use crate::ids::{Fqid, TYPE_SESSION};

/// Default wall-clock timeout for a sub-agent when neither the tool caller
/// nor the SubAgentConfig specifies one. Sized to cover ~2 CI rounds
/// (5-7 min each) plus margin — shorter defaults starve code-modifying
/// sub-agents on CI-only hosts.
const SUB_AGENT_TIMEOUT_DEFAULT_SECS: u64 = 1200;

/// Hard ceiling: no delegation may run longer than this, tool-call `timeout`
/// included — there is no per-agent override. `pub(crate)` so
/// `tools::delegate::AgentDelegateTool::preferred_timeout_secs` can use it
/// as the floor for the *generic* per-tool-call timeout `ToolExecutor`
/// applies to every tool — without that override, that outer wrapper
/// (`[agent] tool_timeout_secs`, default far below this) would silently
/// drop the whole `agent_delegate` call before a delegation ever gets to
/// run for the `timeout` its own caller actually asked for.
pub const SUB_AGENT_TIMEOUT_MAX_SECS: u64 = 1800;

/// Capacity of each running sub-agent's inbox (parent → sub messages).
const SUB_AGENT_INBOX_CAPACITY: usize = 64;

/// Resolve the effective timeout.
///
/// Priority: `tool_timeout` > `config_timeout` > `SUB_AGENT_TIMEOUT_DEFAULT_SECS`,
/// clamped only to the global `SUB_AGENT_TIMEOUT_MAX_SECS` hard ceiling.
/// There is deliberately no per-agent override of that ceiling anymore
/// (removed 2026-08-19) — it let `SubAgentConfig.max_timeout` silently clamp
/// an explicit `agent_delegate` tool-call `timeout` down with no visibility
/// to the caller, which looked indistinguishable from the parameter being
/// ignored outright. The tool-call value is now authoritative up to the
/// global ceiling, full stop.
fn resolve_timeout(tool_timeout: Option<u64>, config_timeout: Option<u64>) -> u64 {
    tool_timeout
        .or(config_timeout)
        .unwrap_or(SUB_AGENT_TIMEOUT_DEFAULT_SECS)
        .min(SUB_AGENT_TIMEOUT_MAX_SECS)
}

/// Whether `delegate_with_parent` should GC (delete) the sub-session's own
/// history immediately after it reaches a terminal state (issue #106).
///
/// Only for a *successful sync* delegation: the sync caller already has the
/// full result text folded into its tool-call return, and there is no other
/// consumer that would ever look the sub-session up again, so keeping its
/// history around is pure disk waste. Every other case must keep it:
/// - Failed/timed-out sync delegations are deliberately NOT GC'd here (the
///   caller sees the error text, but a failed/timed-out sub-session's
///   checkpoint is kept as a resumable tombstone by the code right after
///   this call — deleting the session history out from under that would
///   break `agent_resume`).
/// - Async delegations must never be GC'd on this path, success or not:
///   the result also reaches the parent asynchronously via a
///   `DelegationEvent`, and `session_query`/`sessions_yield` callers
///   reasonably expect to still be able to look up what a background task
///   actually did after it finishes. This was the actual bug: the
///   pre-fix code GC'd on `result.is_ok()` alone, so a successfully
///   completed *async* delegation's sub-session vanished immediately,
///   while a *timed-out-then-resumed* one (which never satisfies
///   `result.is_ok()` on this call) stayed visible — the inconsistency
///   the issue observed.
fn should_gc_sub_session(is_async_delegation: bool, result_is_ok: bool) -> bool {
    result_is_ok && !is_async_delegation
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
    runtime_cell: Arc<std::sync::OnceLock<crate::agents::runtime::AgentRuntime>>,
    /// In-flight background delegations (sub_session_id → entry). Powers
    /// `/agent_list` (read snapshot with live status) and `/agent_kill`
    /// (abort by session id).
    running: Arc<DashMap<String, RunningEntry>>,
    /// Parent → sub inboxes of running async sub-agents
    /// (sub_session_id → sender). Registered by `spawn_delegate_async`,
    /// removed when the sub-agent finishes; powers the
    /// `send_message(recipient=sub_session_id)` tool via
    /// `AgentMessenger::send_to_sub_agent`.
    mailboxes: Arc<DashMap<String, mpsc::Sender<AgentMail>>>,
    /// Namespace for generated FQIDs (`<ns>/s/<uuidv7>` session ids). Bound
    /// at construction from `[system] namespace`.
    namespace: String,
    /// Maximum delegation depth, counting the main agent as depth 1
    /// (RFC §6, `[delegation] max_depth`). A delegation whose child depth
    /// would exceed this limit is rejected at the tool layer before any
    /// pending task is registered or suspension created.
    max_depth: u32,
    /// Sender for `DelegationEvent`s, set once by the daemon via
    /// `set_event_sender` when wiring the orchestrator's `delegation_rx`.
    event_tx_cell: Arc<OnceLock<mpsc::Sender<DelegationEvent>>>,
    /// Sub → parent final messages recorded for SYNC delegations
    /// (sub_session_id → texts). `send_to_parent` skips the broadcast for
    /// sync sub-agents (no running-table entry — the parent is blocked
    /// inside the tool call) and records the text here instead; the sync
    /// completion branch attaches it to the tool result so the parent gets
    /// the full report immediately instead of via the delayed notice
    /// channel. Removed on read. (2026-08-14, sync notice B)
    sent_messages: Arc<DashMap<String, std::sync::RwLock<Vec<String>>>>,
    /// Shared with `ShellTool`/`ShellPollTool`/`ShellKillTool` — used to kill
    /// any shell processes a sub-agent's session still has running when its
    /// delegation ends (timeout or `agent_kill`). See
    /// `crate::tools::shell::kill_processes_for_session` for why this is
    /// necessary: cancelling the delegation's future does not reach the
    /// detached tasks tracking those processes.
    shell_registry: crate::tools::shell::ShellRegistry,
}

impl DelegationCoordinator {
    pub fn new(
        configs: Arc<super::AgentRegistry>,
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
        sub_ctx: std::sync::Arc<SessionContext>,
        timeout_secs: u64,
        inbox: Option<SubAgentMailbox>,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&'a str>,
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
            let sub_session_id = {
                let session = sub_ctx.session.lock().await;
                session.id.clone()
            };

            tracing::info!(
                agent = %config.name,
                sub_session_id = %sub_session_id,
                parent = %parent_session_id,
                tools = ?config.tools,
                task_len = task.len(),
                "creating sub-agent for delegation"
            );

            // --- worktree creation (moved BEFORE prompt so we can inject the path) ---
            // worktree isolation requires the caller-provided `workspace`
            // (git repo root): the sub-agent's worktree is created inside
            // that repository and merged back into it on completion. Never
            // fall back to the daemon cwd — that is a different repository
            // (the workspace repo) and produces an empty worktree (bug 2026-08-13).
            let worktree_repo = match config.isolation {
                AgentIsolation::Worktree => Some(workspace.ok_or_else(|| {
                    anyhow::anyhow!(
                        "agent '{}' uses worktree isolation: the 'workspace' parameter \
                         is required (git repo root where the sub-agent's worktree \
                         is created and merged back)",
                        config.name
                    )
                })?),
                AgentIsolation::Shared => None,
            };

            let (worktree_path, cleanup_worktree, branch_name, main_branch) = match config.isolation {
                AgentIsolation::Worktree => {
                    let repo =
                        worktree_repo.expect("worktree isolation guarantees workspace above");
                    let worktree_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
                    let branch_name = worktree_branch_name(&config.name, &worktree_id);
                    let worktree_path = self
                        .worktrees_root
                        .join(format!("{}_{}", config.name, worktree_id));

                    // Capture the main branch name BEFORE creating the
                    // worktree. The daemon repo is never checked out (HEAD
                    // stays on the main branch), so `git checkout @{-1}`
                    // after the sub-agent finishes has no reflog entry to
                    // resolve (bug 2026-08-14: pathspec '@{-1}' did not
                    // match — merge silently skipped, sub-agent commits left
                    // dangling). Deterministic fix: checkout the captured
                    // branch name instead. Detached HEAD is a hard error —
                    // there is no branch to merge back into.
                    let main_branch_out = std::process::Command::new("git")
                        .args(["rev-parse", "--abbrev-ref", "HEAD"])
                        .current_dir(repo)
                        .output()
                        .map_err(|e| anyhow::anyhow!("failed to detect main branch name: {}", e))?;
                    if !main_branch_out.status.success() {
                        anyhow::bail!(
                            "failed to detect main branch name: {}",
                            String::from_utf8_lossy(&main_branch_out.stderr)
                        );
                    }
                    let main_branch = String::from_utf8_lossy(&main_branch_out.stdout)
                        .trim()
                        .to_string();
                    if main_branch == "HEAD" {
                        anyhow::bail!(
                            "workspace repo is in detached HEAD state; cannot merge sub-agent branch '{}' back",
                            branch_name
                        );
                    }
                    tracing::debug!(main_branch = %main_branch, "captured main branch for worktree merge-back");

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
                        .current_dir(repo)
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
                    (worktree_path, Some(worktree_id), Some(branch_name), Some(main_branch))
                }
                AgentIsolation::Shared => (PathBuf::new(), None, None, None),
            };

            let workspace_dir = if !worktree_path.as_os_str().is_empty() {
                worktree_path.to_string_lossy().to_string()
            } else if let Some(ws) = workspace {
                ws.to_string()
            } else {
                String::new()
            };

            let workspace_section = if workspace_dir.is_empty() {
                String::new()
            } else {
                format!("\n\nWorking directory: {}", workspace_dir)
            };

            // Identity prompt: the sub-agent must know its own session id —
            // it is the addressing key for `agent_list` (self-identification),
            // `send_message` (sub → parent), and recovery. Injecting it also
            // prevents the model from fabricating an id (f362).
            let identity = if config.system_prompt.is_empty() {
                format!(
                    "You are a specialized agent named '{}'. Your session id is '{}'.{}",
                    config.name, sub_session_id, workspace_section
                )
            } else {
                format!(
                    "{}\n\nYour session id is '{}'.{}",
                    config.system_prompt, sub_session_id, workspace_section
                )
            };

            // RFC §三.A line 404-419: sub-sessions flow through the unified
            // path — SessionManager builds a SessionContext (held only by this
            // function), session-level overrides carry the run_mode / model /
            // identity prompt, and `process_turn` does the rest. The
            // SessionContext is created by the caller (delegate /
            // spawn_delegate_async) so the sub-session id is known before the
            // delegation starts — it is the agent's identity and addressing key.

            // Capture whether this is an async delegation BEFORE `inbox` is
            // moved below — the completion branch uses it to decide who owns
            // checkpoint cleanup (async spawn task bodies delete the
            // checkpoint after broadcasting the terminal event; sync
            // delegations clean up here).
            let is_async_delegation = inbox.is_some();
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
                // (parent → sub messages) via `inbox`. The sub-session id is
                // the identity for sub→parent messages for BOTH sync and async
                // sub-agents (a sync sub-agent can message its parent, it
                // just cannot receive messages — no inbox).
                if let Some(mailbox) = inbox {
                    session.sub_agent_inbox = Some(Arc::new(mailbox));
                }
                session.turn_tool_allowlist = allowed_tools;
                // Time-awareness: the sub-agent sees its wall-clock budget as
                // a per-LLM-request countdown reminder. Derived from the same
                // `timeout_secs` that drives the kill timer below, so the
                // injected countdown can never disagree with the actual kill.
                session.delegation_deadline =
                    Some(crate::agents::session::DelegationDeadline::from_now(timeout_secs));
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

            // Durable delegation checkpoint: persists the sub-session identity
            // (keyed by sub_session_id) so the daemon can resume (not mark
            // Failed) on restart.
            let checkpoint = crate::storage::DelegationCheckpoint {
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
                tracing::warn!(sub_session_id = %sub_session_id, err = %e, "persist delegation checkpoint failed");
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
            let synthetic = crate::api::message::ChannelInboundMessage {
                id: format!("delegation:{}", sub_session_id),
                sender: crate::api::message::MessageSender::new(format!("agent:{}", config.name)),
                receiver: crate::api::message::MessageReceiver::new(String::new()),
                content: crate::api::message::ChannelMessageContent::text(task.to_string()),
                timestamp: chrono::Utc::now().timestamp() as u64,
                interruption_scope_id: None,
                silenced_override: None,
                // RFC channel-role-split §1.1: run_mode now travels on the
                // message and process_turn reads ONLY the message — the
                // `session_override.run_mode = Background` write below is no
                // longer consulted. Carry Background here explicitly so
                // sub-agents keep their pre-RFC autonomous prompt rules
                // (SECTION_AUTONOMOUS_RULES) and `ask_user` reports the
                // headless error instead of the channel-missing one.
                run_mode: crate::config::agent::RunMode::Background,
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
                    // semantics as aborting a spawned task. That cancellation
                    // does not reach any shell processes this sub-agent
                    // started (spawn_tracked hands them off to a detached
                    // reaper immediately, by design — see the shell module
                    // docs) — kill them explicitly so a timed-out delegation
                    // actually stops, not just its LLM loop.
                    tracing::warn!(
                        agent = %config.name,
                        timeout_secs,
                        "sub-agent timed out, cancelling turn"
                    );
                    crate::tools::shell::kill_processes_for_session(
                        &self.shell_registry,
                        &sub_session_id,
                    )
                    .await;
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
                let repo =
                    worktree_repo.expect("worktree isolation guarantees workspace for merge");
                // `branch_name` is Some ⟺ Worktree isolation, where
                // `main_branch` was captured before the worktree was created.
                // Borrowed (not moved) — `branch_name` is used again below by
                // the worktree cleanup.
                let Some(main_branch) = main_branch.as_deref() else {
                    anyhow::bail!("worktree delegation missing captured main branch");
                };
                let diff = std::process::Command::new("git")
                    .args(["log", "--oneline", "HEAD..", branch_name])
                    .current_dir(repo)
                    .output();

                let has_commits = match diff {
                    Ok(d) => !d.stdout.is_empty(),
                    Err(_) => false,
                };

                if has_commits {
                    // Switch back to the main branch — captured deterministically
                    // before the worktree was created. `@{-1}` fails here: the
                    // daemon repo is never checked out, so reflog has no previous
                    // branch to resolve (bug 2026-08-14: pathspec '@{-1}' did
                    // not match, merge silently skipped).
                    let checkout = std::process::Command::new("git")
                        .args(["checkout", main_branch])
                        .current_dir(repo)
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
                                .current_dir(repo)
                                .output();

                            match merge {
                                Ok(m) if !m.status.success() => {
                                    // Capture conflicted files for diagnostics.
                                    let conflicts = std::process::Command::new("git")
                                        .args(["diff", "--name-only", "--diff-filter=U"])
                                        .current_dir(repo)
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
                                        .current_dir(repo)
                                        .output();
                                    let detail = if conflict_files.is_empty() {
                                        String::new()
                                    } else {
                                        format!("\nConflicted files:\n{}", conflict_files)
                                    };
                                    // Terminal state reached (merge conflict
                                    // aborts the delegation): sync delegations
                                    // must tombstone their checkpoint here,
                                    // since the completion branch below is
                                    // skipped by this early return. The
                                    // sub-session history is mid-turn, so a
                                    // deleted checkpoint would let a restart
                                    // re-run the failed task.
                                    if !is_async_delegation {
                                        self.persist_terminal_checkpoint(&sub_session_id, DelegationStatus::Failed);
                                    }
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
                            // Fail-fast: without the main branch checked out
                            // the merge cannot run and the sub-agent's commits
                            // would be orphaned (dangling). Abort the delegation
                            // with a clear error so the parent can recover the
                            // branch manually.
                            tracing::error!(
                                branch = %branch_name,
                                main_branch = %main_branch,
                                stderr = %String::from_utf8_lossy(&co.stderr),
                                "failed to checkout main branch for merge"
                            );
                            if !is_async_delegation {
                                self.persist_terminal_checkpoint(&sub_session_id, DelegationStatus::Failed);
                            }
                            return Err(anyhow::anyhow!(
                                "sub-agent '{}' completed but checkout of main branch '{}' failed: {}. \
                                 Sub-agent branch '{}' has commits that were NOT merged; \
                                 recover manually (worktree preserved at {})",
                                config.name,
                                main_branch,
                                String::from_utf8_lossy(&co.stderr),
                                branch_name,
                                worktree_path.display()
                            ));
                        }
                    }
                } else {
                    tracing::debug!(branch = %branch_name, "no new commits, skipping merge");
                }
            }

            // Cleanup worktree + branch on any exit path.
            // (Merge conflicts already returned early above, preserving the worktree.)
            if cleanup_worktree.is_some() {
                let repo =
                    worktree_repo.expect("worktree isolation guarantees workspace for cleanup");
                let _ = std::process::Command::new("git")
                    .args([
                        "worktree",
                        "remove",
                        "--force",
                        &worktree_path.to_string_lossy(),
                    ])
                    .current_dir(repo)
                    .output();
                if let Some(ref bn) = branch_name {
                    let _ = std::process::Command::new("git")
                        .args(["branch", "-D", bn])
                        .current_dir(repo)
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
            // B (2026-08-14): sync delegations — the sub-agent's final
            // messages were recorded in `sent_messages` (see
            // `send_to_parent`: sync skips the broadcast and records the text
            // instead). Attach them to the tool result so the parent receives
            // the full report immediately, not via the delayed notice channel.
            // Removed on read — each sync delegation consumes its own entries.
            let sent_messages: Vec<String> = self
                .sent_messages
                .remove(&sub_session_id)
                .map(|(_, v)| v.into_inner().unwrap_or_default())
                .unwrap_or_default();
            let result = match result {
                Ok(text) => {
                    let mut parts = vec![text];
                    if !sent_messages.is_empty() {
                        parts.push(format!(
                            "[子代理消息]：\n{}",
                            sent_messages.join("\n---\n")
                        ));
                    }
                    if !undelivered.is_empty() {
                        tracing::warn!(
                            sub_session_id = %sub_session_id,
                            count = undelivered.len(),
                            "sub-agent finished with unread parent messages; attaching to result"
                        );
                        parts.push(format!(
                            "[主 agent 有 {} 条消息在任务结束后到达，未处理]：\n{}",
                            undelivered.len(),
                            undelivered.join("\n---\n")
                        ));
                    }
                    Ok(parts.join("\n\n"))
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

            // GC: delete the sub-session for sync delegations only (issue
            // #106). See `should_gc_sub_session` for the rationale — this
            // was previously ungated, so a *successful async* delegation's
            // sub-session also got deleted immediately on completion.
            if should_gc_sub_session(is_async_delegation, result.is_ok()) {
                if let Err(e) = self.session_manager.backend().delete_session(&sub_session_id) {
                    tracing::debug!(sub_session = %sub_session_id, err = %e, "failed to GC sub-session");
                }
            }

            // Sync delegations (no inbox): settle the durable checkpoint here.
            // The task has reached a terminal state (Ok or Err — both are
            // terminal). Completed (Ok) deletes it: the result was returned to
            // the parent's tool call and the sub-session is GC'd (or its
            // history is complete), so nothing resumes on restart. Non-OK
            // terminal states (timed out / failed) keep it as a tombstone with
            // the terminal status so `run_startup` skips re-running the task.
            // The async path keeps its checkpoint until the spawn task body has
            // broadcast the terminal event (`spawn_delegate_async` settles it
            // there); deleting a missing key is idempotent, so the redundant
            // async re-delete is harmless. A crash mid-run still leaves the
            // checkpoint intact for `scan_unfinished_subagents`.
            if !is_async_delegation {
                if result.is_ok() {
                    if let Err(e) = self
                        .session_manager
                        .backend()
                        .delete_delegation_checkpoint(&sub_session_id)
                    {
                        tracing::warn!(sub_session_id = %sub_session_id, err = %e, "delete delegation checkpoint failed");
                    }
                } else {
                    let terminal = if result
                        .as_ref()
                        .err()
                        .and_then(|e| e.downcast_ref::<DelegationTimeout>())
                        .is_some()
                    {
                        DelegationStatus::TimedOut
                    } else {
                        DelegationStatus::Failed
                    };
                    self.persist_terminal_checkpoint(&sub_session_id, terminal);
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
        timeout_secs: u64,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
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

        // Recursion depth guard (RFC §6): reject synchronously at the tool
        // layer BEFORE registering the pending task — a rejected delegation
        // must not leave a hanging pending task / suspension behind. The
        // same check inside `delegate_with_parent` (async task body) is a
        // redundant backstop.
        self.check_depth(parent_session_id)?;

        // Create the sub-session FIRST so the sub-agent's identity (its
        // session FQID) exists before anything is registered — it is the
        // addressing key for agent_list / agent_kill / send_message /
        // pending-task matching / checkpoints. The returned id is what the
        // parent tool call (`agent_delegate` async mode) receives.
        let (sub_ctx, sub_session_id) = self
            .session_manager
            .create_sub_session_context(parent_session_id, &config.name)?;

        // 方案 C (docs/turn-suspension-rfc.md): register the task against the
        // parent's registered SessionContext so the running turn knows to
        // suspend when the LLM ends it. Only the orchestrator's active
        // top-level sessions are registered; sub-agent sessions and
        // switched-away sessions return None and simply don't suspend.
        if let Some(parent_ctx) = self
            .session_manager
            .registered_context_by_session_id(parent_session_id)
        {
            parent_ctx.add_pending_task(sub_session_id.clone());
        }

        tracing::info!(
            agent = %config.name,
            sub_session_id = %sub_session_id,
            task_len = task.len(),
            timeout_secs,
            "spawning sub-agent in background"
        );

        let sub_delegator = self.clone();
        let task_owned = task.to_string();
        let parent_session_id_owned = parent_session_id.to_string();
        let workspace_owned = workspace.map(|s| s.to_string());
        let event_tx = self.event_sender();
        let sub_session_id_clone = sub_session_id.clone();
        let agent_name_owned = agent_name.to_string();
        let running = Arc::clone(&self.running);
        let running_sub_session_id = sub_session_id.clone();
        let allowed_tools_entry = allowed_tools.clone();

        // RFC agent-messaging §3: register the sub-agent's inbox so the
        // parent's `send_message(recipient=sub_session_id)` can reach it.
        // The sender is dropped when the map entry is removed at completion.
        let (mail_tx, mail_rx) = mpsc::channel(SUB_AGENT_INBOX_CAPACITY);
        let mailbox = SubAgentMailbox {
            tx: mail_tx.clone(),
            rx: tokio::sync::Mutex::new(mail_rx),
        };
        self.mailboxes.insert(sub_session_id.clone(), mail_tx);
        let mailboxes = Arc::clone(&self.mailboxes);

        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            // ── Two-layer spawn (panic safety net) ──────────────────────
            //
            // The sub-agent body runs in an *inner* `tokio::spawn` so a panic
            // is surfaced as a `JoinError` (is_panic) instead of propagating
            // uncaught through the outer task. Without this, a panic skips
            // the running-table cleanup, event dispatch, and checkpoint
            // settlement below — the parent agent hangs on its terminal event
            // forever.
            //
            // The inner handle returns `anyhow::Result<String>`; the outer
            // converts `JoinError` outcomes into the same type so the existing
            // collection logic handles them uniformly.
            let sub_delegator_inner = sub_delegator.clone();
            let parent_session_id_inner = parent_session_id_owned.clone();
            let inner_handle: tokio::task::JoinHandle<anyhow::Result<String>> =
                tokio::spawn(async move {
                    sub_delegator_inner
                        .delegate_with_parent(
                            &agent_name_owned,
                            &task_owned,
                            &parent_session_id_inner,
                            sub_ctx,
                            timeout_secs,
                            Some(mailbox),
                            allowed_tools,
                            workspace_owned.as_deref(),
                        )
                        .await
                });

            let result = match inner_handle.await {
                Ok(r) => r,
                Err(je) if je.is_panic() => {
                    let payload = je.into_panic();
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic payload".to_string()
                    };
                    tracing::error!(
                        sub_session_id = %sub_session_id_clone,
                        panic = %msg,
                        "sub-agent task panicked"
                    );
                    Err(anyhow::anyhow!("sub-agent panicked: {}", msg))
                }
                Err(je) if je.is_cancelled() => {
                    // Abort path (cancel / checkpoint_and_cancel_all) already
                    // persisted a checkpoint tombstone. Clean up the running
                    // table and mailbox, then return without emitting an event
                    // — the caller (cancel / shutdown) owns the terminal event.
                    tracing::debug!(
                        sub_session_id = %sub_session_id_clone,
                        "sub-agent task cancelled (abort); cleaning up without event"
                    );
                    running.remove(&running_sub_session_id);
                    mailboxes.remove(&running_sub_session_id);
                    return;
                }
                Err(je) => {
                    tracing::error!(
                        sub_session_id = %sub_session_id_clone,
                        err = %je,
                        "sub-agent task join error"
                    );
                    Err(anyhow::anyhow!("sub-agent task join error: {}", je))
                }
            };

            let duration_secs = start_time.elapsed().as_secs();

            // Classify the outcome: a wall-clock timeout is distinct from a
            // generic failure — the parent must know the task was abandoned
            // mid-flight, not finished-with-error.
            let timed_out_secs = result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<DelegationTimeout>())
                .map(|t| t.secs);

            let parent_session_id_final = parent_session_id_owned.clone();
            // Count of sub → parent messages delivered while running. The
            // parent session has already received them as `DelegationEvent::
            // Message`, so a non-zero count lets `wake` skip the summary in
            // the completion note (de-duplication, ④).
            let sent_message_count = running
                .get(&running_sub_session_id)
                .map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);
            if let Some(tx) = event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        tracing::info!(sub_session_id = %sub_session_id_clone, duration_secs, sent_message_count, "sub-agent completed successfully");
                        if let Err(send_err) = tx
                            .send(DelegationEvent::Completed {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id_final,
                                summary: summary.clone(),
                                duration_secs,
                                sent_message_count,
                            })
                            .await
                        {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(_), Some(secs)) => {
                        tracing::warn!(sub_session_id = %sub_session_id_clone, timeout_secs = secs, duration_secs, "sub-agent timed out");
                        if let Err(send_err) = tx
                            .send(DelegationEvent::TimedOut {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id_final,
                                timeout_secs: secs,
                                duration_secs,
                            })
                            .await
                        {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(e), None) => {
                        tracing::warn!(sub_session_id = %sub_session_id_clone, duration_secs, err = %e, "sub-agent failed");
                        if let Err(send_err) = tx
                            .send(DelegationEvent::Failed {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id_final,
                                error: e.to_string(),
                            })
                            .await
                        {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
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
            if let Some(entry) = running.get(&running_sub_session_id) {
                if let Ok(mut status) = entry.status.write() {
                    *status = terminal;
                }
            }
            running.remove(&running_sub_session_id);
            // Drop the mailbox sender: the sub-agent is gone, so
            // `send_message(recipient=sub_session_id)` will now report the
            // session id as unknown instead of queueing into a dead channel.
            mailboxes.remove(&running_sub_session_id);
            // Settle the durable checkpoint — the task reached a terminal
            // state. Completed deletes it (history is complete; nothing
            // resumes on restart). TimedOut / Failed keep it as a tombstone
            // with the terminal status so `run_startup` skips re-running the
            // task whose history is mid-turn.
            if terminal == DelegationStatus::Completed {
                if let Err(e) = sub_delegator
                    .session_manager
                    .backend()
                    .delete_delegation_checkpoint(&running_sub_session_id)
                {
                    tracing::warn!(sub_session_id = %running_sub_session_id, err = %e, "delete delegation checkpoint failed");
                }
            } else {
                sub_delegator.persist_terminal_checkpoint(&running_sub_session_id, terminal);
            }
        });

        self.running.insert(
            sub_session_id.clone(),
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: agent_name.to_string(),
                parent_session_id: parent_session_id.to_string(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(timeout_secs),
                started_at: chrono::Utc::now(),
                allowed_tools: allowed_tools_entry,
            },
        );
        Ok(sub_session_id)
    }

    /// Layer-3 resumability: revive a timed-out delegation with a fresh budget.
    ///
    /// The kill timer ends the *task*, not the *work* — the sub-session
    /// history (and its incomplete-turn state, re-detected on load) survives
    /// on disk, so `run_recovery` continues from the breakpoint. This method:
    /// 1. validates the durable checkpoint is a `timed_out` tombstone (the
    ///    only resumable terminal — Completed checkpoints are deleted, and
    ///    Failed/Cancelled carry no continuable-turn guarantee),
    /// 2. re-arms the parent's suspension `pending` (idempotent) BEFORE
    ///    spawning, so the eventual terminal event records cleanly instead
    ///    of hitting the `Duplicate` drop path (TimedOut was already
    ///    collected once) — and, if the parent turn is still running, it
    ///    suspends awaiting the result like a fresh async delegation,
    /// 3. rewrites the checkpoint (`running`, `started_at = now`, new
    ///    budget) so a crash during the resumed run recovers with the
    ///    *resumed* budget, then drives `recover_async` (the same path as
    ///    daemon-restart recovery).
    pub fn resume_timed_out(
        &self,
        sub_session_id: &str,
        extra_secs: Option<u64>,
    ) -> anyhow::Result<String> {
        use anyhow::Context as _;
        if self.running.contains_key(sub_session_id) {
            anyhow::bail!("sub-agent '{}' is already running", sub_session_id);
        }
        let backend = self.session_manager.backend();
        let cp = backend
            .load_delegation_checkpoint(sub_session_id)
            .with_context(|| format!("no delegation checkpoint for '{}'", sub_session_id))?;
        if cp.status != "timed_out" {
            anyhow::bail!(
                "sub-agent '{}' is not resumable (checkpoint status: '{}'; only timed_out delegations can be resumed)",
                sub_session_id,
                cp.status
            );
        }
        // Issue #111: resume is only reachable after the original budget
        // already ran out, so defaulting to that same budget guarantees a
        // second timeout unless the first one was a fluke. Default to
        // double the original (floor 600s) instead — still overridable via
        // `extra_secs`, but no longer self-defeating on its core use case.
        let requested = extra_secs.unwrap_or_else(|| cp.timeout_secs.saturating_mul(2).max(600));
        let budget = requested.clamp(1, SUB_AGENT_TIMEOUT_MAX_SECS);

        // Re-arm the parent side. Registered (active) context first; the
        // fallback load covers a parent that switched away or whose turn
        // already ended — `add_pending_task` re-creates the suspension in
        // that case, keeping the parent turn-suspended awaiting the result.
        let parent_ctx = self
            .session_manager
            .registered_context_by_session_id(&cp.parent_session_id)
            .or_else(|| {
                self.session_manager
                    .load_context_by_session_id(&cp.parent_session_id)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "parent session '{}' not found for sub-agent '{}'",
                    cp.parent_session_id,
                    sub_session_id
                )
            })?;
        parent_ctx.add_pending_task(sub_session_id.to_string());

        // Sub-sessions are never registered — always a fresh load (same as
        // daemon-restart recovery). Loading re-detects the incomplete turn
        // from the history tail, which `run_recovery` continues from.
        let sub_ctx = self
            .session_manager
            .load_context_by_session_id(sub_session_id)
            .with_context(|| format!("sub-agent session '{}' not found", sub_session_id))?;

        let resumed = crate::storage::DelegationCheckpoint {
            parent_session_id: cp.parent_session_id.clone(),
            sub_session_id: sub_session_id.to_string(),
            agent_name: cp.agent_name.clone(),
            status: "running".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: budget,
            allowed_tools: cp.allowed_tools.clone(),
            last_checkpoint: None,
        };
        if let Err(e) = backend.save_delegation_checkpoint(&resumed) {
            tracing::warn!(sub_session_id, err = %e, "persist resumed delegation checkpoint failed");
        }
        tracing::info!(
            sub_session_id,
            budget_secs = budget,
            "resuming timed-out delegation with fresh budget"
        );

        self.recover_async(
            sub_session_id.to_string(),
            cp.agent_name.clone(),
            cp.parent_session_id.clone(),
            sub_ctx,
            budget,
            cp.allowed_tools.clone(),
        );
        Ok(sub_session_id.to_string())
    }

    pub fn recover_async(
        &self,
        sub_session_id: String,
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
        self.mailboxes.insert(sub_session_id.clone(), mail_tx);
        let mailboxes = Arc::clone(&self.mailboxes);

        let running = Arc::clone(&self.running);
        let event_tx = self.event_sender();
        let running_sub_session_id = sub_session_id.clone();
        let sub_session_id_clone = sub_session_id.clone();
        let parent_session_id_final = parent_session_id.clone();
        let backend = Arc::clone(self.session_manager.backend());

        let runtime = match self.runtime() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("recover_async failed to get runtime: {}", e);
                return;
            }
        };

        let agent_name_clone = agent_name.clone();
        let allowed_tools_entry = allowed_tools.clone();
        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            {
                let mut session = sub_ctx.session.lock().await;
                session.sub_agent_inbox = Some(Arc::new(mailbox));
                session.turn_tool_allowlist = allowed_tools;
                // Time-awareness on recovery: same budget as the kill timer
                // below (the caller already discounted time spent before the
                // restart when computing `timeout_secs`), so the countdown
                // the sub-agent sees matches the actual remaining budget.
                session.delegation_deadline =
                    Some(crate::agents::session::DelegationDeadline::from_now(timeout_secs));
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
            let sent_message_count = running.get(&running_sub_session_id).map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);

            if let Some(tx) = event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        if let Err(send_err) = tx.send(DelegationEvent::Completed { sub_session_id: sub_session_id_clone.clone(), parent_session_id: parent_session_id_final, summary: summary.clone(), duration_secs, sent_message_count }).await {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(_), Some(secs)) => {
                        if let Err(send_err) = tx.send(DelegationEvent::TimedOut { sub_session_id: sub_session_id_clone.clone(), parent_session_id: parent_session_id_final, timeout_secs: secs, duration_secs }).await {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(e), None) => {
                        if let Err(send_err) = tx.send(DelegationEvent::Failed { sub_session_id: sub_session_id_clone.clone(), parent_session_id: parent_session_id_final, error: e.to_string() }).await {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                }
            }

            let terminal = if timed_out_secs.is_some() { DelegationStatus::TimedOut } else if result.is_ok() { DelegationStatus::Completed } else { DelegationStatus::Failed };
            if let Some(entry) = running.get(&running_sub_session_id) {
                if let Ok(mut status) = entry.status.write() { *status = terminal; }
            }
            running.remove(&running_sub_session_id);
            mailboxes.remove(&running_sub_session_id);
            // Settle the durable checkpoint — the recovered task reached a
            // terminal state. Completed deletes it (its recovery finished and
            // the history is now complete). TimedOut / Failed keep it as a
            // tombstone so a SECOND restart does not re-run the recovery.
            if terminal == DelegationStatus::Completed {
                if let Err(e) = backend.delete_delegation_checkpoint(&running_sub_session_id) {
                    tracing::warn!(sub_session_id = %running_sub_session_id, err = %e, "delete delegation checkpoint failed");
                }
            } else if let Err(e) = backend.update_delegation_checkpoint_status(
                &running_sub_session_id,
                terminal.as_str(),
            ) {
                tracing::warn!(
                    sub_session_id = %running_sub_session_id,
                    status = %terminal.as_str(),
                    err = %e,
                    "update delegation checkpoint status (tombstone) failed"
                );
            }
        });

        self.running.insert(
            sub_session_id,
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name,
                parent_session_id,
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(timeout_secs),
                started_at: chrono::Utc::now(),
                allowed_tools: allowed_tools_entry,
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
        parent_ctx: &crate::api::tool::ToolContext,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
    ) -> anyhow::Result<String> {
        let config = self.find_agent(agent_name);
        let config_timeout = config.as_ref().and_then(|a| a.config.timeout);
        let timeout_secs = resolve_timeout(timeout, config_timeout);
        // Create the sub-session context up front — the sub-session id is
        // the agent's identity, created before the delegation starts (same
        // unified path as the async `spawn_delegate_async`).
        let (sub_ctx, _sub_session_id) = self
            .session_manager
            .create_sub_session_context(&parent_ctx.session_id, agent_name)?;
        self.delegate_with_parent(
            agent_name,
            task,
            &parent_ctx.session_id,
            sub_ctx,
            timeout_secs,
            None,
            allowed_tools,
            workspace,
        )
        .await
    }

    fn delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        parent_ctx: &crate::api::tool::ToolContext,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
    ) -> anyhow::Result<String> {
        let config = self.find_agent(agent_name);
        let config_timeout = config.as_ref().and_then(|a| a.config.timeout);
        let timeout_secs = resolve_timeout(timeout, config_timeout);
        self.spawn_delegate_async(
            agent_name,
            task,
            &parent_ctx.session_id,
            timeout_secs,
            allowed_tools,
            workspace,
        )
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
    fn send_to_sub_agent(&self, sub_session_id: &str, mail: AgentMail) -> Result<(), String> {
        match self.mailboxes.get(sub_session_id) {
            Some(tx) => tx
                .try_send(mail)
                .map_err(|e| format!("消息投递失败（子代理收件箱已满或已关闭）：{}", e)),
            None => Err(format!(
                "sub_session_id '{}' 不存在或子代理已结束（仅 async 子代理可接收消息）",
                sub_session_id
            )),
        }
    }

    async fn send_to_parent(&self, event: AgentMessage) -> bool {
        // Sync delegations have no running-table entry (the parent is blocked
        // inside the tool call awaiting the result), so there is no notice
        // consumer on the event channel: the message must ride back in the
        // tool result instead of a delayed notice. Record the final text for
        // the sync completion branch and skip the broadcast — the
        // `[子代理消息]` injection path is async-only (2026-08-14, B).
        if self.running.get(&event.sub_session_id).is_none() {
            if event.kind != MessageKind::Progress {
                if let Some(msgs) = self.sent_messages.get(&event.sub_session_id) {
                    msgs.write().unwrap().push(event.text.clone());
                } else {
                    self.sent_messages.insert(
                        event.sub_session_id.clone(),
                        std::sync::RwLock::new(vec![event.text.clone()]),
                    );
                }
            }
            return true;
        }
        match self.event_sender() {
            Some(tx) => {
                let sub_session_id = event.sub_session_id.clone();
                let delivered = tx.send(DelegationEvent::Message(event)).await.is_ok();
                if delivered {
                    // Bump the per-task message counter so the completion
                    // wrapper can de-duplicate the summary (④).
                    if let Some(entry) = self.running.get(&sub_session_id) {
                        entry
                            .messages_sent
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                delivered
            }
            None => {
                tracing::warn!(
                    sub_session_id = %event.sub_session_id,
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
    use crate::agents::AgentMessenger;
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
            Default::default(),
        );
        (dc, manager)
    }

    #[tokio::test]
    async fn sync_send_to_parent_records_text_without_broadcast() {
        let (dc, manager) = coordinator(3);
        let parent = manager.get_or_create_context("mock:default:u1");
        let (tx, mut rx) = mpsc::channel(8);
        dc.set_event_sender(tx);

        // No running-table entry ⇒ sync delegation: final messages are
        // recorded for the tool-result merge (B, 2026-08-14), NOT broadcast
        // as DelegationEvent::Message — the parent is blocked in the tool
        // call and receives them via the returned result instead.
        assert!(dc
            .send_to_parent(AgentMessage {
                msg_id: "m1".to_string(),
                sender_name: "coder".to_string(),
                sub_session_id: "test/s/sync-sub".to_string(),
                parent_session_id: parent.session_id.clone(),
                text: "detailed final report".to_string(),
                kind: MessageKind::Final,
            })
            .await);
        assert!(
            rx.try_recv().is_err(),
            "sync delegation message must not be broadcast as DelegationEvent::Message"
        );
        let stored = dc
            .sent_messages
            .get("test/s/sync-sub")
            .map(|v| v.read().unwrap().clone())
            .unwrap_or_default();
        assert_eq!(stored, vec!["detailed final report"]);

        // Progress messages are never recorded (RFC §3.4: progress is
        // dropped for the parent context).
        assert!(dc
            .send_to_parent(AgentMessage {
                msg_id: "m2".to_string(),
                sender_name: "coder".to_string(),
                sub_session_id: "test/s/sync-sub".to_string(),
                parent_session_id: parent.session_id.clone(),
                text: "progress 50%".to_string(),
                kind: MessageKind::Progress,
            })
            .await);
        let stored = dc
            .sent_messages
            .get("test/s/sync-sub")
            .map(|v| v.read().unwrap().clone())
            .unwrap_or_default();
        assert_eq!(stored, vec!["detailed final report"], "progress must be skipped");
    }

    #[tokio::test]
    async fn async_send_to_parent_broadcasts_and_counts() {
        let (dc, manager) = coordinator(3);
        let parent = manager.get_or_create_context("mock:default:u1");
        let (tx, mut rx) = mpsc::channel(8);
        dc.set_event_sender(tx);

        // Fake running entry ⇒ async path: broadcast + count, and nothing
        // recorded in the sync buffer.
        let sub_session_id = "test/s/async-sub".to_string();
        dc.running.insert(
            sub_session_id.clone(),
            RunningEntry {
                handle: tokio::spawn(async {}),
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                parent_session_id: parent.session_id.clone(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(60),
                started_at: chrono::Utc::now(),
                allowed_tools: None,
            },
        );
        assert!(dc
            .send_to_parent(AgentMessage {
                msg_id: "m1".to_string(),
                sender_name: "coder".to_string(),
                sub_session_id: sub_session_id.clone(),
                parent_session_id: parent.session_id.clone(),
                text: "working".to_string(),
                kind: MessageKind::Final,
            })
            .await);
        match rx.try_recv().expect("async message must be broadcast") {
            DelegationEvent::Message(m) => assert_eq!(m.text, "working"),
            other => panic!("expected Message event, got {:?}", other),
        }
        let count = dc
            .running
            .get(&sub_session_id)
            .map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        assert_eq!(count, 1);
        assert!(
            dc.sent_messages.get(&sub_session_id).is_none(),
            "async messages must not touch the sync buffer"
        );
    }

    #[test]
    fn resolve_timeout_priority_fallback_and_clamp() {
        // default: system fallback 1200s
        assert_eq!(resolve_timeout(None, None), 1200);
        // tool value wins over config regardless of ordering
        assert_eq!(resolve_timeout(Some(100), Some(50)), 100);
        assert_eq!(resolve_timeout(Some(100), Some(200)), 100);
        // config used when tool doesn't specify
        assert_eq!(resolve_timeout(None, Some(300)), 300);
        // global hard ceiling 1800s — no per-agent override anymore, the
        // tool-call value is authoritative up to this ceiling
        assert_eq!(resolve_timeout(Some(5000), None), 1800);
        assert_eq!(resolve_timeout(Some(5000), Some(9000)), 1800);
        // 0 passes through (no lower clamp)
        assert_eq!(resolve_timeout(Some(0), None), 0);
    }

    /// issue #106: only a *successful sync* delegation is GC'd. In
    /// particular, a successful *async* delegation must NOT be GC'd — that
    /// was the actual bug (the pre-fix condition was `result.is_ok()`
    /// alone, with no `is_async_delegation` check at all).
    #[test]
    fn should_gc_sub_session_only_for_successful_sync() {
        assert!(should_gc_sub_session(false, true), "sync + success -> GC");
        assert!(
            !should_gc_sub_session(true, true),
            "async + success must NOT be GC'd (issue #106)"
        );
        assert!(
            !should_gc_sub_session(false, false),
            "sync + failure/timeout -> kept as a resumable tombstone"
        );
        assert!(
            !should_gc_sub_session(true, false),
            "async + failure/timeout -> kept"
        );
    }

    #[test]
    fn unknown_session_depth_falls_back_to_one() {
        let (dc, _m) = coordinator(3);
        assert_eq!(dc.session_depth("no-such-session"), 1);
        assert!(dc.check_depth("no-such-session").is_ok());
    }

    // ── resume_timed_out (timeout layer 3) ─────────────────────────────────

    /// Coordinator over a real JsonFileBackend so delegation checkpoints
    /// round-trip (the in-memory backend no-ops them). Runtime is NOT
    /// installed: `recover_async` logs an error and returns without spawning
    /// — everything resume validates/mutates before that point is observable.
    fn coordinator_with_backend(
    ) -> (DelegationCoordinator, Arc<SessionManager>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn crate::storage::SessionBackend> = Arc::new(
            crate::storage::JsonFileBackend::open(dir.path()).unwrap(),
        );
        let manager = Arc::new(SessionManager::new(backend));
        let registry = Arc::new(AgentRegistry::from_vec(vec![coder_config()]));
        let dc = DelegationCoordinator::new(
            registry,
            Arc::clone(&manager),
            PathBuf::new(),
            "test",
            3,
            Default::default(),
        );
        (dc, manager, dir)
    }

    fn checkpoint_for(
        sub_session_id: &str,
        parent_session_id: &str,
        status: &str,
        timeout_secs: u64,
    ) -> crate::storage::DelegationCheckpoint {
        crate::storage::DelegationCheckpoint {
            parent_session_id: parent_session_id.to_string(),
            sub_session_id: sub_session_id.to_string(),
            agent_name: "coder".to_string(),
            status: status.to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs,
            allowed_tools: Some(vec!["shell".to_string()]),
            last_checkpoint: None,
        }
    }

    #[tokio::test]
    async fn resume_timed_out_rejects_missing_and_non_timeout_checkpoints() {
        let (dc, manager, _dir) = coordinator_with_backend();

        // No checkpoint at all → error.
        let err = dc.resume_timed_out("test/s/ghost", None).unwrap_err();
        assert!(format!("{:#}", err).contains("no delegation checkpoint"));

        // Real parent + sub sessions, but the tombstone says "failed" →
        // only timed_out is resumable.
        let parent = manager.get_or_create_context("mock:default:u1");
        let sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
        manager
            .backend()
            .save_delegation_checkpoint(&checkpoint_for(
                &sub.id,
                &parent.session_id,
                "failed",
                600,
            ))
            .unwrap();
        let err = dc.resume_timed_out(&sub.id, None).unwrap_err();
        assert!(format!("{:#}", err).contains("not resumable"));
        assert!(format!("{:#}", err).contains("failed"));
    }

    /// issue #134 (P2): `timed_out_checkpoints` backs agent_resume's
    /// not-found listing — only `timed_out` status checkpoints qualify, not
    /// `failed`/`cancelled`/`running` ones.
    #[tokio::test]
    async fn timed_out_checkpoints_filters_by_status() {
        let (dc, manager, _dir) = coordinator_with_backend();
        let parent = manager.get_or_create_context("mock:default:u1");
        let backend = manager.backend();

        let timed_out_sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
        backend
            .save_delegation_checkpoint(&checkpoint_for(
                &timed_out_sub.id,
                &parent.session_id,
                "timed_out",
                600,
            ))
            .unwrap();

        let failed_sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
        backend
            .save_delegation_checkpoint(&checkpoint_for(
                &failed_sub.id,
                &parent.session_id,
                "failed",
                600,
            ))
            .unwrap();

        let timed_out = dc.timed_out_checkpoints();
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0].sub_session_id, timed_out_sub.id);
    }

    #[tokio::test]
    async fn resume_timed_out_rewrites_checkpoint_and_rearms_parent() {
        let (dc, manager, _dir) = coordinator_with_backend();
        let parent = manager.get_or_create_context("mock:default:u1");
        let sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
        let backend = manager.backend();
        backend
            .save_delegation_checkpoint(&checkpoint_for(
                &sub.id,
                &parent.session_id,
                "timed_out",
                600,
            ))
            .unwrap();

        // Parent turn already ended (no suspension) — resume must re-arm it.
        assert!(!parent.has_pending_async_work());

        let resumed = dc.resume_timed_out(&sub.id, Some(9999)).unwrap();
        assert_eq!(resumed, sub.id);

        // Fresh budget clamped to the global 1800s ceiling and checkpoint
        // flipped back to running.
        let cp = backend.load_delegation_checkpoint(&sub.id).unwrap();
        assert_eq!(cp.status, "running");
        assert_eq!(cp.timeout_secs, 1800);
        assert_eq!(cp.allowed_tools.as_deref(), Some(&["shell".to_string()][..]));

        // Parent suspension re-armed so the eventual terminal event records
        // (instead of hitting the Duplicate drop path).
        assert!(parent.has_pending_async_work());

        // Second resume while the first holds the slot → already running.
        // (recover_async returned early without inserting a RunningEntry
        // because no runtime is installed in tests, so simulate the entry.)
        dc.running.insert(
            sub.id.clone(),
            RunningEntry {
                handle: tokio::spawn(async {}),
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                parent_session_id: parent.session_id.clone(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(1800),
                started_at: chrono::Utc::now(),
                allowed_tools: None,
            },
        );
        let err = dc.resume_timed_out(&sub.id, None).unwrap_err();
        assert!(format!("{:#}", err).contains("already running"));
    }

    #[tokio::test]
    async fn resume_timed_out_default_budget_doubles_original_with_floor_and_ceiling() {
        let (dc, manager, _dir) = coordinator_with_backend();
        let backend = manager.backend();

        // Issue #111 repro: a small original budget (15s) must not default to
        // itself again — the delegation only reaches `resume` after that
        // budget already ran out once. 15 * 2 = 30, below the 600s floor.
        let parent = manager.get_or_create_context("mock:default:u1");
        let sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
        backend
            .save_delegation_checkpoint(&checkpoint_for(&sub.id, &parent.session_id, "timed_out", 15))
            .unwrap();
        dc.resume_timed_out(&sub.id, None).unwrap();
        assert_eq!(backend.load_delegation_checkpoint(&sub.id).unwrap().timeout_secs, 600);

        // Original budget large enough that doubling it clears the floor:
        // 500 * 2 = 1000.
        let sub2 = manager.create_sub_session(&parent.session_id, "coder").unwrap();
        backend
            .save_delegation_checkpoint(&checkpoint_for(&sub2.id, &parent.session_id, "timed_out", 500))
            .unwrap();
        dc.resume_timed_out(&sub2.id, None).unwrap();
        assert_eq!(backend.load_delegation_checkpoint(&sub2.id).unwrap().timeout_secs, 1000);

        // Doubling past the global ceiling still clamps to 1800.
        let sub3 = manager.create_sub_session(&parent.session_id, "coder").unwrap();
        backend
            .save_delegation_checkpoint(&checkpoint_for(&sub3.id, &parent.session_id, "timed_out", 1000))
            .unwrap();
        dc.resume_timed_out(&sub3.id, None).unwrap();
        assert_eq!(backend.load_delegation_checkpoint(&sub3.id).unwrap().timeout_secs, 1800);
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
        let sub_session_id = dc
            .spawn_delegate_async("coder", "do the thing", &parent_id, 60, None, None)
            .unwrap();
        assert!(
            sub_session_id.contains("/s/"),
            "sub_session_id should be a session FQID: {sub_session_id}"
        );
        // current_thread runtime: the spawned body has not been polled yet, so
        // both tables are deterministically populated. The test body never
        // awaits, so the background task is cancelled at runtime drop.
        assert_eq!(dc.running_snapshot(), vec![sub_session_id.clone()]);
        assert_eq!(dc.running_count(), 1);
        let snap = parent.suspension_snapshot().unwrap();
        assert_eq!(snap.pending, vec![sub_session_id]);
    }

    #[tokio::test]
    async fn spawn_async_depth_rejected_without_pending_or_running() {
        let (dc, manager) = coordinator(1);
        let parent = manager.get_or_create_context("mock:default:u1");
        let err = dc
            .spawn_delegate_async("coder", "task", &parent.session_id, 60, None, None)
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
            .spawn_delegate_async("nope", "task", &parent.session_id, 60, None, None)
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
        let sub_session_id = "test/s/sub".to_string();
        // Production path creates a durable checkpoint at spawn; cancel's
        // tombstone only updates an existing entry.
        let checkpoint = crate::storage::DelegationCheckpoint {
            parent_session_id: parent.session_id.clone(),
            sub_session_id: sub_session_id.clone(),
            agent_name: "coder".to_string(),
            status: "running".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: 60,
            allowed_tools: None,
            last_checkpoint: None,
        };
        dc.session_manager
            .backend()
            .save_delegation_checkpoint(&checkpoint)
            .unwrap();
        dc.running.insert(
            sub_session_id.clone(),
            RunningEntry {
                handle: tokio::spawn(async {}),
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                parent_session_id: parent.session_id.clone(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(60),
                started_at: chrono::Utc::now(),
                allowed_tools: None,
            },
        );

        assert!(dc.cancel(&sub_session_id).await);
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Failed {
                sub_session_id: t,
                parent_session_id: s,
                error,
            } => {
                assert_eq!(t, sub_session_id);
                assert_eq!(s, parent.session_id);
                assert_eq!(error, "cancelled");
            }
            other => panic!("expected Failed(cancelled), got {other:?}"),
        }
        assert!(dc.running_snapshot().is_empty());

        // Tombstone is terminal Cancelled → restart skips resume (no
        // duplicate execution), unlike the hot-switch "checkpointed" path.
        let cp = dc
            .session_manager
            .backend()
            .load_delegation_checkpoint(&sub_session_id)
            .unwrap();
        assert_eq!(cp.status, "cancelled");

        // Exactly one event: the two-layer spawn's cancelled branch must NOT
        // emit a second Failed (caller owns the terminal event). Give the
        // aborted task's cleanup tail a chance to run on the current_thread
        // runtime, then confirm the channel is quiet.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "cancel must emit exactly one Failed(cancelled) event"
        );
    }

    #[tokio::test]
    async fn cancel_unknown_sub_session_returns_false() {
        let (dc, _m) = coordinator(3);
        assert!(!dc.cancel("no-such-session").await);
    }

    #[test]
    fn checkpoint_roundtrip_via_backend() {
        let (dc, manager) = coordinator(3);
        let _ = dc; // not needed — we test the backend directly
        let backend = manager.backend();
        let cp = crate::storage::DelegationCheckpoint {
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
        assert_eq!(loaded[0].sub_session_id, "test/s/sub");
        assert_eq!(loaded[0].agent_name, "coder");
        assert_eq!(loaded[0].timeout_secs, 300);

        backend.delete_delegation_checkpoint("test/s/sub").unwrap();
        assert!(backend.load_delegation_checkpoints().is_empty());
    }

    /// 方案 A: terminal cleanup rewrites the checkpoint status (tombstone)
    /// instead of deleting it — single-load roundtrip + status update +
    /// idempotent update of a missing checkpoint.
    #[test]
    fn checkpoint_tombstone_update_via_backend() {
        let (dc, manager) = coordinator(3);
        let _ = dc; // not needed — we test the backend directly
        let backend = manager.backend();
        let cp = crate::storage::DelegationCheckpoint {
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

        // Single-load roundtrip.
        let loaded = backend.load_delegation_checkpoint("test/s/sub").unwrap();
        assert_eq!(loaded.status, "running");
        assert!(backend.load_delegation_checkpoint("no-such-session").is_none());

        // Tombstone: the terminal status is persisted, not deleted.
        backend
            .update_delegation_checkpoint_status("test/s/sub", "timed_out")
            .unwrap();
        let loaded = backend.load_delegation_checkpoint("test/s/sub").unwrap();
        assert_eq!(loaded.status, "timed_out");
        assert_eq!(loaded.agent_name, "coder"); // other fields preserved
        assert_eq!(backend.load_delegation_checkpoints().len(), 1); // still on disk

        // Idempotent: updating a missing checkpoint is a no-op.
        backend
            .update_delegation_checkpoint_status("no-such-session", "failed")
            .unwrap();

        backend.delete_delegation_checkpoint("test/s/sub").unwrap();
        assert!(backend.load_delegation_checkpoints().is_empty());
    }

    #[tokio::test]
    async fn checkpoint_and_cancel_all_empties_running_and_writes_checkpoints() {
        let (dc, manager) = coordinator(3);
        let backend = manager.backend();

        // Insert a hand-crafted running entry.
        let sub_session_id = "test/s/sub".to_string();
        dc.running.insert(
            sub_session_id.clone(),
            RunningEntry {
                handle: tokio::spawn(async {}),
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                parent_session_id: "parent".to_string(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(60),
                started_at: chrono::Utc::now(),
                allowed_tools: None,
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
        assert_eq!(loaded[0].sub_session_id, sub_session_id);
    }

    #[test]
    fn load_checkpoints_returns_persisted_checkpoints() {
        let (dc, manager) = coordinator(3);
        let backend = manager.backend();
        let cp = crate::storage::DelegationCheckpoint {
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
        assert_eq!(loaded[0].sub_session_id, "sub");
        assert_eq!(loaded[0].status, "checkpointed");
    }

    #[test]
    fn worktree_branch_name_uses_subagent_prefix() {
        assert_eq!(
            worktree_branch_name("coder", "deadbeef"),
            "subagent/coder_deadbeef"
        );
        assert_eq!(
            worktree_branch_name("main", "01234567"),
            "subagent/main_01234567"
        );
    }

    // ── Two-layer spawn panic safety net ───────────────────────────────

    /// Verifies that when the inner sub-agent task panics, the outer spawn
    /// catches it and converts the JoinError into a `DelegationEvent::Failed`
    /// whose error message contains "panicked". The running table and mailbox
    /// must also be cleaned so the parent agent does not hang.
    ///
    /// Because `spawn_delegate_async` calls `delegate_with_parent` (which
    /// returns `Err`, not a panic, when no runtime is installed), this test
    /// exercises the two-layer pattern directly: it spawns an outer task
    /// whose inner task panics, using the coordinator's real `running`,
    /// `mailboxes`, and event-channel infrastructure — the same code path
    /// `spawn_delegate_async` uses after the fix.
    #[tokio::test]
    async fn panic_in_inner_task_emits_failed_and_cleans_running() {
        let (dc, manager) = coordinator(3);
        let parent = manager.get_or_create_context("mock:default:u1");
        let (tx, mut rx) = mpsc::channel(8);
        dc.set_event_sender(tx);

        let sub_session_id = "test/s/panic-sub".to_string();
        let parent_session_id = parent.session_id.clone();

        // Production path creates a durable checkpoint before spawning;
        // persist_terminal_checkpoint only updates an existing entry.
        let checkpoint = crate::storage::DelegationCheckpoint {
            parent_session_id: parent.session_id.clone(),
            sub_session_id: sub_session_id.clone(),
            agent_name: "coder".to_string(),
            status: "running".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: 60,
            allowed_tools: None,
            last_checkpoint: None,
        };
        dc.session_manager
            .backend()
            .save_delegation_checkpoint(&checkpoint)
            .unwrap();

        // Set up the coordinator's internal tables just like
        // `spawn_delegate_async` does before spawning.
        let running = Arc::clone(&dc.running);
        let mailboxes = Arc::clone(&dc.mailboxes);
        let event_tx = dc.event_sender();
        let running_sub_session_id = sub_session_id.clone();
        let sub_session_id_clone = sub_session_id.clone();
        let sub_delegator = dc.clone();

        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            // Inner task that panics — simulates a sub-agent whose body
            // panics during execution.
            let inner_handle: tokio::task::JoinHandle<anyhow::Result<String>> =
                tokio::spawn(async {
                    panic!("inner task exploded");
                });

            let result = match inner_handle.await {
                Ok(r) => r,
                Err(je) if je.is_panic() => {
                    let payload = je.into_panic();
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic payload".to_string()
                    };
                    tracing::error!(panic = %msg, "sub-agent panicked");
                    Err(anyhow::anyhow!("sub-agent panicked: {}", msg))
                }
                Err(je) if je.is_cancelled() => {
                    running.remove(&running_sub_session_id);
                    mailboxes.remove(&running_sub_session_id);
                    return;
                }
                Err(je) => Err(anyhow::anyhow!("join error: {}", je)),
            };

            // ── Collection logic (mirrors spawn_delegate_async) ───────
            let duration_secs = start_time.elapsed().as_secs();
            let timed_out_secs = result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<DelegationTimeout>())
                .map(|t| t.secs);
            let sent_message_count = running
                .get(&running_sub_session_id)
                .map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);

            if let Some(tx) = &event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        let _ = tx
                            .send(DelegationEvent::Completed {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id.clone(),
                                summary: summary.clone(),
                                duration_secs,
                                sent_message_count,
                            })
                            .await;
                    }
                    (Err(_), Some(secs)) => {
                        let _ = tx
                            .send(DelegationEvent::TimedOut {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id.clone(),
                                timeout_secs: secs,
                                duration_secs,
                            })
                            .await;
                    }
                    (Err(e), None) => {
                        let _ = tx
                            .send(DelegationEvent::Failed {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id.clone(),
                                error: e.to_string(),
                            })
                            .await;
                    }
                }
            }

            let terminal = if timed_out_secs.is_some() {
                DelegationStatus::TimedOut
            } else if result.is_ok() {
                DelegationStatus::Completed
            } else {
                DelegationStatus::Failed
            };
            if let Some(entry) = running.get(&running_sub_session_id) {
                if let Ok(mut status) = entry.status.write() {
                    *status = terminal;
                }
            }
            running.remove(&running_sub_session_id);
            mailboxes.remove(&running_sub_session_id);
            if terminal == DelegationStatus::Completed {
                let _ = sub_delegator
                    .session_manager
                    .backend()
                    .delete_delegation_checkpoint(&running_sub_session_id);
            } else {
                sub_delegator.persist_terminal_checkpoint(&running_sub_session_id, terminal);
            }
        });

        dc.running.insert(
            sub_session_id.clone(),
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                parent_session_id: parent.session_id.clone(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(60),
                started_at: chrono::Utc::now(),
                allowed_tools: None,
            },
        );

        // Wait for the Failed event (panic path → no DelegationTimeout →
        // falls into the generic Err branch).
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for event")
            .expect("event channel closed");
        match event {
            DelegationEvent::Failed { error, .. } => {
                assert!(
                    error.contains("panicked"),
                    "error should mention panic: {error}"
                );
            }
            other => panic!("expected Failed event, got {other:?}"),
        }

        // Give the cleanup tail a chance to run on the current_thread
        // runtime, then verify the running table and checkpoint tombstone.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            dc.running.get(&sub_session_id).is_none(),
            "running table should be cleaned up after panic"
        );
        let cp = dc
            .session_manager
            .backend()
            .load_delegation_checkpoint(&sub_session_id);
        assert!(
            cp.is_some_and(|c| c.status == "failed"),
            "checkpoint should carry a 'failed' tombstone after panic"
        );
    }

    // ── RunningEntry timeout_secs / started_at checkpoint fallback ──────

    /// `checkpoint_and_cancel_all`'s fallback (no existing durable
    /// checkpoint) must use the `RunningEntry`'s actual `timeout_secs` and
    /// `started_at` instead of hardcoded defaults (600 s / now).
    #[tokio::test]
    async fn checkpoint_fallback_uses_running_entry_timeout_and_started_at() {
        let (dc, manager) = coordinator(3);
        let backend = manager.backend();

        let started = chrono::Utc::now() - chrono::Duration::seconds(100);
        let sub_session_id = "test/s/fallback-sub".to_string();
        dc.running.insert(
            sub_session_id.clone(),
            RunningEntry {
                handle: tokio::spawn(async {}),
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: "coder".to_string(),
                parent_session_id: "parent".to_string(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(42),
                started_at: started,
                allowed_tools: None,
            },
        );

        // No pre-existing checkpoint → triggers the fallback branch.
        assert!(backend
            .load_delegation_checkpoint(&sub_session_id)
            .is_none());

        dc.checkpoint_and_cancel_all();

        let cp = backend
            .load_delegation_checkpoint(&sub_session_id)
            .expect("checkpoint should be written");
        assert_eq!(
            cp.timeout_secs, 42,
            "fallback should use entry's timeout_secs"
        );
        assert_eq!(
            cp.started_at, started,
            "fallback should use entry's started_at, not Utc::now()"
        );
        assert_eq!(cp.status, "checkpointed");
    }
}
