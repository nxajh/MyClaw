use std::path::{Path, PathBuf};

use anyhow::Result;

use super::jobs::migrate_jobs;
use super::sessions::{migrate_session_meta_task_ids, migrate_sessions, migrate_tasks};
use super::types::{MigrationPlan, MigrationReport};
use super::users::{archive_legacy_user_dirs, migrate_resolver, migrate_users};

// ── 入口 ─────────────────────────────────────────────────────────────────────

/// 默认 base_dir。委托给 `config::default_base_dir`（单一权威来源），不再
/// 自己重新算一遍——两份独立实现曾经因为改一处忘改另一处而分叉过。
pub fn default_base_dir() -> PathBuf {
    crate::config::default_base_dir()
}

/// 构建迁移计划（干跑用；也可直接 [`MigrationPlan::apply`]）。
///
/// - `auto=false`：`migrate-namespace` 场景——只做 ID 重写，不做 6.6 归档。
/// - 幂等：数据已符合 `to_namespace` 形态 → 空 plan。
pub fn build_plan(
    workspace_dir: &Path,
    base_dir: &Path,
    to_namespace: &str,
    auto: bool,
) -> Result<MigrationPlan> {
    let mut plan = MigrationPlan::default();
    // 6.1 → 6.2 同事务：root FQID 由 6.1 决定，6.2 复用。
    let root_fqid = migrate_users(&mut plan, base_dir, to_namespace)?;
    migrate_resolver(&mut plan, base_dir, to_namespace, root_fqid.as_deref())?;
    // 6.5 先于 6.3：`active.json` 的 `_cron_<jobid>` 键需要 job old→new 映射。
    let job_map = migrate_jobs(&mut plan, workspace_dir, to_namespace)?;
    migrate_sessions(&mut plan, workspace_dir, to_namespace, &job_map)?;
    migrate_tasks(&mut plan, workspace_dir, to_namespace)?;
    if auto {
        archive_legacy_user_dirs(&mut plan, workspace_dir, to_namespace)?;
        migrate_session_meta_task_ids(&mut plan, workspace_dir)?;
    }
    Ok(plan)
}

/// 启动自动迁移（RFC §6.1–6.6）。幂等：数据已符合目标形态 → no-op。
pub fn run_auto(workspace_dir: &Path, base_dir: &Path, namespace: &str) -> Result<MigrationReport> {
    let plan = build_plan(workspace_dir, base_dir, namespace, true)?;
    if plan.is_empty() {
        tracing::debug!("migration: 数据已符合目标形态，无需迁移");
        return Ok(MigrationReport {
            backups: 0,
            steps: 0,
            migrated: false,
        });
    }
    plan.apply()?;
    tracing::info!(
        backups = plan.backups.len(),
        steps = plan.steps.len(),
        "migration: 启动自动迁移完成"
    );
    Ok(MigrationReport {
        backups: plan.backups.len(),
        steps: plan.steps.len(),
        migrated: true,
    })
}
