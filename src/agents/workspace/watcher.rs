//! WorkspaceWatcher — 监视 skills/ 和 agents/ 目录变化，通知 AgentLoop。
//!
//! 两种使用模式：
//!  1. **观察者模式**（`new`）：暴露 `watch::Receiver<ChangeSet>`，调用方
//!     自己根据 ChangeSet 决定如何响应。
//!  2. **自维护模式**（`spawn_managed`，RFC v2 §三.D）：watcher 持有
//!     AgentRegistry + SkillManager 引用，目录变化时直接调用其 reload
//!     方法，调用方无需关心信号流。
//!
//! 使用 notify crate 实现文件系统监视。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use tokio::sync::watch;

/// 目录变化描述
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub skills_changed: bool,
    pub agents_changed: bool,
    pub memory_changed: bool,
}

/// 文件系统监视器。
///
/// 监视 skills/ 和 agents/ 目录变化（P1: 随 base dir），通知 AgentLoop。
/// 通过 `rx` channel 发送变化信号。
pub struct WorkspaceWatcher {
    /// 变化信号接收端（AgentLoop 持有）
    pub rx: watch::Receiver<ChangeSet>,
    // watcher 必须存活才能持续监听
    _watcher: RecommendedWatcher,
}

impl WorkspaceWatcher {
    /// P1 variant: hot-reload roots supplied directly (base-dir derived).
    ///
    /// `agents_skills_dir` is the cross-agent shared skills library
    /// (`~/.agents/skills`, issue #83) — `None` when `[skills]
    /// include_agents_dir = false`. Changes under either skills directory
    /// set `skills_changed`.
    pub fn new(
        skills_dir: PathBuf,
        agents_skills_dir: Option<PathBuf>,
        agents_dir: PathBuf,
        memory_dir: &Path,
        users_dir: PathBuf,
    ) -> Result<Self> {
        let (tx, rx) = watch::channel(ChangeSet::default());

        let memory_dir = memory_dir.to_path_buf();

        let skills_dir_c = skills_dir.clone();
        let agents_skills_dir_c = agents_skills_dir.clone();
        let agents_dir_c = agents_dir.clone();
        let memory_dir_c = memory_dir.clone();
        let users_dir_c = users_dir.clone();

        let mut watcher = notify::recommended_watcher(
            move |res: std::result::Result<notify::Event, notify::Error>| {
                let event = match res {
                    Ok(e) => e,
                    Err(_) => return,
                };

                // Only care about content changes.
                match event.kind {
                    EventKind::Create(_)
                    | EventKind::Modify(_)
                    | EventKind::Remove(_)
                    | EventKind::Any => {}
                    _ => return,
                }

                let mut changes = ChangeSet::default();
                for path in &event.paths {
                    if path.starts_with(&skills_dir_c) {
                        changes.skills_changed = true;
                    }
                    if path.starts_with(&users_dir_c) && path.components().any(|c| c.as_os_str() == "skills") {
                        changes.skills_changed = true;
                    }
                    if let Some(ref dir) = agents_skills_dir_c {
                        if path.starts_with(dir) {
                            changes.skills_changed = true;
                        }
                    }
                    if path.starts_with(&agents_dir_c) {
                        changes.agents_changed = true;
                    }
                    if path.starts_with(&memory_dir_c) {
                        // Only trigger for .md files
                        if path.extension().is_some_and(|ext| ext == "md") {
                            changes.memory_changed = true;
                        }
                    }
                }

                if changes.skills_changed || changes.agents_changed || changes.memory_changed {
                    let _ = tx.send(changes);
                }
            },
        )?;

        if skills_dir.exists() {
            watcher.watch(&skills_dir, RecursiveMode::Recursive)?;
        }
        if let Some(ref dir) = agents_skills_dir {
            if dir.exists() {
                watcher.watch(dir, RecursiveMode::Recursive)?;
            }
        }
        if agents_dir.exists() {
            watcher.watch(&agents_dir, RecursiveMode::Recursive)?;
        }
        // memory dir is ensured by daemon startup
        if memory_dir.exists() {
            watcher.watch(&memory_dir, RecursiveMode::Recursive)?;
        }
        // users dir: P2 moved user-layer skills under `users/{uuid}/skills/`.
        // The event callback already classifies those paths into
        // `skills_changed`, but without this registration notify never
        // delivers events from this subtree, leaving user-layer skill
        // edits invisible to hot-reload (draft promote/delete stalled the
        // live index until a manual /reload or restart).
        if users_dir.exists() {
            watcher.watch(&users_dir, RecursiveMode::Recursive)?;
        }

        Ok(Self {
            rx,
            _watcher: watcher,
        })
    }

    /// Self-maintaining mode: spawn a tokio task that owns the watcher and
    /// directly drives `agent_registry.reload_from_dir` and `skill_manager.reload`
    /// when the corresponding directories change.
    ///
    /// Callers no longer need to subscribe to `rx`; the watcher is the
    /// authoritative reloader. Returns a guard whose Drop terminates the task.
    pub fn spawn_managed(
        skills_dir: PathBuf,
        agents_skills_dir: Option<PathBuf>,
        agents_dir: PathBuf,
        memory_root: &Path,
        users_dir: PathBuf,
        agent_registry: crate::agents::AgentRegistry,
        skill_manager: Arc<RwLock<super::skills::SkillManager>>,
    ) -> Result<ManagedWatcherGuard> {
        let watcher = Self::new(
            skills_dir.clone(),
            agents_skills_dir.clone(),
            agents_dir.clone(),
            memory_root,
            users_dir.clone(),
        )?;
        let mut rx = watcher.rx.clone();

        // Clone for the guard: the async block below moves the originals.
        let guard_agents_dir = agents_dir.clone();
        let guard_skills_dir = skills_dir.clone();

        let token = tokio_util::sync::CancellationToken::new();
        let task_token = token.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_token.cancelled() => break,
                    res = rx.changed() => {
                        if res.is_err() { break; }
                        let changes = rx.borrow().clone();
                        if changes.agents_changed {
                            let n = agent_registry.reload_from_dir(&agents_dir);
                            tracing::info!(agent_count = n, "agents hot-reloaded by watcher");
                        }
                        if changes.skills_changed {
                            // Merge both layers into one `reload()` call —
                            // `reload()` clears and replaces everything, so
                            // calling it once per dir would wipe out the
                            // other layer's skills (issue #83).
                            let user_skills_map = super::skill_loader::load_all_users_skills(&users_dir);
                            let agent_defs = super::skill_loader::load_skills_from_dir(&skills_dir);
                            let shared_defs = if let Some(d) = &agents_skills_dir {
                                super::skill_loader::load_skills_from_dir(d)
                            } else {
                                Vec::new()
                            };
                            
                            let mut write_guard = skill_manager.write();
                            write_guard.reload_from_definitions(user_skills_map, agent_defs, shared_defs);
                            tracing::info!(skill_count = write_guard.skill_count(), "skills hot-reloaded by watcher");
                        }
                        // memory_changed is left to AttachmentManager.diff_memory
                        // because the attachment payload is computed per-turn.
                    }
                }
            }
        });

        Ok(ManagedWatcherGuard {
            _watcher: watcher,
            _handle: handle,
            cancel: token,
            agents_dir: guard_agents_dir,
            skills_dir: guard_skills_dir,
        })
    }
}

/// Handle returned by `WorkspaceWatcher::spawn_managed`. Dropping it cancels
/// the watcher task and releases the inotify handles.
pub struct ManagedWatcherGuard {
    _watcher: WorkspaceWatcher,
    _handle: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
    /// Directories the watcher reloads on change — exposed so callers can
    /// trigger a manual reload (e.g. `/reload` slash command).
    pub agents_dir: PathBuf,
    pub skills_dir: PathBuf,
}

impl Drop for ManagedWatcherGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
