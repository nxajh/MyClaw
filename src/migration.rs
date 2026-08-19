//! RFC §6 数据迁移引擎（identity-id-rfc §6）。
//!
//! 启动自动迁移（[`run_auto`]）收敛 5 项格式 + 2 项清理：
//! - 6.1 `users.json`：v1 孤儿 `root` 条目 → uuidv7 FQID（Case A 原位升级 /
//!   Case B 丢弃孤儿、保留 `username=root` 的 FQID 条目），key/uid 归一化
//!   （含双重前缀污染值）；
//! - 6.2 `user_resolver.json`：覆盖值双重前缀 / 旧 `root` 段 → 规范 user.id
//!   （与 6.1 同事务，root FQID 用 6.1 结果）；
//! - 6.3 `sessions/`：8-hex 目录 → `<ns>/s/<uuidv7>`（目录重命名 + `meta.json`
//!   `id` 重写 + `active.json` session_id 值重写 + `_cron_<jobid>` 键改名）；
//!   备份策略：逐目录 `meta.json.bak` + `active.json.bak` + old→new manifest
//!   （不做全量目录拷贝——重命名本身原子，meta 备份覆盖全部变更面）；
//! - 6.4 `.state/tasks.json`：`task_{n}` → `<ns>/t/<uuidv7>`，`parent_id` 映射
//!   表重写；
//! - 6.5 `cron/jobs.json`：遗留 job id → `<ns>/job/<uuidv7>`，`run_logs` 文件
//!   同步改名，`active.json` 的 `_cron_` 键连带（键内含 job id，不 rename 则
//!   cron 会话历史断链）；
//! - 6.6 `users/` 遗留 rk 目录 → `users/.legacy-rk-archive/`（仅自动模式；
//!   不动内容，不归档新布局根目录）；
//! - B 清理：`sessions/*/meta.json` 遗留顶层 `task_id` 字段（寻址面迁移 B：
//!   agent 寻址从 task_id 迁到 session id 后的数据卫生；仅自动模式，备份
//!   策略同 6.3：`.migration-backups/<dir>/meta.json.bak`）。
//!
//! 手动命令 `myclaw migrate-namespace <new>`（RFC §6.7）复用同一 builder：
//! 目标 namespace 参数化，流程 备份 → 干跑 → 确认 → 执行。
//!
//! 设计：plan-based。[`build_plan`] 只读数据、产出备份 + 步骤清单（幂等——
//! 数据已符合目标形态则 plan 为空）；[`MigrationPlan::apply`] 先备份后执行。
//! JSON 一律经 `serde_json::Value` 改写（未知字段原样保留，避免结构漂移丢字段）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::ids::{dir_name, Fqid, TYPE_JOB, TYPE_SESSION, TYPE_TASK, TYPE_USER};

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

// ── 6.1 users.json ───────────────────────────────────────────────────────────

/// 6.1 `users.json` → version 2（uuidv7 FQID + username）。
///
/// 实际数据已部分演进（daemon P4-3 `migrate_legacy_to_root` 跑过），须收敛
/// 「已部分迁移」状态而非假设 RFC 的原始 v1：
/// - Case A（原始 v1）：`{root: {uid:"root", username:""}}` 单条目 → 原位升级：
///   生成 uuidv7 FQID 作 key+uid，补 `username="root"`；
/// - Case B（当前数据）：孤儿 `root` 条目（uid="root", username=""）与新 FQID
///   条目（username="root"）并存 → 丢弃孤儿、保留 FQID 条目；
/// - 双重前缀 uid（`ns/u/ns/u/<uuid>`，user_id() 旧 bug 污染）归一化为单层 FQID。
///
/// 返回 root 的规范 FQID（6.2 复用；无 root 时 None）。
fn migrate_users(plan: &mut MigrationPlan, base_dir: &Path, to_ns: &str) -> Result<Option<String>> {
    let path = base_dir.join("users.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("migration: 读 {} 失败", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("migration: 解析 {} 失败", path.display()))?;
    let Some(users) = root.get_mut("users").and_then(|v| v.as_object_mut()) else {
        return Ok(None);
    };

    // 既有 FQID 条目中的 root（Case B 判断用）。
    let existing_root: Option<String> = users.iter().find_map(|(_, v)| {
        let username = v.get("username").and_then(|s| s.as_str()).unwrap_or("");
        let uid = v.get("uid").and_then(|s| s.as_str()).unwrap_or("");
        (username == "root" && is_fqid_any_ns(uid)).then(|| uid.to_string())
    });
    let mut root_fqid = existing_root.clone();

    let old_users = std::mem::take(users);
    let mut next = serde_json::Map::new();
    let mut changed = false;
    let mut dropped = 0usize;
    for (key, mut user) in old_users {
        let uid = user
            .get("uid")
            .and_then(|s| s.as_str())
            .unwrap_or(key.as_str())
            .to_string();
        if is_fqid_any_ns(&uid) || is_double_prefixed(&uid) {
            // FQID 条目（可能双重前缀 / 错误 ns）→ 归一化。
            let norm = normalize_user_id(&uid, to_ns, None).unwrap_or(uid.clone());
            let uid_changed = user.get("uid").and_then(|s| s.as_str()) != Some(norm.as_str());
            let key_changed = key != norm;
            if uid_changed || key_changed {
                if uid_changed {
                    user["uid"] = serde_json::Value::String(norm.clone());
                }
                changed = true;
            }
            if user.get("username").and_then(|s| s.as_str()) == Some("root") {
                root_fqid = Some(norm.clone());
            }
            next.insert(norm, user);
        } else if uid == "root" || key == "root" {
            // 孤儿 root 条目。
            if root_fqid.is_some() {
                // Case B：已有 username=root 的 FQID 条目 → 丢弃孤儿。
                dropped += 1;
                changed = true;
                continue;
            }
            // Case A：唯一 root 条目 → 原位升级。
            let fqid = Fqid::new(to_ns, TYPE_USER).to_string();
            user["uid"] = serde_json::Value::String(fqid.clone());
            user["username"] = serde_json::Value::String("root".to_string());
            root_fqid = Some(fqid.clone());
            next.insert(fqid, user);
            changed = true;
        } else {
            // 其他遗留形态（非 root、非 FQID）——不丢数据，原样保留。
            next.insert(key, user);
        }
    }
    if !changed {
        return Ok(root_fqid);
    }
    root["version"] = serde_json::Value::from(2u32);
    root["users"] = serde_json::Value::Object(next);
    let body = serde_json::to_string_pretty(&root)?;
    plan.backups.push(Backup {
        from: path.clone(),
        to: path.with_extension("json.v1.bak"),
        label: "users.json → users.json.v1.bak".to_string(),
    });
    let label = if dropped > 0 {
        format!("users.json：丢弃 {dropped} 个孤儿 root 条目，key/uid 归一到 {to_ns}/u/<uuidv7>")
    } else {
        format!("users.json：key/uid 归一到 {to_ns}/u/<uuidv7>（version 2）")
    };
    plan.steps.push(Step::WriteJson { path, body, label });
    Ok(root_fqid)
}

// ── 6.2 user_resolver.json ───────────────────────────────────────────────────

/// 6.2 `user_resolver.json`：覆盖值 → 规范 user.id（与 6.1 同事务；root 段
/// 用 6.1 结果改写）。当前数据 7 条覆盖值全部为双重前缀污染值。
fn migrate_resolver(
    plan: &mut MigrationPlan,
    base_dir: &Path,
    to_ns: &str,
    root_fqid: Option<&str>,
) -> Result<()> {
    let path = crate::config::user_resolver_path(base_dir);
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("migration: 读 {} 失败", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("migration: 解析 {} 失败", path.display()))?;
    let Some(overrides) = root.get_mut("overrides").and_then(|v| v.as_object_mut()) else {
        return Ok(());
    };
    let old = std::mem::take(overrides);
    let mut next = serde_json::Map::new();
    let mut changed = 0usize;
    for (k, v) in old {
        let val = v.as_str().unwrap_or_default();
        match normalize_user_id(val, to_ns, root_fqid) {
            Some(nv) if nv != val => {
                next.insert(k, serde_json::Value::String(nv));
                changed += 1;
            }
            _ => {
                next.insert(k, v);
            }
        }
    }
    *overrides = next;
    if changed == 0 {
        return Ok(());
    }
    let body = serde_json::to_string_pretty(&root)?;
    plan.backups.push(Backup {
        from: path.clone(),
        to: path.with_extension("json.v1.bak"),
        label: "user_resolver.json → user_resolver.json.v1.bak".to_string(),
    });
    plan.steps.push(Step::WriteJson {
        path,
        body,
        label: format!("user_resolver.json：{changed} 条覆盖值归一到 {to_ns}/u/<uuidv7>"),
    });
    Ok(())
}

// ── 6.5 cron/jobs.json（先于 6.3，供 `_cron_` 键改名）────────────────────────

/// 6.5 `cron/jobs.json`：遗留 job id → `<ns>/job/<uuidv7>`；`run_logs` 文件
/// 同步改名（dir_name 转义）。返回 job old→new 映射（6.3 `_cron_` 键用）。
fn migrate_jobs(
    plan: &mut MigrationPlan,
    workspace: &Path,
    to_ns: &str,
) -> Result<HashMap<String, String>> {
    let cron_dir = workspace.join("cron");
    let path = cron_dir.join("jobs.json");
    let mut job_map = HashMap::new();
    if !path.exists() {
        return Ok(job_map);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("migration: 读 {} 失败", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("migration: 解析 {} 失败", path.display()))?;
    let Some(jobs) = root.get_mut("jobs").and_then(|v| v.as_array_mut()) else {
        return Ok(job_map);
    };
    let mut changed = false;
    for job in jobs.iter_mut() {
        let Some(id) = job
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let new_id = if is_fqid_any_ns(&id) {
            // FQID：namespace 段改写（migrate-namespace 场景）。
            rewrite_fqid_ns(&id, TYPE_JOB, to_ns)
        } else {
            // 遗留短 id（12-hex 等）→ 新 FQID。
            Some(Fqid::new(to_ns, TYPE_JOB).to_string())
        };
        if let Some(new_id) = new_id {
            if new_id != id {
                job["id"] = serde_json::Value::String(new_id.clone());
                job_map.insert(id.clone(), new_id);
                changed = true;
            }
        }
    }
    if !changed {
        // jobs.json 已迁移（全 FQID）：active.json 的 `_cron_<old>` 键仍需
        // old→new 映射——从备份恢复（按 name 配对），不重复写 jobs.json。
        return Ok(restore_job_map_from_bak(&cron_dir, &root));
    }
    let body = serde_json::to_string_pretty(&root)?;
    plan.backups.push(Backup {
        from: path.clone(),
        to: path.with_extension("json.bak"),
        label: "cron/jobs.json → jobs.json.bak".to_string(),
    });
    for (old, new) in &job_map {
        let old_log = cron_dir.join("run_logs").join(format!("{}.jsonl", dir_name(old)));
        let new_log = cron_dir.join("run_logs").join(format!("{}.jsonl", dir_name(new)));
        if old_log.exists() {
            plan.steps.push(Step::RenameFile {
                from: old_log,
                to: new_log,
                label: format!("run_logs/{old}.jsonl → run_logs/{}.jsonl", dir_name(new)),
            });
        }
    }
    plan.steps.push(Step::WriteJson {
        path,
        body,
        label: format!("cron/jobs.json：{} 条 job id → {to_ns}/job/<uuidv7>", job_map.len()),
    });
    Ok(job_map)
}

/// 按 `name` 收集 jobs 的 id（配对用）。
fn job_id_by_name(root: &serde_json::Value) -> HashMap<String, String> {
    root.get("jobs")
        .and_then(|v| v.as_array())
        .map(|jobs| {
            jobs.iter()
                .filter_map(|j| {
                    Some((
                        j.get("name")?.as_str()?.to_owned(),
                        j.get("id")?.as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// jobs.json 已迁移（全 FQID）时，从 `jobs.json.bak`（首次备份的原始数据）
/// 恢复 old id → new id 映射（按 job `name` 配对），供 `active.json` 的
/// `_cron_<jobid>` 键改名。备份缺失或无法配对 → 空 map。
fn restore_job_map_from_bak(
    cron_dir: &Path,
    current: &serde_json::Value,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bak_path = cron_dir.join("jobs.json.bak");
    let Ok(raw) = std::fs::read_to_string(&bak_path) else {
        tracing::warn!("migration: jobs.json 已迁移但 {} 不存在，无法恢复 _cron_ 键映射", bak_path.display());
        return map;
    };
    let Ok(bak) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return map;
    };
    let old_by_name = job_id_by_name(&bak);
    let new_by_name = job_id_by_name(current);
    for (name, old_id) in old_by_name {
        if let Some(new_id) = new_by_name.get(&name) {
            if new_id != &old_id {
                map.insert(old_id, new_id.clone());
            }
        }
    }
    map
}

// ── 6.3 sessions/ ────────────────────────────────────────────────────────────

/// 6.3 `sessions/`：8-hex 目录 → `<ns>/s/<uuidv7>`。
///
/// 每个目录：`meta.json.bak` 备份 → 重写 `id`（+ `parent_session_id` 重映射）
/// → 目录重命名（dir_name 转义）。`active.json`：备份 + 全部 session_id 值
/// 重写（owner 键不动；`_cron_<jobid>` 键按 job_map 改名——键内含 job id，
/// 不改则 cron 会话历史断链）。最后写 `.migration-manifest.json`（old→new
/// 审计）。不做全量目录拷贝（磁盘 85% 占用下 662M 拷贝过重；重命名原子，
/// meta 备份覆盖全部变更面，history/archive/files 内容零变更）。
///
/// 只迁 8-hex 目录：跳过 rk 目录与 `active.json` 文件本身。
fn migrate_sessions(
    plan: &mut MigrationPlan,
    workspace: &Path,
    to_ns: &str,
    job_map: &HashMap<String, String>,
) -> Result<()> {
    let sessions_root = workspace.join("sessions");
    if !sessions_root.exists() {
        return Ok(());
    }
    let mut hex_dirs: Vec<(PathBuf, String)> = Vec::new();
    for entry in std::fs::read_dir(&sessions_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type()?.is_dir() && is_8hex(&name) {
            hex_dirs.push((entry.path(), name));
        }
    }
    if hex_dirs.is_empty() {
        return Ok(());
    }

    // old 8-hex → new FQID（一次生成，同批一致）。若 meta.id 已是 FQID
    // （上次运行写过 meta 但 rename 未发生的半途状态）→ 复用，不重新生成。
    let mut session_map: HashMap<String, String> = HashMap::new();
    for (dir, old) in &hex_dirs {
        let reuse = std::fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_owned))
            .filter(|id| is_fqid_any_ns(id));
        session_map.insert(
            old.clone(),
            reuse.unwrap_or_else(|| Fqid::new(to_ns, TYPE_SESSION).to_string()),
        );
    }

    for (dir, old) in &hex_dirs {
        let new = &session_map[old];
        let meta_path = dir.join("meta.json");
        if meta_path.exists() {
            // 可写探测：owner 异常/只读目录（如 nobody 遗留）无法重写 meta →
            // warn + 跳过该目录（保持原样 8-hex，数据不丢，等有权限时重跑收敛）。
            if std::fs::OpenOptions::new()
                .write(true)
                .open(&meta_path)
                .is_err()
            {
                tracing::warn!(
                    "migration: {old}/meta.json 不可写（owner/权限异常），跳过该会话目录的迁移"
                );
                continue;
            }
            // 备份到独立目录（不放在将被 RenameDir 的 hex 目录内——否则备份
            // 文件会随目录移动，且重跑时 from 路径已不存在导致 copy 失败）。
            let backup_to = sessions_root
                .join(".migration-backups")
                .join(old)
                .join("meta.json.bak");
            plan.backups.push(Backup {
                from: meta_path.clone(),
                to: backup_to,
                label: format!("{old}/meta.json → .migration-backups/{old}/meta.json.bak"),
            });
            let raw = std::fs::read_to_string(&meta_path)
                .with_context(|| format!("migration: 读 {} 失败", meta_path.display()))?;
            let mut meta: serde_json::Value = serde_json::from_str(&raw)
                .with_context(|| format!("migration: 解析 {} 失败", meta_path.display()))?;
            meta["id"] = serde_json::Value::String(new.clone());
            if let Some(parent) = meta.get("parent_session_id").and_then(|v| v.as_str()) {
                if let Some(nv) = session_map.get(parent) {
                    meta["parent_session_id"] = serde_json::Value::String(nv.clone());
                }
            }
            plan.steps.push(Step::WriteJson {
                path: meta_path,
                body: serde_json::to_string_pretty(&meta)?,
                label: format!("{old}/meta.json：id → {new}"),
            });
        }
        let new_dir = sessions_root.join(dir_name(new));
        plan.steps.push(Step::RenameDir {
            from: dir.clone(),
            to: new_dir.clone(),
            label: format!("sessions/{old} → sessions/{}", dir_name(new)),
        });
    }

    // active.json：值（session_id）全部重映射；`_cron_<jobid>` 键改名。
    let active_path = sessions_root.join("active.json");
    if active_path.exists() {
        let raw = std::fs::read_to_string(&active_path)
            .with_context(|| format!("migration: 读 {} 失败", active_path.display()))?;
        let mut active: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("migration: 解析 {} 失败", active_path.display()))?;
        let Some(map) = active.as_object_mut() else {
            return Ok(());
        };
        let old_map = std::mem::take(map);
        let mut next = serde_json::Map::new();
        let mut changed = false;
        for (k, v) in old_map {
            let mut nk = k.clone();
            if let Some(rest) = k.strip_prefix("_cron_") {
                if let Some(new_job) = job_map.get(rest) {
                    nk = format!("_cron_{new_job}");
                }
            }
            let mut drop = false;
            let nv = match v.as_str().and_then(|sid| session_map.get(sid)) {
                // 本次映射内的 8-hex 旧 id → 新 FQID。
                Some(new_sid) => {
                    changed = true;
                    serde_json::Value::String(new_sid.clone())
                }
                None => {
                    // 不在本次映射：8-hex 值且目录已不存在（已迁/已删/历史残留）
                    // → 死键丢弃；目录仍在（未迁/跳过的目录）→ 保留原值。
                    if let Some(sid) = v.as_str() {
                        if is_8hex(sid) && !sessions_root.join(sid).exists() {
                            drop = true;
                            changed = true;
                        }
                    }
                    v.clone()
                }
            };
            if nk != k {
                changed = true;
            }
            if !drop {
                next.insert(nk, nv);
            }
        }
        *map = next;
        if changed {
            plan.backups.push(Backup {
                from: active_path.clone(),
                to: active_path.with_extension("json.bak"),
                label: "sessions/active.json → active.json.bak".to_string(),
            });
            plan.steps.push(Step::WriteJson {
                path: active_path,
                body: serde_json::to_string_pretty(&active)?,
                label: format!("sessions/active.json：{} 个 session_id 值重写", session_map.len()),
            });
        }
    }

    // 审计 manifest：old → new（供人工核对 / 回滚参考）。
    let manifest = serde_json::json!({ "version": 1, "sessions": session_map });
    plan.steps.push(Step::WriteJson {
        path: sessions_root.join(".migration-manifest.json"),
        body: serde_json::to_string_pretty(&manifest)?,
        label: "sessions/.migration-manifest.json（old→new 审计）".to_string(),
    });
    Ok(())
}

// ── 6.4 .state/tasks.json ────────────────────────────────────────────────────

/// 6.4 `.state/tasks.json`：`task_{n}` → `<ns>/t/<uuidv7>`；`parent_id` 经
/// 映射表重写。`next_id` 字段退役（序列化本就跳过 namespace/save_path，
/// body 只有 `tasks`）。
fn migrate_tasks(plan: &mut MigrationPlan, workspace: &Path, to_ns: &str) -> Result<()> {
    let path = workspace.join(".state").join("tasks.json");
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("migration: 读 {} 失败", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("migration: 解析 {} 失败", path.display()))?;
    let Some(tasks) = root.get_mut("tasks").and_then(|v| v.as_array_mut()) else {
        return Ok(());
    };

    // 第一遍：old → new 映射（遗留短 id 生成 FQID；FQID 改写 ns 段）。
    let mut map: HashMap<String, String> = HashMap::new();
    for task in tasks.iter() {
        let Some(id) = task.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let new_id = if is_fqid_any_ns(id) {
            rewrite_fqid_ns(id, TYPE_TASK, to_ns).unwrap_or_else(|| id.to_string())
        } else {
            Fqid::new(to_ns, TYPE_TASK).to_string()
        };
        if new_id != id {
            map.insert(id.to_string(), new_id);
        }
    }
    if map.is_empty() {
        return Ok(());
    }

    // 第二遍：应用映射（id + parent_id）。
    for task in tasks.iter_mut() {
        if let Some(id) = task.get("id").and_then(|v| v.as_str()) {
            if let Some(new) = map.get(id) {
                task["id"] = serde_json::Value::String(new.clone());
            }
        }
        if let Some(pid) = task.get("parent_id").and_then(|v| v.as_str()) {
            if let Some(new) = map.get(pid) {
                task["parent_id"] = serde_json::Value::String(new.clone());
            }
        }
    }
    let body = serde_json::to_string_pretty(&root)?;
    plan.backups.push(Backup {
        from: path.clone(),
        to: path.with_extension("json.bak"),
        label: ".state/tasks.json → tasks.json.bak".to_string(),
    });
    plan.steps.push(Step::WriteJson {
        path,
        body,
        label: format!(".state/tasks.json：{} 条 task id → {to_ns}/t/<uuidv7>", map.len()),
    });
    Ok(())
}

// ── 6.6 users/ 遗留 rk 目录归档（仅自动）────────────────────────────────────

/// 6.6 `users/` 遗留 rk 目录 → `.legacy-rk-archive/`（启动自动；不动内容，
/// 不归档新布局根目录 `<ns>/`）。数据考古非迁移必需——仅挪走死数据。
fn archive_legacy_user_dirs(plan: &mut MigrationPlan, workspace: &Path, to_ns: &str) -> Result<()> {
    let users_root = workspace.join("users");
    if !users_root.exists() {
        return Ok(());
    }
    let archive = users_root.join(".legacy-rk-archive");
    for entry in std::fs::read_dir(&users_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if name == ".legacy-rk-archive" || name == to_ns {
            continue;
        }
        plan.steps.push(Step::MoveDir {
            from: entry.path(),
            to: archive.join(&name),
            label: format!("users/{name} → users/.legacy-rk-archive/{name}"),
        });
    }
    Ok(())
}

// ── B 清理：session meta 遗留 task_id ───────────────────────────────────────

/// 清理 `sessions/*/meta.json` 中遗留的顶层 `task_id` 字段。
///
/// 寻址面迁移 B 后，sub-agent 的持久标识是 session id（`delegations/` 主键、
/// suspension pending 匹配均按 sub_session_id），`task_id` 字段已无消费方。
/// 数据卫生：仅自动模式（daemon 启动、组件加载前）执行；幂等——无 `task_id`
/// → 无动作。备份策略与 6.3 一致（`.migration-backups/<dir>/meta.json.bak`），
/// 重跑/崩溃恢复保留首轮原始备份。
fn migrate_session_meta_task_ids(plan: &mut MigrationPlan, workspace: &Path) -> Result<()> {
    let sessions_root = workspace.join("sessions");
    if !sessions_root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&sessions_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let meta_path = dir.join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        // 可写探测（与 migrate_sessions 一致）：owner 异常/只读目录无法重写 →
        // warn + 跳过（数据不丢，等有权限时重跑收敛）。
        if std::fs::OpenOptions::new().write(true).open(&meta_path).is_err() {
            tracing::warn!(
                "migration: {}/meta.json 不可写（owner/权限异常），跳过 task_id 清理",
                dir.file_name().unwrap_or_default().to_string_lossy()
            );
            continue;
        }
        let raw = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("migration: 读 {} 失败", meta_path.display()))?;
        let mut meta: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("migration: 解析 {} 失败", meta_path.display()))?;
        let has_task_id = meta
            .as_object()
            .map(|o| o.contains_key("task_id"))
            .unwrap_or(false);
        if !has_task_id {
            continue;
        }
        if let Some(obj) = meta.as_object_mut() {
            obj.remove("task_id");
        }
        let dir_name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        plan.backups.push(Backup {
            from: meta_path.clone(),
            to: sessions_root
                .join(".migration-backups")
                .join(&dir_name)
                .join("meta.json.bak"),
            label: format!("{dir_name}/meta.json → .migration-backups/{dir_name}/meta.json.bak"),
        });
        plan.steps.push(Step::WriteJson {
            path: meta_path,
            body: serde_json::to_string(&meta)?,
            label: format!("{dir_name}/meta.json：删除遗留 task_id 字段"),
        });
    }
    Ok(())
}

// ── 判定/归一化 helper ───────────────────────────────────────────────────────

/// 是否 `<ns>/<type>/<uuid>` 形态（任意 ns、已知类型段）。
fn is_fqid_any_ns(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() == 3 && crate::ids::is_known_type(parts[1]) && Uuid::parse_str(parts[2]).is_ok()
}

/// 是否双重前缀 user.id（`ns/u/ns/u/<uuid>` 及更深嵌套）。
fn is_double_prefixed(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() >= 5
        && parts.len() % 2 == 1
        && parts[1] == TYPE_USER
        && parts[parts.len() - 2] == TYPE_USER
        && Uuid::parse_str(parts[parts.len() - 1]).is_ok()
}

/// FQID 的 namespace 段改写（`<ns>/<type>/<uuid>` → `<to_ns>/<type>/<uuid>`）。
/// 非 FQID 或类型不符 → None。
fn rewrite_fqid_ns(s: &str, type_seg: &str, to_ns: &str) -> Option<String> {
    let parts: Vec<&str> = s.split('/').collect();
    match parts.as_slice() {
        [ns, ty, uuid] if *ty == type_seg && Uuid::parse_str(uuid).is_ok() => {
            if *ns != to_ns {
                Some(format!("{to_ns}/{type_seg}/{uuid}"))
            } else {
                Some(s.to_string())
            }
        }
        _ => None,
    }
}

/// 归一化 user.id 值：剥双重前缀 → 旧 `root` 段映射 → namespace 段改写。
/// 无法识别（非 u 形态）→ None（调用方保持原样）。
fn normalize_user_id(v: &str, to_ns: &str, root_fqid: Option<&str>) -> Option<String> {
    // 1. 剥双重前缀（`ns/u/ns/u/<uuid>` → `ns/u/<uuid>`，循环到不能再剥）。
    let mut cur = v.to_string();
    loop {
        let parts: Vec<&str> = cur.split('/').collect();
        if parts.len() >= 5
            && parts.len() % 2 == 1
            && parts[1] == TYPE_USER
            && (2..parts.len() - 1)
                .step_by(2)
                .all(|i| parts[i] == parts[0] && parts[i + 1] == TYPE_USER)
            && Uuid::parse_str(parts[parts.len() - 1]).is_ok()
        {
            cur = format!("{}/{}/{}", parts[0], parts[1], parts[parts.len() - 1]);
            continue;
        }
        break;
    }
    let parts: Vec<&str> = cur.split('/').collect();
    match parts.as_slice() {
        // 2. 旧 `ns/u/root` → root 规范 FQID（6.1 同事务提供）。
        [ns, ty, name] if *ty == TYPE_USER && *name == "root" => {
            if let Some(r) = root_fqid {
                return Some(r.to_string());
            }
            if *ns != to_ns {
                return Some(format!("{to_ns}/u/root"));
            }
            Some(cur)
        }
        // 3. FQID → namespace 段改写。
        [ns, ty, uuid] if *ty == TYPE_USER && Uuid::parse_str(uuid).is_ok() => {
            if *ns != to_ns {
                Some(format!("{to_ns}/u/{uuid}"))
            } else {
                Some(cur)
            }
        }
        _ => None,
    }
}

/// 是否 8-hex 遗留 session 目录名（32 位随机短 id）。
fn is_8hex(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::id_from_dir;
    use std::fs;

    fn write_json(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    // ── 归一化 helper ───────────────────────────────────────────────────────

    #[test]
    fn normalize_strips_double_prefix() {
        let v = "myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de";
        assert_eq!(
            normalize_user_id(v, "myclaw", None).unwrap(),
            "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
        // 三重前缀
        let t = "myclaw/u/myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de";
        assert_eq!(
            normalize_user_id(t, "myclaw", None).unwrap(),
            "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
    }

    #[test]
    fn normalize_rewrites_namespace() {
        assert_eq!(
            normalize_user_id("brand/u/019fe342-6a03-7561-86de-0c2327a8c3de", "myclaw", None)
                .unwrap(),
            "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
        // 双重前缀 + ns 改写
        assert_eq!(
            normalize_user_id("brand/u/brand/u/019fe342-6a03-7561-86de-0c2327a8c3de", "myclaw", None)
                .unwrap(),
            "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
    }

    #[test]
    fn normalize_maps_legacy_root_segment() {
        assert_eq!(
            normalize_user_id("myclaw/u/root", "myclaw", Some("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"))
                .unwrap(),
            "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
        assert_eq!(
            normalize_user_id("brand/u/root", "myclaw", None).unwrap(),
            "myclaw/u/root"
        );
    }

    #[test]
    fn normalize_leaves_unknown_alone() {
        assert_eq!(normalize_user_id("telegram:myclaw:6270938644", "myclaw", None), None);
        assert_eq!(normalize_user_id("myclaw/s/019fe342-6a03-7561-86de-0c2327a8c3de", "myclaw", None), None);
    }

    // ── 6.1 users.json ──────────────────────────────────────────────────────

    #[test]
    fn users_case_a_upgrades_orphan_root_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("users.json"),
            r#"{"version":1,"users":{"root":{"uid":"root","username":"","active":true,"created_ms":100}}}"#,
        );
        let mut plan = MigrationPlan::default();
        let root_fqid = migrate_users(&mut plan, tmp.path(), "myclaw").unwrap();
        let fqid = root_fqid.expect("root fqid");
        assert!(fqid.starts_with("myclaw/u/"), "fqid={fqid}");
        plan.apply().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("users.json")).unwrap()).unwrap();
        assert_eq!(v["version"], 2);
        let users = v["users"].as_object().unwrap();
        assert_eq!(users.len(), 1);
        let u = &users[&fqid];
        assert_eq!(u["uid"], fqid.as_str());
        assert_eq!(u["username"], "root");
        // 备份存在
        assert!(tmp.path().join("users.json.v1.bak").exists());
    }

    #[test]
    fn users_case_b_drops_orphan_keeps_fqid() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("users.json"),
            r#"{"version":2,"users":{
                "root":{"uid":"root","username":"","active":true,"created_ms":100},
                "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de":{"uid":"myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de","username":"root","active":true,"created_ms":200}
            }}"#,
        );
        let mut plan = MigrationPlan::default();
        let root_fqid = migrate_users(&mut plan, tmp.path(), "myclaw").unwrap();
        assert_eq!(
            root_fqid.as_deref(),
            Some("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de")
        );
        plan.apply().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("users.json")).unwrap()).unwrap();
        let users = v["users"].as_object().unwrap();
        assert_eq!(users.len(), 1);
        assert!(users.contains_key("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"));
        assert_eq!(users["myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"]["username"], "root");
    }

    #[test]
    fn users_double_prefixed_uid_normalized() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("users.json"),
            r#"{"version":2,"users":{
                "myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de":{"uid":"myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de","username":"root","active":true,"created_ms":200}
            }}"#,
        );
        let mut plan = MigrationPlan::default();
        let root_fqid = migrate_users(&mut plan, tmp.path(), "myclaw").unwrap();
        assert_eq!(
            root_fqid.as_deref(),
            Some("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de")
        );
        plan.apply().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("users.json")).unwrap()).unwrap();
        let users = v["users"].as_object().unwrap();
        assert!(users.contains_key("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"));
        assert!(!users.contains_key("myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"));
    }

    #[test]
    fn users_already_migrated_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("users.json"),
            r#"{"version":2,"users":{
                "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de":{"uid":"myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de","username":"root","active":true,"created_ms":200}
            }}"#,
        );
        let mut plan = MigrationPlan::default();
        let root_fqid = migrate_users(&mut plan, tmp.path(), "myclaw").unwrap();
        assert_eq!(
            root_fqid.as_deref(),
            Some("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de")
        );
        assert!(plan.backups.is_empty() && plan.steps.is_empty(), "应为 no-op");
    }

    // ── 6.2 resolver ────────────────────────────────────────────────────────

    #[test]
    fn resolver_normalizes_double_prefixed_values() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("user_resolver.json"),
            r#"{"version":1,"overrides":{
                "wechat:default:x@im.wechat":"myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de",
                "qqbot:xiaoliu:ABC":"myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
            }}"#,
        );
        let mut plan = MigrationPlan::default();
        migrate_resolver(&mut plan, tmp.path(), "myclaw", Some("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"))
            .unwrap();
        plan.apply().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("user_resolver.json")).unwrap())
                .unwrap();
        for (_, val) in v["overrides"].as_object().unwrap() {
            assert_eq!(val, "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de");
        }
        assert!(tmp.path().join("user_resolver.json.v1.bak").exists());
    }

    #[test]
    fn resolver_maps_legacy_root_value() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("user_resolver.json"),
            r#"{"version":1,"overrides":{"wechat:default:x@im.wechat":"myclaw/u/root"}}"#,
        );
        let mut plan = MigrationPlan::default();
        migrate_resolver(&mut plan, tmp.path(), "myclaw", Some("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"))
            .unwrap();
        plan.apply().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("user_resolver.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["overrides"]["wechat:default:x@im.wechat"],
            "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
    }

    // ── 6.5 jobs ────────────────────────────────────────────────────────────

    #[test]
    fn jobs_rewrites_legacy_id_and_run_log() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("cron/jobs.json"),
            r#"{"jobs":[{"id":"07fcb1d780eb","schedule":"0 0 10 * * 5","prompt":"p","target":"wechat"}]}"#,
        );
        write_json(
            &tmp.path().join("cron/run_logs/07fcb1d780eb.jsonl"),
            "line1\n",
        );
        let mut plan = MigrationPlan::default();
        let job_map = migrate_jobs(&mut plan, tmp.path(), "myclaw").unwrap();
        assert_eq!(job_map.len(), 1);
        let (old, new) = job_map.iter().next().unwrap();
        assert_eq!(old, "07fcb1d780eb");
        assert!(new.starts_with("myclaw/job/"), "new={new}");
        plan.apply().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("cron/jobs.json")).unwrap())
                .unwrap();
        assert_eq!(v["jobs"][0]["id"], new.as_str());
        // run_logs 改名 + jobs.json.bak
        assert!(tmp.path().join("cron/run_logs").join(format!("{}.jsonl", dir_name(new))).exists());
        assert!(!tmp.path().join("cron/run_logs/07fcb1d780eb.jsonl").exists());
        assert!(tmp.path().join("cron/jobs.json.bak").exists());
    }

    // ── 6.3 sessions ────────────────────────────────────────────────────────

    #[test]
    fn sessions_rename_dirs_rewrite_meta_and_active() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(sessions.join("aabbccdd")).unwrap();
        fs::create_dir_all(sessions.join("00112233")).unwrap();
        write_json(
            &sessions.join("aabbccdd/meta.json"),
            r#"{"id":"aabbccdd","owner":"wechat:default:x@im.wechat","created_at":"2026-05-14T17:38:06Z","message_count":1}"#,
        );
        write_json(
            &sessions.join("00112233/meta.json"),
            r#"{"id":"00112233","owner":"qqbot:xiaoer:ABC","created_at":"2026-05-14T17:38:06Z","message_count":2,"parent_session_id":"aabbccdd"}"#,
        );
        write_json(
            &sessions.join("active.json"),
            r#"{"wechat:default:x@im.wechat":"aabbccdd","qqbot:xiaoer:ABC":"00112233","_cron_07fcb1d780eb":"aabbccdd","_heartbeat_8c153d9b-3c66-47d7-9cf0-346cdcfa80e9":"00112233"}"#,
        );
        let mut job_map = HashMap::new();
        job_map.insert("07fcb1d780eb".to_string(), "myclaw/job/019fe342-6a03-7561-86de-0c2327a8c3de".to_string());
        let mut plan = MigrationPlan::default();
        migrate_sessions(&mut plan, tmp.path(), "myclaw", &job_map).unwrap();
        plan.apply().unwrap();

        // 目录重命名 + meta.id 重写
        let mut new_ids: Vec<String> = plan
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::RenameDir { to, .. } => Some(
                    to.file_name().unwrap().to_string_lossy().to_string(),
                ),
                _ => None,
            })
            .collect();
        new_ids.sort();
        assert_eq!(new_ids.len(), 2);
        for n in &new_ids {
            assert!(n.starts_with("myclaw_s_"), "dir={n}");
            let meta: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(sessions.join(n).join("meta.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(meta["id"].as_str().unwrap(), &id_from_dir(n));
            // parent_session_id 重映射
            if meta["owner"] == "qqbot:xiaoer:ABC" {
                let parent = meta["parent_session_id"].as_str().unwrap().to_string();
                assert!(parent.starts_with("myclaw/s/"), "parent={parent}");
                let parent_dir = dir_name(&parent);
                assert!(sessions.join(&parent_dir).exists());
            }
        }
        // active.json：值重写 + `_cron_` 键改名 + heartbeat 键不动
        let active: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(sessions.join("active.json")).unwrap(),
        )
        .unwrap();
        let map = active.as_object().unwrap();
        assert!(map.contains_key("_cron_myclaw/job/019fe342-6a03-7561-86de-0c2327a8c3de"));
        assert!(map.contains_key("_heartbeat_8c153d9b-3c66-47d7-9cf0-346cdcfa80e9"));
        assert!(!map.contains_key("_cron_07fcb1d780eb"));
        for (_, v) in map {
            let sid = v.as_str().unwrap();
            assert!(sid.starts_with("myclaw/s/"), "value={sid}");
        }
        // 备份 + manifest
        assert!(sessions.join("active.json.bak").exists());
        assert!(sessions.join(".migration-manifest.json").exists());
        assert!(sessions.join(".migration-backups/aabbccdd/meta.json.bak").exists());
        assert!(sessions.join(".migration-backups/00112233/meta.json.bak").exists());
        assert!(!sessions.join("aabbccdd").exists());
    }

    #[test]
    fn sessions_skips_non_hex_dirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(sessions.join("qqbot:xiaoer:ABC")).unwrap();
        write_json(&sessions.join("active.json"), r#"{}"#);
        let mut plan = MigrationPlan::default();
        migrate_sessions(&mut plan, tmp.path(), "myclaw", &HashMap::new()).unwrap();
        assert!(plan.is_empty(), "非 8-hex 目录不应触发迁移");
    }

    // ── 6.4 tasks ───────────────────────────────────────────────────────────

    #[test]
    fn tasks_rewrite_ids_and_parents() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join(".state/tasks.json"),
            r#"{"tasks":[
                {"id":"task_15","parent_id":null,"subject":"parent","description":"","status":"in_progress","created_at":"2026-08-08T17:34:07Z"},
                {"id":"task_16","parent_id":"task_15","subject":"child","description":"","status":"completed","created_at":"2026-08-08T17:34:38Z"}
            ]}"#,
        );
        let mut plan = MigrationPlan::default();
        migrate_tasks(&mut plan, tmp.path(), "myclaw").unwrap();
        plan.apply().unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join(".state/tasks.json")).unwrap(),
        )
        .unwrap();
        let tasks = v["tasks"].as_array().unwrap();
        let parent = tasks.iter().find(|t| t["subject"] == "parent").unwrap();
        let child = tasks.iter().find(|t| t["subject"] == "child").unwrap();
        assert!(parent["id"].as_str().unwrap().starts_with("myclaw/t/"));
        assert_eq!(child["parent_id"], parent["id"]);
        assert!(tmp.path().join(".state/tasks.json.bak").exists());
    }

    #[test]
    fn tasks_noop_when_already_fqid() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join(".state/tasks.json"),
            r#"{"tasks":[{"id":"myclaw/t/019fe342-6a03-7561-86de-0c2327a8c3de","parent_id":null,"subject":"s","description":"","status":"pending","created_at":"2026-08-08T17:34:07Z"}]}"#,
        );
        let mut plan = MigrationPlan::default();
        migrate_tasks(&mut plan, tmp.path(), "myclaw").unwrap();
        assert!(plan.is_empty());
    }

    // ── 6.6 archive ─────────────────────────────────────────────────────────

    #[test]
    fn archive_moves_rk_dirs_keeps_namespace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let users = tmp.path().join("users");
        fs::create_dir_all(users.join("qqbot:xiaoer:ABC")).unwrap();
        fs::create_dir_all(users.join("telegram_myclaw_6270938644")).unwrap();
        fs::create_dir_all(users.join("myclaw/u/root")).unwrap();
        let mut plan = MigrationPlan::default();
        archive_legacy_user_dirs(&mut plan, tmp.path(), "myclaw").unwrap();
        plan.apply().unwrap();
        assert!(users.join(".legacy-rk-archive/qqbot:xiaoer:ABC").exists());
        assert!(users.join(".legacy-rk-archive/telegram_myclaw_6270938644").exists());
        assert!(!users.join("qqbot:xiaoer:ABC").exists());
        assert!(users.join("myclaw/u/root").exists(), "新布局根目录不归档");
    }

    // ── 端到端：build_plan 幂等 ─────────────────────────────────────────────

    #[test]
    fn build_plan_applies_then_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        // 模拟当前真实数据状态：users v2（孤儿+双重前缀）→ resolver 双重前缀 → sessions
        // 8-hex → tasks task_{n} → jobs 遗留 id。
        write_json(
            &tmp.path().join("users.json"),
            r#"{"version":2,"users":{
                "root":{"uid":"root","username":"","active":true,"created_ms":100},
                "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de":{"uid":"myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de","username":"root","active":true,"created_ms":200}
            }}"#,
        );
        write_json(
            &tmp.path().join("user_resolver.json"),
            r#"{"version":1,"overrides":{"wechat:default:x@im.wechat":"myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"}}"#,
        );
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(sessions.join("aabbccdd")).unwrap();
        write_json(
            &sessions.join("aabbccdd/meta.json"),
            r#"{"id":"aabbccdd","owner":"wechat:default:x@im.wechat","created_at":"2026-05-14T17:38:06Z","message_count":1}"#,
        );
        write_json(&sessions.join("active.json"), r#"{"wechat:default:x@im.wechat":"aabbccdd","_cron_07fcb1d780eb":"aabbccdd"}"#);
        write_json(
            &tmp.path().join("cron/jobs.json"),
            r#"{"jobs":[{"id":"07fcb1d780eb","schedule":"0 0 10 * * 5","prompt":"p","target":"wechat"}]}"#,
        );
        write_json(&tmp.path().join("cron/run_logs/07fcb1d780eb.jsonl"), "x\n");
        write_json(
            &tmp.path().join(".state/tasks.json"),
            r#"{"tasks":[{"id":"task_15","parent_id":null,"subject":"s","description":"","status":"pending","created_at":"2026-08-08T17:34:07Z"}]}"#,
        );
        fs::create_dir_all(tmp.path().join("users/qqbot:xiaoer:ABC")).unwrap();
        fs::create_dir_all(tmp.path().join("users/myclaw/u/root")).unwrap();

        let plan = build_plan(tmp.path(), tmp.path(), "myclaw", true).unwrap();
        assert!(!plan.is_empty());
        plan.apply().unwrap();

        // 再次构建 → 空 plan（幂等）。
        let plan2 = build_plan(tmp.path(), tmp.path(), "myclaw", true).unwrap();
        assert!(plan2.is_empty(), "重跑应为 no-op，实际 {} 备份 {} 步骤", plan2.backups.len(), plan2.steps.len());
    }

    #[test]
    fn migrate_namespace_rewrites_fqid_ns() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("users.json"),
            r#"{"version":2,"users":{
                "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de":{"uid":"myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de","username":"root","active":true,"created_ms":200}
            }}"#,
        );
        write_json(
            &tmp.path().join("user_resolver.json"),
            r#"{"version":1,"overrides":{"wechat:default:x@im.wechat":"myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"}}"#,
        );
        write_json(
            &tmp.path().join(".state/tasks.json"),
            r#"{"tasks":[{"id":"myclaw/t/019fe342-6a03-7561-86de-0c2327a8c3de","parent_id":null,"subject":"s","description":"","status":"pending","created_at":"2026-08-08T17:34:07Z"}]}"#,
        );
        let plan = build_plan(tmp.path(), tmp.path(), "brand", false).unwrap();
        assert!(!plan.is_empty());
        plan.apply().unwrap();
        let users: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join("users.json")).unwrap(),
        )
        .unwrap();
        assert!(users["users"].as_object().unwrap().contains_key("brand/u/019fe342-6a03-7561-86de-0c2327a8c3de"));
        let resolver: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join("user_resolver.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            resolver["overrides"]["wechat:default:x@im.wechat"],
            "brand/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
        let tasks: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join(".state/tasks.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(tasks["tasks"][0]["id"], "brand/t/019fe342-6a03-7561-86de-0c2327a8c3de");
    }

    // ── 部署回归（真实故障修复）────────────────────────────────────────────

    /// jobs.json 已迁移（全 FQID）时，仍须从 jobs.json.bak 恢复 old→new 映射
    /// （供 active.json 的 `_cron_<jobid>` 键改名），且不重复写 jobs.json。
    #[test]
    fn jobs_restores_map_from_bak_when_already_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("cron/jobs.json"),
            r#"{"jobs":[{"id":"myclaw/job/019fe342-6a03-7561-86de-0c2327a8c3de","name":"weekly","schedule":"0 0 10 * * 5","prompt":"p","target":"wechat"}]}"#,
        );
        write_json(
            &tmp.path().join("cron/jobs.json.bak"),
            r#"{"jobs":[{"id":"07fcb1d780eb","name":"weekly","schedule":"0 0 10 * * 5","prompt":"p","target":"wechat"}]}"#,
        );
        let mut plan = MigrationPlan::default();
        let job_map = migrate_jobs(&mut plan, tmp.path(), "myclaw").unwrap();
        assert_eq!(
            job_map.get("07fcb1d780eb").map(String::as_str),
            Some("myclaw/job/019fe342-6a03-7561-86de-0c2327a8c3de")
        );
        // 已迁移 → 无 jobs 写步骤（不重复写）。
        assert!(
            plan.steps.iter().all(|s| !s.label().contains("jobs.json")),
            "已迁移不应再写 jobs.json，实际步骤: {:?}",
            plan.steps.iter().map(|s| s.label()).collect::<Vec<_>>()
        );
    }

    /// active.json：映射外的 8-hex 值且目录已不存在（已迁/已删）→ 死键丢弃；
    /// 目录仍在的保留原值；FQID 值原样保留。
    #[test]
    fn sessions_drops_dead_active_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(sessions.join("aabbccdd")).unwrap();
        write_json(
            &sessions.join("aabbccdd/meta.json"),
            r#"{"id":"aabbccdd","owner":"wechat:default:x@im.wechat","created_at":"2026-05-14T17:38:06Z","message_count":1}"#,
        );
        write_json(
            &sessions.join("active.json"),
            r#"{
                "wechat:default:x@im.wechat":"aabbccdd",
                "dead:key:deadbeef":"deadbeef",
                "alive:key:alive":"myclaw/s/019fe342-6a03-7561-86de-0c2327a8c3de"
            }"#,
        );
        let mut plan = MigrationPlan::default();
        migrate_sessions(&mut plan, tmp.path(), "myclaw", &HashMap::new()).unwrap();
        plan.apply().unwrap();
        let active: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(sessions.join("active.json")).unwrap(),
        )
        .unwrap();
        let map = active.as_object().unwrap();
        assert!(!map.contains_key("dead:key:deadbeef"), "死键应被丢弃");
        assert!(map.contains_key("alive:key:alive"));
        assert_eq!(
            map["alive:key:alive"],
            "myclaw/s/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
        // aabbccdd 目录存在 → 本次映射内 → 重写为 FQID。
        let v = map["wechat:default:x@im.wechat"].as_str().unwrap();
        assert!(v.starts_with("myclaw/s/"), "value={v}");
    }

    /// 不可写目录（owner 异常/只读）→ 跳过该会话目录迁移（不阻塞全局），
    /// 其余目录照迁。root 下权限位无效 → 跳过测试。
    #[test]
    fn sessions_skips_unwritable_dir() {
        fn running_as_root() -> bool {
            std::fs::read_to_string("/proc/self/status")
                .map(|s| {
                    s.lines()
                        .find(|l| l.starts_with("Uid:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        == Some("0")
                })
                .unwrap_or(false)
        }
        if running_as_root() {
            eprintln!("skip: root 无视权限位，无法构造不可写目录");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(sessions.join("aabbccdd")).unwrap();
        fs::create_dir_all(sessions.join("00112233")).unwrap();
        write_json(
            &sessions.join("aabbccdd/meta.json"),
            r#"{"id":"aabbccdd","owner":"wechat:default:x@im.wechat","created_at":"2026-05-14T17:38:06Z","message_count":1}"#,
        );
        write_json(
            &sessions.join("00112233/meta.json"),
            r#"{"id":"00112233","owner":"qqbot:xiaoer:ABC","created_at":"2026-05-14T17:38:06Z","message_count":2}"#,
        );
        // aabbccdd 只读（nobody 遗留模拟）。
        let mut perms = fs::metadata(sessions.join("aabbccdd/meta.json")).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o444);
        fs::set_permissions(sessions.join("aabbccdd/meta.json"), perms).unwrap();

        let mut plan = MigrationPlan::default();
        migrate_sessions(&mut plan, tmp.path(), "myclaw", &HashMap::new()).unwrap();
        plan.apply().unwrap();
        // 只读目录未被迁移（原样保留 8-hex，无 rename 步骤）。
        assert!(sessions.join("aabbccdd").exists(), "只读目录应原样保留");
        assert!(
            !plan.steps.iter().any(|s| s.label().starts_with("sessions/aabbccdd")),
            "只读目录不应有迁移步骤: {:?}",
            plan.steps.iter().map(|s| s.label()).collect::<Vec<_>>()
        );
        // 可写目录正常迁移。
        assert!(!sessions.join("00112233").exists(), "可写目录应已 rename");
    }

    // ── B 清理：session meta task_id ────────────────────────────────────────

    #[test]
    fn session_meta_task_id_cleanup_removes_field_and_backs_up() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let dir = "myclaw_s_019fec31-ed7d-7791-92e9-6822b5053031";
        fs::create_dir_all(sessions.join(dir)).unwrap();
        write_json(
            &sessions.join(dir).join("meta.json"),
            r#"{"id":"myclaw/s/019fec31-ed7d-7791-92e9-6822b5053031","owner":"telegram:myclaw:6270938644","created_at":"2026-08-10T15:02:02Z","message_count":8,"parent_session_id":"myclaw/s/019fe564-1566-7453-b9b0-89c5d707fa93","task_id":"myclaw/t/019fec31-ed1f-7032-b666-ff67bd0c10c4","segments":[{"segment":0,"start_id":1,"count":8}]}"#,
        );
        // auto=true（daemon 启动路径）→ 清理步骤 + 备份。
        let plan = build_plan(tmp.path(), tmp.path(), "myclaw", true).unwrap();
        assert!(!plan.is_empty(), "带 task_id 的 meta 应触发清理");
        plan.apply().unwrap();
        let meta: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(sessions.join(dir).join("meta.json")).unwrap(),
        )
        .unwrap();
        assert!(meta.get("task_id").is_none(), "task_id 应已删除");
        assert_eq!(
            meta["id"].as_str().unwrap(),
            "myclaw/s/019fec31-ed7d-7791-92e9-6822b5053031",
            "其余字段原样保留"
        );
        assert_eq!(meta["message_count"].as_u64().unwrap(), 8);
        assert!(sessions
            .join(".migration-backups")
            .join(dir)
            .join("meta.json.bak")
            .exists());
        // 幂等：再次构建 → 空 plan。
        let plan2 = build_plan(tmp.path(), tmp.path(), "myclaw", true).unwrap();
        assert!(
            plan2.is_empty(),
            "重跑应为 no-op，实际 {} 备份 {} 步骤",
            plan2.backups.len(),
            plan2.steps.len()
        );
    }

    #[test]
    fn session_meta_task_id_cleanup_only_auto_and_only_dirty_files() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let clean = "myclaw_s_019fee03-bd90-7353-bf91-65300eabdb85";
        let dirty = "myclaw_s_019fee03-f18f-70c2-96fa-f97d109dd34f";
        fs::create_dir_all(sessions.join(clean)).unwrap();
        write_json(
            &sessions.join(clean).join("meta.json"),
            r#"{"id":"myclaw/s/019fee03-bd90-7353-bf91-65300eabdb85","owner":"x","created_at":"2026-08-11T00:00:00Z","message_count":1}"#,
        );
        fs::create_dir_all(sessions.join(dirty)).unwrap();
        write_json(
            &sessions.join(dirty).join("meta.json"),
            r#"{"id":"myclaw/s/019fee03-f18f-70c2-96fa-f97d109dd34f","owner":"x","created_at":"2026-08-11T00:00:00Z","message_count":2,"task_id":"myclaw/t/019fee03-ed1f-7032-b666-ff67bd0c10c4"}"#,
        );
        // auto=false（migrate-namespace CLI 路径）→ 不产生 task_id 清理。
        let plan = build_plan(tmp.path(), tmp.path(), "brand", false).unwrap();
        assert!(
            !plan.steps.iter().any(|s| s.label().contains("task_id")),
            "auto=false 不应清理 task_id: {:?}",
            plan.steps.iter().map(|s| s.label()).collect::<Vec<_>>()
        );
        // auto=true → 只清理带 task_id 的文件，干净文件不动。
        let plan = build_plan(tmp.path(), tmp.path(), "myclaw", true).unwrap();
        let labels: Vec<&str> = plan.steps.iter().map(|s| s.label()).collect();
        assert!(
            labels.iter().any(|l| l.contains(dirty)),
            "应清理带 task_id 的 meta: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l.contains(clean)),
            "无 task_id 的 meta 不应有步骤: {labels:?}"
        );
        // 备份仅覆盖被清理的文件。
        plan.apply().unwrap();
        assert!(sessions
            .join(".migration-backups")
            .join(dirty)
            .join("meta.json.bak")
            .exists());
        assert!(!sessions.join(".migration-backups").join(clean).exists());
    }
}
