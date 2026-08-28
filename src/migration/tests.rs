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
