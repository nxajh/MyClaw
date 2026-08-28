//! Core delegation logic: sub-session creation, depth gating, and
//! `delegate_with_parent` (shared by the sync and async paths).

use std::path::PathBuf;
use std::sync::Arc;

use crate::agents::delegation::{DelegationStatus, DelegationTimeout, SubAgentMailbox};
use crate::agents::session::{BackendPersistHook, PersistHook};
use crate::agents::SessionContext;
use crate::config::sub_agent::AgentIsolation;
use crate::ids::{Fqid, TYPE_SESSION};

use super::DelegationCoordinator;
use super::lifecycle::should_gc_sub_session;
use super::worktree::worktree_branch_name;

impl DelegationCoordinator {
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
    pub(super) fn session_depth(&self, session_id: &str) -> u32 {
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
    pub(super) fn check_depth(&self, parent_session_id: &str) -> anyhow::Result<()> {
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
}
