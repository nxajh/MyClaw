use std::path::PathBuf;

use anyhow::{Context, Result};

// ── Plan 类型 ────────────────────────────────────────────────────────────────

/// 备份步骤：全部在 steps 之前执行；目标已存在则跳过（保留首轮原始备份，
/// 崩溃后重跑不覆盖）。
#[derive(Debug)]
pub struct Backup {
    pub from: PathBuf,
    pub to: PathBuf,
    pub label: String,
}

/// 变更步骤：按序执行（互不重叠，顺序安全）。
#[derive(Debug)]
pub enum Step {
    WriteJson {
        path: PathBuf,
        body: String,
        label: String,
    },
    RenameDir {
        from: PathBuf,
        to: PathBuf,
        label: String,
    },
    RenameFile {
        from: PathBuf,
        to: PathBuf,
        label: String,
    },
    MoveDir {
        from: PathBuf,
        to: PathBuf,
        label: String,
    },
}

impl Step {
    /// 人类可读描述（干跑展示用）。
    pub fn label(&self) -> &str {
        match self {
            Step::WriteJson { label, .. }
            | Step::RenameDir { label, .. }
            | Step::RenameFile { label, .. }
            | Step::MoveDir { label, .. } => label,
        }
    }
}

/// 一次迁移的全部动作（备份 + 步骤）。空 = 无需迁移。
#[derive(Debug, Default)]
pub struct MigrationPlan {
    pub backups: Vec<Backup>,
    pub steps: Vec<Step>,
}

impl MigrationPlan {
    /// 是否需要迁移（无任何动作 = 数据已符合目标形态）。
    pub fn is_empty(&self) -> bool {
        self.backups.is_empty() && self.steps.is_empty()
    }

    /// 执行：先全部备份，再按序执行步骤。单步失败 → 报错返回（.bak 保留，
    /// 可手动恢复；重跑幂等）。
    pub fn apply(&self) -> Result<()> {
        for b in &self.backups {
            if b.to.exists() {
                continue; // 已有备份（重跑/崩溃恢复）→ 保留首轮原始备份
            }
            if let Some(parent) = b.to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("migration: 创建备份目录 {}", parent.display()))?;
            }
            std::fs::copy(&b.from, &b.to).with_context(|| {
                format!(
                    "migration: 备份 {} → {} 失败",
                    b.from.display(),
                    b.to.display()
                )
            })?;
        }
        for s in &self.steps {
            match s {
                Step::WriteJson { path, body, label } => {
                    if let Some(parent) = path.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            tracing::warn!(
                                err = %e,
                                "migration: 创建目录 {} 失败，跳过步骤：{label}",
                                parent.display()
                            );
                            continue;
                        }
                    }
                    if let Err(e) = std::fs::write(path, body) {
                        // 单步失败不阻塞全局（如个别目录 owner 异常/只读）：
                        // warn + 跳过，其余步骤继续。
                        tracing::warn!(
                            err = %e,
                            "migration: 写 {label}（{}）失败，跳过该步骤",
                            path.display()
                        );
                        continue;
                    }
                }
                Step::RenameDir { from, to, label }
                | Step::MoveDir { from, to, label } => {
                    if to.exists() {
                        continue; // 目标已存在（重跑）→ 跳过
                    }
                    if let Some(parent) = to.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            tracing::warn!(
                                err = %e,
                                "migration: 创建目录 {} 失败，跳过步骤：{label}",
                                parent.display()
                            );
                            continue;
                        }
                    }
                    if let Err(e) = std::fs::rename(from, to) {
                        tracing::warn!(
                            err = %e,
                            "migration: {label}（{} → {}）失败，跳过该步骤",
                            from.display(),
                            to.display()
                        );
                        continue;
                    }
                }
                Step::RenameFile { from, to, label } => {
                    if !from.exists() {
                        continue;
                    }
                    if to.exists() {
                        continue;
                    }
                    if let Some(parent) = to.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            tracing::warn!(
                                err = %e,
                                "migration: 创建目录 {} 失败，跳过步骤：{label}",
                                parent.display()
                            );
                            continue;
                        }
                    }
                    if let Err(e) = std::fs::rename(from, to) {
                        tracing::warn!(
                            err = %e,
                            "migration: {label}（{} → {}）失败，跳过该步骤",
                            from.display(),
                            to.display()
                        );
                        continue;
                    }
                }
            }
        }
        Ok(())
    }
}

/// 迁移结果摘要（日志 / CLI 展示用）。
#[derive(Debug, Clone, Copy)]
pub struct MigrationReport {
    pub backups: usize,
    pub steps: usize,
    pub migrated: bool,
}
