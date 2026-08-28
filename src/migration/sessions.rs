use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::ids::{dir_name, Fqid, TYPE_SESSION, TYPE_TASK};
use super::types::{Backup, MigrationPlan, Step};
use super::users::{is_8hex, is_fqid_any_ns, rewrite_fqid_ns};

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
pub(super) fn migrate_sessions(
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
pub(super) fn migrate_tasks(plan: &mut MigrationPlan, workspace: &Path, to_ns: &str) -> Result<()> {
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

// ── B 清理：session meta 遗留 task_id ───────────────────────────────────────

/// 清理 `sessions/*/meta.json` 中遗留的顶层 `task_id` 字段。
///
/// 寻址面迁移 B 后，sub-agent 的持久标识是 session id（`delegations/` 主键、
/// suspension pending 匹配均按 sub_session_id），`task_id` 字段已无消费方。
/// 数据卫生：仅自动模式（daemon 启动、组件加载前）执行；幂等——无 `task_id`
/// → 无动作。备份策略与 6.3 一致（`.migration-backups/<dir>/meta.json.bak`），
/// 重跑/崩溃恢复保留首轮原始备份。
pub(super) fn migrate_session_meta_task_ids(plan: &mut MigrationPlan, workspace: &Path) -> Result<()> {
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
