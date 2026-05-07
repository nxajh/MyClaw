//! WorkspaceWatcher — 监视 skills/ 和 agents/ 目录变化，通知 AgentLoop。
//!
//! 使用 notify crate 实现文件系统监视。
//! 变化信号通过 tokio::sync::watch channel 传递。

use std::path::Path;

use anyhow::Result;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
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
/// 监视 `workspace/skills/` 和 `workspace/agents/` 目录，
/// 通过 `rx` channel 发送变化信号。
pub struct WorkspaceWatcher {
    /// 变化信号接收端（AgentLoop 持有）
    pub rx: watch::Receiver<ChangeSet>,
    // watcher 必须存活才能持续监听
    _watcher: RecommendedWatcher,
}

impl WorkspaceWatcher {
    pub fn new(workspace_dir: &Path) -> Result<Self> {
        let (tx, rx) = watch::channel(ChangeSet::default());

        let skills_dir = workspace_dir.join("skills");
        let agents_dir = workspace_dir.join("agents");
        let memory_dir = workspace_dir.join("memory");

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
}
