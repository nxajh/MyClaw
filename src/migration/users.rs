use std::path::Path;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::ids::{Fqid, TYPE_USER};
use super::types::{Backup, MigrationPlan, Step};

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
pub(super) fn migrate_users(plan: &mut MigrationPlan, base_dir: &Path, to_ns: &str) -> Result<Option<String>> {
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
pub(super) fn migrate_resolver(
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

// ── 6.6 users/ 遗留 rk 目录归档（仅自动）────────────────────────────────────

/// 6.6 `users/` 遗留 rk 目录 → `.legacy-rk-archive/`（启动自动；不动内容，
/// 不归档新布局根目录 `<ns>/`）。数据考古非迁移必需——仅挪走死数据。
pub(super) fn archive_legacy_user_dirs(plan: &mut MigrationPlan, workspace: &Path, to_ns: &str) -> Result<()> {
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

// ── 判定/归一化 helper ───────────────────────────────────────────────────────

/// 是否 `<ns>/<type>/<uuid>` 形态（任意 ns、已知类型段）。
pub(super) fn is_fqid_any_ns(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() == 3 && crate::ids::is_known_type(parts[1]) && Uuid::parse_str(parts[2]).is_ok()
}

/// 是否双重前缀 user.id（`ns/u/ns/u/<uuid>` 及更深嵌套）。
pub(super) fn is_double_prefixed(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() >= 5
        && parts.len() % 2 == 1
        && parts[1] == TYPE_USER
        && parts[parts.len() - 2] == TYPE_USER
        && Uuid::parse_str(parts[parts.len() - 1]).is_ok()
}

/// FQID 的 namespace 段改写（`<ns>/<type>/<uuid>` → `<to_ns>/<type>/<uuid>`）。
/// 非 FQID 或类型不符 → None。
pub(super) fn rewrite_fqid_ns(s: &str, type_seg: &str, to_ns: &str) -> Option<String> {
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
pub(super) fn normalize_user_id(v: &str, to_ns: &str, root_fqid: Option<&str>) -> Option<String> {
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
pub(super) fn is_8hex(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}
