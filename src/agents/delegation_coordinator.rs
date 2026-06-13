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

use crate::agents::delegation::DelegationEvent;
use crate::agents::session::{BackendPersistHook, PersistHook, SessionManager};
use crate::config::sub_agent::AgentIsolation;

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
    /// In-flight background delegations (task_id → JoinHandle). Powers
    /// `/agent_list` (read snapshot) and `/agent_kill` (abort by id).
    running: Arc<DashMap<String, JoinHandle<()>>>,
    /// Sender for `DelegationEvent`s, set once by the daemon via
    /// `set_event_sender` when wiring the orchestrator's `delegation_rx`.
    event_tx_cell: Arc<OnceLock<mpsc::Sender<DelegationEvent>>>,
}

impl DelegationCoordinator {
    pub fn new(
        configs: Arc<super::AgentRegistry>,
        session_manager: Arc<SessionManager>,
        worktrees_root: PathBuf,
    ) -> Self {
        Self {
            configs,
            session_manager,
            worktrees_root,
            runtime_cell: Arc::new(OnceLock::new()),
            running: Arc::new(DashMap::new()),
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

    /// Cancel a running background task by id.
    pub fn cancel(&self, task_id: &str) -> bool {
        if let Some((_, handle)) = self.running.remove(task_id) {
            handle.abort();
            true
        } else {
            false
        }
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
            return (format!("{:016x}", rand::random::<u64>()), None);
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
                (format!("{:016x}", rand::random::<u64>()), None)
            }
        }
    }

    /// Core delegation logic — shared by sync and async paths.
    ///
    /// Returns a boxed future to break the async recursion cycle:
    /// delegate_with_parent → AgentLoop::run → compact_impl → summarize_inline
    /// → execute_tool → delegate_with_parent (nested sub-agent).
    pub fn delegate_with_parent<'a>(
        &'a self,
        agent_name: &'a str,
        task: &'a str,
        parent_session_id: &'a str,
        task_id_override: Option<&'a str>,
        session_key: Option<&'a str>,
        reply_target: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>
    {
        Box::pin(async move {
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
                .unwrap_or_else(|| format!("del_{}", uuid::Uuid::new_v4()));

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

            {
                let mut session = sub_ctx.session.lock().await;
                session.session_override.run_mode = Some(crate::config::agent::RunMode::Background);
                session.session_override.permission_mode =
                    Some(crate::agents::PermissionMode::Full);
                if let Some(ref m) = config.model {
                    session.session_override.model = Some(m.clone());
                }
                session.session_override.system_prompt_override = Some(identity.clone());
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
            };

            tracing::debug!(agent = %config.name, "sub-agent started");
            let result = sub_ctx
                .process_turn(synthetic, None, runtime)
                .await
                .map(|tr| tr.text);

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
                                    tracing::warn!(
                                        branch = %branch_name,
                                        stderr = %String::from_utf8_lossy(&m.stderr),
                                        "merge conflict — aborting merge, worktree preserved"
                                    );
                                    let _ = std::process::Command::new("git")
                                        .args(["merge", "--abort"])
                                        .output();
                                    return Err(anyhow::anyhow!(
                                        "sub-agent '{}' completed but merge failed (conflict). Worktree preserved at {}",
                                        config.name,
                                        worktree_path.display()
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

            // Cleanup worktree + branch (only on success).
            if cleanup_worktree.is_some() && result.is_ok() {
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

            result
        }) // end Box::pin
    }

    /// Delegate a task asynchronously — spawns the sub-agent in a
    /// background tokio task whose JoinHandle is stashed in `running`
    /// so `/agent_list` and `/agent_kill` can see it.
    pub fn delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        parent_session_id: &str,
        reply_target: &str,
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

        let task_id = format!("del_{}", uuid::Uuid::new_v4());

        tracing::info!(
            agent = %config.name,
            task_id = %task_id,
            task_len = task.len(),
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
                )
                .await;

            let duration_secs = start_time.elapsed().as_secs();

            if let Some(tx) = event_tx {
                match result {
                    Ok(summary) => {
                        tracing::info!(task_id = %task_id_clone, duration_secs, "sub-agent completed successfully");
                        let _ = tx
                            .send(DelegationEvent::Completed {
                                task_id: task_id_clone.clone(),
                                parent_session_id: parent_session_id_owned,
                                reply_target: reply_target_owned,
                                summary,
                                duration_secs,
                            })
                            .await;
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %task_id_clone, duration_secs, err = %e, "sub-agent failed");
                        let _ = tx
                            .send(DelegationEvent::Failed {
                                task_id: task_id_clone.clone(),
                                parent_session_id: parent_session_id_owned,
                                reply_target: reply_target_owned,
                                error: e.to_string(),
                            })
                            .await;
                    }
                }
            }

            // Self-remove from the running table once the spawn finishes.
            running.remove(&running_task_id);
        });

        self.running.insert(task_id.clone(), handle);
        Ok(task_id)
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
    ) -> anyhow::Result<String> {
        let reply_target = parent_session.reply_target().map(|s| s.to_string());
        self.delegate_with_parent(
            agent_name,
            task,
            &parent_session.id,
            None,
            None,
            reply_target.as_deref(),
        )
        .await
    }

    fn list_available(&self) -> Vec<(String, Option<String>)> {
        self.configs
            .values_cloned()
            .into_iter()
            .map(|a| (a.config.name.clone(), a.config.description.clone()))
            .collect()
    }
}
