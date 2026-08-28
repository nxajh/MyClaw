use super::*;
use crate::scheduling_types::job_types::parse_hhmm;
/// 解环) — its only remaining caller was this test module.
/// Minimal channel mock for webhook dispatch tests (#151 Phase 3d:
pub(crate) fn test_scheduler(dir: &std::path::Path) -> SharedScheduler {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    Scheduler::new(
        dir.join("jobs"),
        "test",
        "UTC".to_string(),
        None,
        tx,
        dir.join("last_channel"),
        dir.join("last_recipient"),
    )
}

pub(crate) fn test_entry(schedule: &str) -> JobEntry {
    JobEntry {
        id: String::new(),
        schedule: Some(ScheduleSpec::cron(schedule)),
        webhook: None,
        prompt: "p".to_string(),
        name: Some("test-job".to_string()),
        tz: None,
        active_hours: None,
        enabled: true,
        last_run_at: None,
        next_run_at: None,
        created_at: None,
        delivery: DeliveryConfig::default(),
        last_runs: Vec::new(),
        enabled_tools: None,
        disabled_tools: None,
        retry: None,
        failure_alert: None,
        consecutive_errors: 0,
        consecutive_skipped: 0,
        max_runs: None,
        completed_runs: 0,
        delete_after_run: false,
        model: None,
        provider: None,
        last_failure_alert_at: None,
        context_policy: crate::config::scheduler::ContextPolicy::Inject,
    }
}

#[test]
fn trigger_now_forces_next_run_due() {
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    let id = sched.add_job(test_entry("0 0 10 * * 5")).unwrap();
    let before = sched
        .jobs()
        .iter()
        .find(|j| j.id == id)
        .unwrap()
        .next_run_at
        .clone();
    assert!(before.is_some());

    sched
        .update_job(&id, JobUpdate { trigger_now: true, ..Default::default() })
        .unwrap();

    let after = sched.jobs().iter().find(|j| j.id == id).unwrap().next_run_at.clone();
    let after = chrono::DateTime::parse_from_rfc3339(&after.unwrap()).unwrap();
    assert!(after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[test]
fn trigger_now_rejected_for_paused_job() {
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    let id = sched.add_job(test_entry("0 0 10 * * 5")).unwrap();
    sched.set_enabled(&id, false).unwrap();
    assert!(sched
        .update_job(&id, JobUpdate { trigger_now: true, ..Default::default() })
        .is_err());
}

#[test]
fn parse_hours_valid() {
    assert_eq!(parse_hhmm("08:00"), Some(480));
    assert_eq!(parse_hhmm("24:00"), Some(1440));
    assert_eq!(parse_hhmm("13:30"), Some(810));
}

#[test]
fn parse_interval_minutes() {
    assert_eq!(parse_interval("30m"), Some(Duration::from_secs(30 * 60)));
    assert_eq!(parse_interval("5m"), Some(Duration::from_secs(5 * 60)));
}

#[test]
fn parse_interval_hours() {
    assert_eq!(parse_interval("1h"), Some(Duration::from_secs(3600)));
    assert_eq!(parse_interval("2h"), Some(Duration::from_secs(7200)));
}

#[test]
fn parse_interval_seconds() {
    assert_eq!(parse_interval("30s"), Some(Duration::from_secs(30)));
}

#[test]
fn parse_interval_zero_disables() {
    assert_eq!(parse_interval("0"), None);
}

#[test]
fn parse_interval_invalid() {
    assert_eq!(parse_interval("abc"), None);
}


#[test]
fn is_active_hours_no_restriction() {
    assert!(is_active_hours(&None, "Asia/Shanghai"));
}

#[test]
fn is_active_hours_invalid_format_always_active() {
    assert!(is_active_hours(&Some("bad".to_string()), "Asia/Shanghai"));
}

#[test]
fn test_normalize_weekday_unix() {
    assert_eq!(normalize_weekday_unix("0 0 9 * * 0"), "0 0 9 * * 1"); // Sun
    assert_eq!(normalize_weekday_unix("0 0 9 * * 1"), "0 0 9 * * 2"); // Mon
    assert_eq!(normalize_weekday_unix("0 0 9 * * 6"), "0 0 9 * * 7"); // Sat
    assert_eq!(normalize_weekday_unix("0 0 9 * * 7"), "0 0 9 * * 1"); // Sun
    assert_eq!(normalize_weekday_unix("0 0 9 * * 1-5"), "0 0 9 * * 2-6"); // Mon-Fri
    assert_eq!(normalize_weekday_unix("0 0 9 * * 0,6"), "0 0 9 * * 1,7"); // Sun,Sat
    assert_eq!(normalize_weekday_unix("0 0 9 * * */2"), "0 0 9 * * */2"); // step unchanged
    assert_eq!(normalize_weekday_unix("0 0 9 * * 1-5/2"), "0 0 9 * * 2-6/2");
    assert_eq!(normalize_weekday_unix("0 0 9 * * MON-FRI"), "0 0 9 * * MON-FRI");
    assert_eq!(normalize_weekday_unix("0 0 9 * * MON,5"), "0 0 9 * * MON,6");
    assert_eq!(normalize_weekday_unix("not a cron"), "not a cron");
}

#[test]
fn validate_schedule_valid() {
    assert!(validate_schedule("0 0 9 * * *").is_ok());
    assert!(validate_schedule("0 */30 * * * *").is_ok());
}

#[test]
fn validate_schedule_invalid() {
    assert!(validate_schedule("not a cron").is_err());
}

#[test]
fn validate_tz_valid() {
    assert!(validate_tz("Asia/Shanghai").is_ok());
    assert!(validate_tz("UTC").is_ok());
}

#[test]
fn validate_tz_invalid() {
    assert!(validate_tz("Invalid/Zone").is_err());
}

#[test]
fn validate_active_hours_valid() {
    assert!(validate_active_hours("08:00-24:00").is_ok());
    assert!(validate_active_hours("00:00-23:59").is_ok());
}

#[test]
fn validate_active_hours_invalid_format() {
    assert!(validate_active_hours("bad").is_err());
    assert!(validate_active_hours("25:00-26:00").is_err());
}

#[test]
fn validate_active_hours_start_ge_end() {
    assert!(validate_active_hours("18:00-08:00").is_err());
    assert!(validate_active_hours("12:00-12:00").is_err());
}

// ── P1-B2 directory-based job storage ─────────────────────────────────

#[test]
fn jobs_store_directory_load_wins_over_legacy_file() {
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");

    // Directory store: one job at {id}/meta.json
    let id_a = "test/job/019fe342aaaa";
    let meta_dir = jobs_root.join(crate::ids::dir_name(id_a));
    std::fs::create_dir_all(&meta_dir).unwrap();
    let entry_a = serde_json::to_string(&test_entry("0 9 * * *")).unwrap();
    let mut parsed: serde_json::Value = serde_json::from_str(&entry_a).unwrap();
    parsed["id"] = serde_json::json!(id_a);
    std::fs::write(meta_dir.join("meta.json"), parsed.to_string()).unwrap();

    // Legacy single file also present with a DIFFERENT job.
    let id_legacy = "test/job/019fe342bbbb";
    let mut legacy_entry = serde_json::to_value(test_entry("0 8 * * *")).unwrap();
    legacy_entry["id"] = serde_json::json!(id_legacy);
    std::fs::create_dir_all(&jobs_root).unwrap();
    std::fs::write(
        jobs_root.join("jobs.json"),
        serde_json::to_string(&JobsFile {
            jobs: vec![serde_json::from_value(legacy_entry).unwrap()],
        })
        .unwrap(),
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root,
        "test",
        "UTC".to_string(),
        None,
        tx,
        dir.path().join("lc"),
        dir.path().join("lr"),
    );
    let jobs = sched.jobs();
    // Directory store wins entirely; legacy job is ignored.
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, id_a);
}

#[test]
fn jobs_store_legacy_file_fallback_when_dir_empty() {
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    std::fs::create_dir_all(&jobs_root).unwrap();

    let id_legacy = "test/job/019fe342cccc";
    let mut legacy_entry = serde_json::to_value(test_entry("0 8 * * *")).unwrap();
    legacy_entry["id"] = serde_json::json!(id_legacy);
    std::fs::write(
        jobs_root.join("jobs.json"),
        serde_json::to_string(&JobsFile {
            jobs: vec![serde_json::from_value(legacy_entry).unwrap()],
        })
        .unwrap(),
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root,
        "test",
        "UTC".to_string(),
        None,
        tx,
        dir.path().join("lc"),
        dir.path().join("lr"),
    );
    let jobs = sched.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, id_legacy);
}

#[test]
fn jobs_store_writes_per_job_meta_and_prunes_delete() {
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let sched = test_scheduler(dir.path());

    // Add a job → meta.json appears under {id}/meta.json.
    let entry = test_entry("0 9 * * *");
    let id = sched.add_job(entry.clone()).unwrap();
    let meta = jobs_root
        .join(crate::ids::dir_name(&id))
        .join("meta.json");
    assert!(meta.is_file());
    // meta.json round-trips the full JobEntry.
    let on_disk: JobEntry =
        serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
    assert_eq!(on_disk.id, id);
    assert!(matches!(
        on_disk.schedule,
        Some(ScheduleSpec::Kind(ScheduleKind::Cron { ref expr })) if expr == "0 9 * * *"
    ));
    // The legacy single file must NOT be (re)created.
    assert!(!jobs_root.join("jobs.json").exists());

    // Remove → meta.json (and the whole per-job dir) gone.
    assert!(sched.remove_job(&id).unwrap().is_some());
    assert!(!meta.exists());
}

#[test]
fn remove_job_deletes_run_logs_dir_and_returns_audit() {
    // #77: remove_job used to leave the per-job directory (meta.json +
    // run_logs/) behind forever. It must now be gone, and the caller
    // must get back enough to report what was deleted.
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let sched = test_scheduler(dir.path());

    let mut entry = test_entry("0 9 * * *");
    entry.name = Some("probe".to_string());
    let id = sched.add_job(entry).unwrap();
    let job_dir = jobs_root.join(crate::ids::dir_name(&id));

    sched.append_run_log_inner(
        &id,
        &RunRecord {
            run_at: "2026-08-20T13:00:00Z".to_string(),
            status: RunStatus::Ok,
            output_preview: "first run ok".to_string(),
            ..Default::default()
        },
    );
    sched.append_run_log_inner(
        &id,
        &RunRecord {
            run_at: "2026-08-20T13:05:00Z".to_string(),
            status: RunStatus::Error,
            output_preview: "second run failed".to_string(),
            ..Default::default()
        },
    );
    assert!(job_dir.join("run_logs").is_dir());

    let audit = sched.remove_job(&id).unwrap().expect("job was present");
    assert_eq!(audit.name, "probe");
    assert_eq!(audit.run_log_count, 2);
    assert_eq!(audit.last_status, Some("error"));
    assert_eq!(audit.last_output_preview.as_deref(), Some("second run failed"));

    // The whole per-job directory is gone — not just meta.json.
    assert!(!job_dir.exists());

    // Removing again reports "not found", not a stale audit.
    assert!(sched.remove_job(&id).unwrap().is_none());
}

#[test]
fn drain_auto_delete_deletes_job_dir_too() {
    // #77: the auto-delete path (max_runs + delete_after_run) had the
    // same directory-leak bug as explicit remove.
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let sched = test_scheduler(dir.path());

    let mut entry = test_entry("0 9 * * *");
    entry.name = Some("one-shot-probe".to_string());
    entry.max_runs = Some(1);
    entry.completed_runs = 1;
    entry.delete_after_run = true;
    let id = sched.add_job(entry).unwrap();
    let job_dir = jobs_root.join(crate::ids::dir_name(&id));
    sched.append_run_log_inner(
        &id,
        &RunRecord {
            run_at: "2026-08-20T13:04:39Z".to_string(),
            status: RunStatus::Ok,
            output_preview: "done".to_string(),
            ..Default::default()
        },
    );
    assert!(job_dir.is_dir());

    let deleted = sched.drain_auto_delete();
    assert_eq!(deleted, vec![id]);
    assert!(!job_dir.exists());
}

// ── Orthogonal trigger model (§3.4) tests ─────────────────────────

// ── §3.4 schedule_kind convergence (legacy → polymorphic object) ──────

#[test]
fn legacy_string_schedule_folds_to_cron_object() {
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let id = "test/job/019fe4ceaaaa";
    let meta_dir = jobs_root.join(crate::ids::dir_name(id));
    std::fs::create_dir_all(&meta_dir).unwrap();
    // Pre-convergence shape: bare string + null discriminator.
    std::fs::write(
        meta_dir.join("meta.json"),
        r#"{"id": "test/job/019fe4ceaaaa", "name": "legacy-cron", "schedule": "0 9 * * *",
            "prompt": "p", "target": "last", "schedule_kind": null}"#,
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root.clone(), "test", "UTC".to_string(), None, tx,
        dir.path().join("lc"), dir.path().join("lr"),
    );
    let jobs = sched.jobs();
    assert!(matches!(
        jobs[0].schedule,
        Some(ScheduleSpec::Kind(ScheduleKind::Cron { ref expr })) if expr == "0 9 * * *"
    ));
    // The fold happens at load; an empty update forces a save so the
    // canonical object hits the disk.
    sched.update_job(id, JobUpdate::default()).unwrap();
    // Persisted back as the canonical object (no bare string, no
    // schedule_kind key).
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            jobs_root.join(crate::ids::dir_name(id)).join("meta.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(saved["schedule"]["kind"], "cron");
    assert_eq!(saved["schedule"]["expr"], "0 9 * * *");
    assert!(saved.get("schedule_kind").is_none());
}

// ── #78 target/delivery convergence ───────────────────────────────

#[test]
fn parse_target_string_covers_last_none_and_fixed() {
    assert_eq!(parse_target_string("last").mode, DeliveryMode::Last);
    assert_eq!(parse_target_string("").mode, DeliveryMode::Last);
    assert_eq!(parse_target_string("none").mode, DeliveryMode::None);

    let fixed = parse_target_string("wechat");
    assert_eq!(fixed.mode, DeliveryMode::Fixed);
    assert_eq!(fixed.channel.as_deref(), Some("wechat"));
    assert_eq!(fixed.account_id, None);

    let fixed_acct = parse_target_string("telegram:work");
    assert_eq!(fixed_acct.mode, DeliveryMode::Fixed);
    assert_eq!(fixed_acct.channel.as_deref(), Some("telegram"));
    assert_eq!(fixed_acct.account_id.as_deref(), Some("work"));
}

#[test]
fn legacy_target_and_delivery_fold_into_unified_delivery() {
    // #78: a pre-migration meta.json with `target: "wechat"` and the
    // old dead-field `delivery: {channel, to}` (channel was never
    // actually read — target alone decided routing) must fold into a
    // single `delivery` with `to` preserved (the one old field that
    // *was* live) and `channel` sourced from `target` (the one that
    // actually took effect), matching the issue's "6 active jobs, all
    // losslessly mappable" claim.
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let id = "test/job/019fe4cecccc";
    let meta_dir = jobs_root.join(crate::ids::dir_name(id));
    std::fs::create_dir_all(&meta_dir).unwrap();
    std::fs::write(
        meta_dir.join("meta.json"),
        r#"{"id": "test/job/019fe4cecccc", "name": "wechat-probe",
            "schedule": {"kind": "cron", "expr": "0 9 * * *"}, "prompt": "p",
            "target": "wechat", "delivery": {"channel": "ignored-was-dead", "to": "user-42"}}"#,
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root.clone(), "test", "UTC".to_string(), None, tx,
        dir.path().join("lc"), dir.path().join("lr"),
    );
    let jobs = sched.jobs();
    assert_eq!(jobs[0].delivery.mode, DeliveryMode::Fixed);
    assert_eq!(jobs[0].delivery.channel.as_deref(), Some("wechat"));
    assert_eq!(jobs[0].delivery.to.as_deref(), Some("user-42"));
}

#[test]
fn already_migrated_delivery_is_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let id = "test/job/019fe4cedddd";
    let meta_dir = jobs_root.join(crate::ids::dir_name(id));
    std::fs::create_dir_all(&meta_dir).unwrap();
    // Has both a (stale) `target` and an already-migrated `delivery`
    // with `mode` — `delivery` must win, `target` must be ignored.
    std::fs::write(
        meta_dir.join("meta.json"),
        r#"{"id": "test/job/019fe4cedddd", "name": "already-new",
            "schedule": {"kind": "cron", "expr": "0 9 * * *"}, "prompt": "p",
            "target": "none",
            "delivery": {"mode": "fixed", "channel": "discord", "to": "chan-1"}}"#,
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root.clone(), "test", "UTC".to_string(), None, tx,
        dir.path().join("lc"), dir.path().join("lr"),
    );
    let jobs = sched.jobs();
    assert_eq!(jobs[0].delivery.mode, DeliveryMode::Fixed);
    assert_eq!(jobs[0].delivery.channel.as_deref(), Some("discord"));
}

#[test]
fn missing_target_and_delivery_defaults_to_last() {
    // No legacy `target`, no `delivery` at all — serde default kicks
    // in (DeliveryMode::Last), matching the old implicit default.
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let id = "test/job/019fe4ceeeee";
    let meta_dir = jobs_root.join(crate::ids::dir_name(id));
    std::fs::create_dir_all(&meta_dir).unwrap();
    std::fs::write(
        meta_dir.join("meta.json"),
        r#"{"id": "test/job/019fe4ceeeee", "name": "bare",
            "schedule": {"kind": "cron", "expr": "0 9 * * *"}, "prompt": "p"}"#,
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root.clone(), "test", "UTC".to_string(), None, tx,
        dir.path().join("lc"), dir.path().join("lr"),
    );
    assert_eq!(sched.jobs()[0].delivery.mode, DeliveryMode::Last);
}

#[tokio::test]
async fn resolve_delivery_none_mode_skips() {
    let dir = tempfile::tempdir().unwrap();
    let sched = test_scheduler(dir.path());
    let cfg = DeliveryConfig {
        mode: DeliveryMode::None,
        ..Default::default()
    };
    assert!(sched.resolve_delivery(&cfg).await.is_none());
}

#[tokio::test]
async fn resolve_delivery_fixed_mode_uses_explicit_channel() {
    let dir = tempfile::tempdir().unwrap();
    let sched = test_scheduler(dir.path());
    let cfg = DeliveryConfig {
        mode: DeliveryMode::Fixed,
        channel: Some("wechat".to_string()),
        to: Some("user-1".to_string()),
        ..Default::default()
    };
    let (ch, acc, recipient) = sched.resolve_delivery(&cfg).await.unwrap();
    assert_eq!(ch, "wechat");
    assert_eq!(acc, "default");
    assert_eq!(recipient.as_deref(), Some("user-1"));
}

#[tokio::test]
async fn resolve_delivery_fixed_mode_without_channel_is_misconfigured() {
    let dir = tempfile::tempdir().unwrap();
    let sched = test_scheduler(dir.path());
    let cfg = DeliveryConfig {
        mode: DeliveryMode::Fixed,
        ..Default::default()
    };
    assert!(sched.resolve_delivery(&cfg).await.is_none());
}

#[tokio::test]
async fn resolve_delivery_last_mode_reads_last_channel_and_pins_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let sched = test_scheduler(dir.path());
    *sched.last_channel.lock().await = Some("telegram:default".to_string());
    *sched.last_recipient.lock().await = Some("whoever-last-messaged".to_string());

    // No `to` override — falls back to last_recipient.
    let cfg = DeliveryConfig::default(); // mode: Last
    let (ch, acc, recipient) = sched.resolve_delivery(&cfg).await.unwrap();
    assert_eq!(ch, "telegram");
    assert_eq!(acc, "default");
    assert_eq!(recipient.as_deref(), Some("whoever-last-messaged"));

    // `to` override pins the recipient while the channel still
    // resolves from last_channel — the "channel drifts, recipient
    // pinned" pattern from the 2026-08-20 incident, now explicit.
    let pinned = DeliveryConfig {
        to: Some("pinned-user".to_string()),
        ..Default::default()
    };
    let (_, _, recipient) = sched.resolve_delivery(&pinned).await.unwrap();
    assert_eq!(recipient.as_deref(), Some("pinned-user"));
}

#[tokio::test]
async fn resolve_delivery_last_mode_without_prior_channel_skips() {
    let dir = tempfile::tempdir().unwrap();
    let sched = test_scheduler(dir.path());
    // last_channel was never set.
    let cfg = DeliveryConfig::default();
    assert!(sched.resolve_delivery(&cfg).await.is_none());
}

#[test]
fn legacy_schedule_kind_discriminator_is_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let id = "test/job/019fe4cebbbb";
    let meta_dir = jobs_root.join(crate::ids::dir_name(id));
    std::fs::create_dir_all(&meta_dir).unwrap();
    // Old shape for interval jobs: display string + discriminator object.
    std::fs::write(
        meta_dir.join("meta.json"),
        r#"{"id": "test/job/019fe4cebbbb", "name": "legacy-every", "schedule": "every 30m",
            "prompt": "p", "target": "last",
            "schedule_kind": {"kind": "every", "interval_ms": 1800000}}"#,
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root.clone(), "test", "UTC".to_string(), None, tx,
        dir.path().join("lc"), dir.path().join("lr"),
    );
    let jobs = sched.jobs();
    assert!(matches!(
        jobs[0].schedule,
        Some(ScheduleSpec::Kind(ScheduleKind::Every { interval_ms: 1_800_000 }))
    ));
    sched.update_job(id, JobUpdate::default()).unwrap();
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            jobs_root.join(crate::ids::dir_name(id)).join("meta.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(saved["schedule"]["kind"], "every");
    assert_eq!(saved["schedule"]["interval_ms"], 1_800_000);
    assert!(saved.get("schedule_kind").is_none());
}

#[test]
fn legacy_jobs_json_single_file_folds_discriminators() {
    // Review finding: the pre-P1-B2 single-file fallback must fold
    // schedule_kind at the Value level too, or every/at display strings
    // would be misread as cron expressions (silently never firing).
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    std::fs::create_dir_all(&jobs_root).unwrap();
    std::fs::write(
        jobs_root.join("jobs.json"),
        r#"{"jobs": [
            {"id": "test/job/019fe4cedddd", "name": "tick", "schedule": "every 30m",
             "prompt": "p", "target": "last",
             "schedule_kind": {"kind": "every", "interval_ms": 1800000}},
            {"id": "test/job/019fe4ceeeee", "name": "old-cron", "schedule": "0 9 * * *",
             "prompt": "p", "target": "last", "schedule_kind": null}
        ]}"#,
    )
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        jobs_root, "test", "UTC".to_string(), None, tx,
        dir.path().join("lc"), dir.path().join("lr"),
    );
    let jobs = sched.jobs();
    assert_eq!(jobs.len(), 2);
    let tick = jobs.iter().find(|j| j.name.as_deref() == Some("tick")).unwrap();
    assert!(matches!(
        tick.schedule,
        Some(ScheduleSpec::Kind(ScheduleKind::Every { interval_ms: 1_800_000 }))
    ));
    let cron = jobs.iter().find(|j| j.name.as_deref() == Some("old-cron")).unwrap();
    assert!(matches!(
        cron.schedule,
        Some(ScheduleSpec::Kind(ScheduleKind::Cron { ref expr })) if expr == "0 9 * * *"
    ));
}

#[test]
fn schedule_spec_serde_roundtrip() {
    let spec = ScheduleSpec::cron("0 0 9 * * *");
    let v = serde_json::to_value(&spec).unwrap();
    assert_eq!(v, serde_json::json!({"kind": "cron", "expr": "0 0 9 * * *"}));
    let back: ScheduleSpec = serde_json::from_value(v).unwrap();
    assert!(matches!(back, ScheduleSpec::Kind(ScheduleKind::Cron { .. })));
    // Legacy string still parses (untagged read-compat).
    let legacy: ScheduleSpec =
        serde_json::from_value(serde_json::json!("0 0 9 * * *")).unwrap();
    assert!(legacy.is_at() == false);
    assert_eq!(legacy.describe(), "0 0 9 * * *");
}

#[test]
fn cron_timer_skips_scheduleless_jobs() {
    // compute_next_run(None schedule) never yields a due time.
    assert!(compute_next_run(None, None, "UTC").is_none());
    // And with a schedule it still works.
    assert!(compute_next_run(Some(&ScheduleSpec::cron("0 0 9 * * *")), None, "UTC").is_some());
}

// ── Event extraction / filters / payload appendix ─────────────────

// ── Webhook guard (rate / concurrency / idempotency) ──────────────

// ── Template rendering tests (migrated from webhook_loader.rs) ────

