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
/// 监视 skills/ 和 agents/ 目录变化（P1: 随 data dir），通知 AgentLoop。
/// 通过 `rx` channel 发送变化信号。
pub struct WorkspaceWatcher {
    /// 变化信号接收端（AgentLoop 持有）
    pub rx: watch::Receiver<ChangeSet>,
    // watcher 必须存活才能持续监听
    _watcher: RecommendedWatcher,
}

impl WorkspaceWatcher {
    pub fn new(workspace_dir: &Path, knowledge_dir: &Path) -> Result<Self> {
        // P1: skills/agents 热加载目录随 data dir（系统配置面）。此构造器保留
        // 旧签名（workspace 侧）以兼容既有调用；daemon 侧请用 `new_with_roots`。
        Self::new_with_roots(
            workspace_dir.join("skills"),
            workspace_dir.join("agents"),
            knowledge_dir,
        )
    }

    /// P1 variant: hot-reload roots supplied directly (data-dir derived).
    pub fn new_with_roots(
        skills_dir: PathBuf,
        agents_dir: PathBuf,
        memory_dir: &Path,
    ) -> Result<Self> {
        let (tx, rx) = watch::channel(ChangeSet::default());

        let memory_dir = memory_dir.to_path_buf();

        let skills_dir_c = skills_dir.clone();
        let agents_dir_c = agents_dir.clone();
        let memory_dir_c = memory_dir.clone();

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
        if agents_dir.exists() {
            watcher.watch(&agents_dir, RecursiveMode::Recursive)?;
        }
        // memory dir is ensured by daemon startup
        if memory_dir.exists() {
            watcher.watch(&memory_dir, RecursiveMode::Recursive)?;
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
        workspace_dir: &Path,
        knowledge_dir: &Path,
        agent_registry: crate::agents::AgentRegistry,
        skill_manager: Arc<RwLock<super::skills::SkillManager>>,
    ) -> Result<ManagedWatcherGuard> {
        Self::spawn_managed_with_roots(
            workspace_dir.join("skills"),
            workspace_dir.join("agents"),
            knowledge_dir,
            agent_registry,
            skill_manager,
        )
    }

    /// P1 variant: reload roots supplied directly (data-dir derived).
    pub fn spawn_managed_with_roots(
        skills_dir: PathBuf,
        agents_dir: PathBuf,
        knowledge_dir: &Path,
        agent_registry: crate::agents::AgentRegistry,
        skill_manager: Arc<RwLock<super::skills::SkillManager>>,
    ) -> Result<ManagedWatcherGuard> {
        let watcher = Self::new_with_roots(
            skills_dir.clone(),
            agents_dir.clone(),
            knowledge_dir,
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
                            let defs = super::skill_loader::load_skills_from_dir(&skills_dir);
                            let skills: Vec<crate::agents::Skill> =
                                defs.iter().map(crate::agents::Skill::from_definition).collect();
                            let count = skills.len();
                            skill_manager.write().reload(skills);
                            tracing::info!(skill_count = count, "skills hot-reloaded by watcher");
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
