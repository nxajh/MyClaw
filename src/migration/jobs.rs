use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::ids::{dir_name, Fqid, TYPE_JOB};
use super::types::{Backup, MigrationPlan, Step};
use super::users::{is_fqid_any_ns, rewrite_fqid_ns};

// ── 6.5 cron/jobs.json（先于 6.3，供 `_cron_` 键改名）────────────────────────

/// 6.5 `cron/jobs.json`：遗留 job id → `<ns>/job/<uuidv7>`；`run_logs` 文件
/// 同步改名（dir_name 转义）。返回 job old→new 映射（6.3 `_cron_` 键用）。
pub(super) fn migrate_jobs(
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
pub(super) fn job_id_by_name(root: &serde_json::Value) -> HashMap<String, String> {
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
pub(super) fn restore_job_map_from_bak(
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
