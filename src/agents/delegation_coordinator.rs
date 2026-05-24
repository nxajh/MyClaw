//! `DelegationCoordinator` — creates and runs specialized sub-agents on demand.
//!
//! RFC v2 §三.D: implements [`AgentDelegator`](crate::agents::AgentDelegator) by
//! creating temporary `AgentLoop` instances with restricted tool sets and
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
use std::sync::Arc;

use parking_lot::RwLock;

use crate::agents::delegation::{DelegationEvent, DelegationManager};
use crate::agents::session::{BackendPersistHook, PersistHook, Session, SessionManager};
use crate::agents::skills::SkillManager;
use crate::agents::tool_registry::ToolRegistry;
use crate::config::sub_agent::{AgentIsolation, SubAgentConfig};
use crate::providers::ProviderRegistry;

/// F36: prefer this name. `SubAgentDelegator` remains as a type alias to
/// keep external callers compiling while H47's removal of `TaskDelegator`
/// propagates; the alias will be deleted once every reference reads
/// `DelegationCoordinator`.
pub type SubAgentDelegator = DelegationCoordinator;

/// Holds sub-agent configs and creates temporary AgentLoops for delegation.
///
/// Implements [`AgentDelegator`](crate::agents::AgentDelegator); the legacy
/// `TaskDelegator` dual-impl was removed in H47 (callers now go through
/// `AgentDelegator`, which carries `&Session` so per-user context is
/// available end-to-end).
#[derive(Clone)]
pub struct DelegationCoordinator {
    /// Sub-agent configurations, indexed by name.
    configs: super::AgentRegistry,
    /// Shared service registry (for LLM access).
    registry: Arc<dyn ProviderRegistry>,
    /// Parent tool registry (tools are filtered per sub-agent).
    tools: Arc<ToolRegistry>,
    /// Parent skill manager (shared read-only).
    skills: Arc<RwLock<SkillManager>>,
    /// Default max_tool_calls from parent agent config.
    default_max_tool_calls: usize,
    /// Shared SessionManager — B15: sub-sessions are top-level peers of
    /// regular sessions, distinguished by `meta.parent_session_id`.
    /// Replaces the old per-parent `JsonFileBackend` rooted at
    /// `{sessions_root}/{parent}/subagents/`.
    session_manager: Arc<SessionManager>,
    /// Sessions directory — only used to write `subagent_running_*.json`
    /// marker files for hot-switch recovery. H50 will delete the marker
    /// mechanism (recovery.rs scans `SessionManager.list_all_sessions`
    /// + `parent_session_id` directly), at which point this field goes
    /// away too.
    sessions_root: PathBuf,
    /// Root directory for git worktrees (when isolation = worktree).
    worktrees_root: PathBuf,
}

impl DelegationCoordinator {
    pub fn new(
        configs: super::AgentRegistry,
        registry: Arc<dyn ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        skills: Arc<RwLock<SkillManager>>,
        default_max_tool_calls: usize,
        session_manager: Arc<SessionManager>,
        sessions_root: PathBuf,
        worktrees_root: PathBuf,
    ) -> Self {
        Self {
            configs,
            registry,
            tools,
            skills,
            default_max_tool_calls,
            session_manager,
            sessions_root,
            worktrees_root,
        }
    }

    fn find_config(&self, name: &str) -> Option<SubAgentConfig> {
        self.configs.get(name)
    }

    /// Build a filtered ToolRegistry containing only the allowed tools.
    fn build_filtered_tools(&self, allowed_tools: &[String]) -> ToolRegistry {
        let mut filtered = ToolRegistry::new();
        for tool_name in allowed_tools {
            if let Some(tool) = self.tools.get(tool_name) {
                filtered.register(tool);
            } else {
                tracing::warn!(tool = %tool_name, "sub-agent references unknown tool, skipping");
            }
        }
        filtered
    }

    /// Create a persisted sub-session for a sub-agent invocation.
    ///
    /// B15: sub-sessions are top-level peers of regular sessions, sharing
    /// the same backend. `SessionManager.create_sub_session` writes
    /// `meta.parent_session_id` and `meta.agent_name` so recovery can
    /// link sub-sessions back to their parent without per-parent
    /// subdirectories. The returned `PersistHook` points at the shared
    /// backend.
    ///
    /// Returns `(session_id, Some(hook))` on success, or `(random_id, None)`
    /// if the parent is unknown or storage is unavailable — letting the
    /// sub-agent run ephemerally.
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>> {
        Box::pin(async move {
        let config = self.find_config(agent_name)
            .ok_or_else(|| {
                let available = self.configs.names();
                anyhow::anyhow!(
                    "Unknown sub-agent '{}'. Available: {}",
                    agent_name, available.join(", ")
                )
            })?;

        // Generate a unique task_id and create a running-state marker file so the
        // daemon can detect interrupted sub-agents after a hot-switch restart.
        let task_id = task_id_override.map(|s| s.to_string())
            .unwrap_or_else(|| format!("del_{}", uuid::Uuid::new_v4()));
        // We'll write the marker after we know the sub_session_id.
        // For now, save the path.
        let marker_path = self.sessions_root.join(format!("subagent_running_{}.json", task_id));

        tracing::info!(
            agent = %config.name,
            task_id = %task_id,
            parent = %parent_session_id,
            tools = ?config.tools,
            task_len = task.len(),
            "creating sub-agent for delegation"
        );

        let tools = self.build_filtered_tools(&config.tools);

        // --- worktree creation (moved BEFORE prompt so we can inject the path) ---
        let (worktree_path, cleanup_worktree, branch_name) = match config.isolation {
            AgentIsolation::Worktree => {
                let task_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
                let branch_name = format!("subagent/{}_{}", config.name, task_id);
                let worktree_path = self.worktrees_root.join(format!("{}_{}", config.name, task_id));

                if let Some(parent) = worktree_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if worktree_path.exists() {
                    let _ = std::fs::remove_dir_all(&worktree_path);
                }

                let output = std::process::Command::new("git")
                    .args(["worktree", "add", "-b", &branch_name, &worktree_path.to_string_lossy(), "HEAD"])
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
            format!("You are a specialized agent named '{}'.{}", config.name, workspace_section)
        } else {
            format!("{}{}", config.system_prompt, workspace_section)
        };

        let (session_id, persist_hook) = self.open_sub_session(parent_session_id, &config.name);

        // Write the marker file now that we know the sub_session_id.
        let marker_state = serde_json::json!({
            "agent_name": config.name,
            "task_id": task_id,
            "task_preview": task.chars().take(200).collect::<String>(),
            "started_at": chrono::Utc::now().to_rfc3339(),
            "parent_session_id": parent_session_id,
            "sub_session_id": session_id,
            "session_key": session_key.unwrap_or(""),
            "reply_target": reply_target.unwrap_or(""),
        });
        if let Err(e) = std::fs::write(&marker_path, serde_json::to_string_pretty(&marker_state).unwrap_or_default()) {
            tracing::warn!(path = %marker_path.display(), err = %e, "failed to write sub-agent marker file");
        }

        let session = Session::new(session_id);

        let agent_config = crate::agents::AgentConfig {
            max_tool_calls: config.max_tool_calls.unwrap_or(self.default_max_tool_calls),
            prompt_config: crate::agents::SystemPromptConfig {
                workspace_dir,
                permission_mode: crate::agents::PermissionMode::Full,
                identity_header: Some(identity),
                ..Default::default()
            },
            ..Default::default()
        };

        let agent = crate::agents::Agent::new(
            self.registry.clone(),
            Arc::new(tools),
            Arc::new(RwLock::new(SkillManager::new())),
            agent_config,
        );
        let agent = match &config.model {
            Some(m) => agent.with_model(m.clone()),
            None => agent,
        };
        let mut loop_ = agent.loop_for_with_persist(session, persist_hook);

        tracing::debug!(agent = %config.name, "sub-agent started");
        let result = loop_.run(task, None, None).await;
        match &result {
            Ok(text) => tracing::debug!(agent = %config.name, text_len = text.len(), "sub-agent completed"),
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
                            .args(["merge", "--no-ff", "-m",
                                   &format!("merge sub-agent: {}", config.name),
                                   branch_name])
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
                                    config.name, worktree_path.display()
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
                .args(["worktree", "remove", "--force", &worktree_path.to_string_lossy()])
                .output();
            if let Some(ref bn) = branch_name {
                let _ = std::process::Command::new("git")
                    .args(["branch", "-D", bn])
                    .output();
            }
            tracing::debug!(path = %worktree_path.display(), "cleaned up worktree and branch");
        }

        // Cleanup the running-state marker file — sub-agent is done (success or failure).
        let _ = std::fs::remove_file(&marker_path);

        result
        }) // end Box::pin
    }

    /// Delegate a task asynchronously — spawns sub-agent in a background tokio task.
    ///
    /// `parent_session_id` is the hex session ID of the calling agent; used to
    /// locate the `subagents/` directory for history persistence.
    pub fn delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        parent_session_id: &str,
        reply_target: &str,
        delegation_manager: &DelegationManager,
    ) -> anyhow::Result<String> {
        let config = self.find_config(agent_name)
            .ok_or_else(|| {
                let available = self.configs.names();
                anyhow::anyhow!(
                    "Unknown sub-agent '{}'. Available: {}",
                    agent_name, available.join(", ")
                )
            })?;

        let task_id = format!("del_{}", uuid::Uuid::new_v4());

        tracing::info!(
            agent = %config.name,
            task_id = %task_id,
            task_len = task.len(),
            "spawning sub-agent in background"
        );

        let configs = self.configs.clone();
        let registry = self.registry.clone();
        let tools = self.tools.clone();
        let skills = self.skills.clone();
        let default_max_tool_calls = self.default_max_tool_calls;
        let session_manager = Arc::clone(&self.session_manager);
        let sessions_root = self.sessions_root.clone();
        let worktrees_root = self.worktrees_root.clone();
        let task_owned = task.to_string();
        let parent_session_id_owned = parent_session_id.to_string();
        let session_key_owned = parent_session_id.to_string();
        let reply_target_owned = reply_target.to_string();
        let event_tx = delegation_manager.event_sender();
        let task_id_clone = task_id.clone();
        let agent_name_owned = agent_name.to_string();

        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            let sub_delegator = DelegationCoordinator {
                configs,
                registry,
                tools,
                skills,
                default_max_tool_calls,
                session_manager,
                sessions_root,
                worktrees_root,
            };

            let result = sub_delegator
                .delegate_with_parent(
                    &agent_name_owned, &task_owned, &parent_session_id_owned,
                    Some(&task_id_clone), Some(&session_key_owned), Some(&reply_target_owned),
                )
                .await;

            let duration_secs = start_time.elapsed().as_secs();

            match result {
                Ok(summary) => {
                    tracing::info!(task_id = %task_id_clone, duration_secs, "sub-agent completed successfully");
                    let _ = event_tx.send(DelegationEvent::Completed {
                        task_id: task_id_clone.clone(),
                        parent_session_id: parent_session_id_owned,
                        reply_target: reply_target_owned,
                        summary,
                        duration_secs,
                    }).await;
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id_clone, duration_secs, err = %e, "sub-agent failed");
                    let _ = event_tx.send(DelegationEvent::Failed {
                        task_id: task_id_clone.clone(),
                        parent_session_id: parent_session_id_owned,
                        reply_target: reply_target_owned,
                        error: e.to_string(),
                    }).await;
                }
            }
        });

        delegation_manager.register(task_id.clone(), handle);
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
        let reply_target = parent_session
            .reply_target()
            .map(|s| s.to_string());
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
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect()
    }
}
