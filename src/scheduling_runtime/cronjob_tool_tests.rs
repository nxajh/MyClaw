//! cronjob_tool 集成测试 —— #151 Phase 8+ 从 tools 层迁出（scheduling_runtime 层）。
//!
//! 这些测试需要真实 `Scheduler`（临时目录持久化 + 真实热路径），L3 层经
//! api/L1 门面无法构造具体类型，故整体迁到 L4：L4→L3 引用方向合法，
//! 测试语义与工具对外行为零改动。

use std::sync::Arc;

use crate::providers::Tool;
use crate::scheduling_runtime::scheduler::Scheduler;
use crate::scheduling_types::cron_types::*;
use crate::scheduling_types::job_types::{JobEntry, SchedulerApi};
use crate::tools::cronjob_tool::{format_unknown_job_listing, parse_webhook_channel, CronJobTool};

fn args(json: serde_json::Value) -> serde_json::Value { json }

#[test]
fn rejects_malformed_filter_object() {
    // Review finding: silently dropping a malformed filter would widen
    // the trigger condition beyond what the user expects — reject hard.
    let e = parse_webhook_channel(
        &args(serde_json::json!({"webhook": {"secret": "s", "filters": ["oops"]}})),
        Some("ok-name"),
    )
    .unwrap_err();
    assert!(e.contains("must be an object"));
}

#[test]
fn rejects_filter_missing_field() {
    let e = parse_webhook_channel(
        &args(serde_json::json!({"webhook": {"secret": "s", "filters": [{"equals": "x"}]}})),
        Some("ok-name"),
    )
    .unwrap_err();
    assert!(e.contains("'field'"));
}

#[test]
fn accepts_valid_filters() {
    let wh = parse_webhook_channel(
        &args(serde_json::json!({"webhook": {"secret": "s",
            "filters": [{"field": "action", "equals": "opened"}]}})),
        Some("ok-name"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(wh.filters.as_deref().map(|f| f.len()), Some(1));
}
}

#[cfg(test)]
mod update_echo_tests {
use crate::scheduling_runtime::scheduler::Scheduler;

fn test_tool(dir: &std::path::Path) -> CronJobTool {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        dir.join("jobs"),
        "test",
        "UTC".to_string(),
        None,
        tx,
        dir.join("last_channel"),
        dir.join("last_recipient"),
    );
    let entry = JobEntry {
        id: String::new(),
        schedule: Some(ScheduleSpec::cron("0 9 * * *")),
        webhook: None,
        prompt: "p".to_string(),
        name: Some("probe".to_string()),
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
    };
    sched.add_job(entry).unwrap();
    CronJobTool::new(sched)
}

fn job_id(tool: &CronJobTool) -> String {
    tool.scheduler.jobs()[0].id.clone()
}

#[test]
fn update_echoes_only_the_changed_fields() {
    // #76: five consecutive patches to *different* fields must produce
    // five *different* result strings, or the NoProgress loop breaker
    // (same tool + different args + same result, N times) kills a
    // perfectly legitimate field-by-field update sequence.
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let id = job_id(&tool);

    let cases: Vec<(serde_json::Value, &str)> = vec![
        // Legacy `target` alias folds into the unified `delivery` field (#78).
        (serde_json::json!({"id": &id, "target": "wechat"}), "delivery"),
        (serde_json::json!({"id": &id, "schedule": "every 2m"}), "schedule"),
        (
            serde_json::json!({"id": &id, "active_hours": "09:00-18:00"}),
            "active_hours",
        ),
        (serde_json::json!({"id": &id, "tz": "Asia/Shanghai"}), "tz"),
        (serde_json::json!({"id": &id, "enabled": false}), "enabled"),
    ];

    let mut outputs = std::collections::HashSet::new();
    for (args, expected_field) in &cases {
        let result = tool.handle_update(args).unwrap();
        assert!(result.success, "update failed: {:?}", result.error);
        assert!(
            result.output.contains(expected_field),
            "expected '{}' to mention field '{}'",
            result.output,
            expected_field
        );
        outputs.insert(result.output.clone());
    }
    assert_eq!(
        outputs.len(),
        cases.len(),
        "every distinct-field update must produce a distinct result string"
    );
}

#[test]
fn update_with_no_fields_is_still_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let id = job_id(&tool);
    let result = tool.handle_update(&serde_json::json!({"id": &id})).unwrap();
    assert!(!result.success);
}

// ── #78: target/delivery unification at the tool boundary ────────

#[test]
fn legacy_target_alias_resolves_to_fixed_delivery() {
    let cfg = resolve_delivery_for_create(&serde_json::json!({"target": "wechat:work"}))
        .unwrap();
    assert_eq!(cfg.mode, DeliveryMode::Fixed);
    assert_eq!(cfg.channel.as_deref(), Some("wechat"));
    assert_eq!(cfg.account_id.as_deref(), Some("work"));
}

#[test]
fn create_defaults_to_last_when_neither_target_nor_delivery_given() {
    let cfg = resolve_delivery_for_create(&serde_json::json!({})).unwrap();
    assert_eq!(cfg.mode, DeliveryMode::Last);
}

#[test]
fn delivery_object_wins_over_legacy_target_when_both_given() {
    let cfg = resolve_delivery_for_create(&serde_json::json!({
        "target": "wechat",
        "delivery": {"mode": "none"}
    }))
    .unwrap();
    assert_eq!(cfg.mode, DeliveryMode::None);
}

#[test]
fn fixed_mode_without_channel_is_rejected() {
    let err = resolve_delivery_for_create(&serde_json::json!({
        "delivery": {"mode": "fixed"}
    }))
    .unwrap_err();
    assert!(err.contains("requires 'channel'"), "got: {err}");
}

#[test]
fn update_with_neither_target_nor_delivery_leaves_delivery_unchanged() {
    assert_eq!(
        resolve_delivery_for_update(&serde_json::json!({"id": "x"})).unwrap(),
        None
    );
}

#[test]
fn update_via_cronjob_tool_honors_delivery_object() {
    // End-to-end: the tool boundary accepts the new `delivery` schema
    // and it lands on the job unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let id = job_id(&tool);
    let result = tool
        .handle_update(&serde_json::json!({
            "id": &id,
            "delivery": {"mode": "fixed", "channel": "discord", "to": "chan-9", "thread_id": "t-1"}
        }))
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    let job = tool.scheduler.jobs().into_iter().find(|j| j.id == id).unwrap();
    assert_eq!(job.delivery.mode, DeliveryMode::Fixed);
    assert_eq!(job.delivery.channel.as_deref(), Some("discord"));
    assert_eq!(job.delivery.to.as_deref(), Some("chan-9"));
    assert_eq!(job.delivery.thread_id.as_deref(), Some("t-1"));
}
}

/// issue #134 (P3): all six `Job '{}' not found` sites list what's actually
/// configured instead of a bare failure.
#[cfg(test)]
mod unknown_job_listing_tests {
use crate::scheduling_runtime::scheduler::Scheduler;

fn test_tool(dir: &std::path::Path) -> CronJobTool {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let sched = Scheduler::new(
        dir.join("jobs"),
        "test",
        "UTC".to_string(),
        None,
        tx,
        dir.join("last_channel"),
        dir.join("last_recipient"),
    );
    let entry = JobEntry {
        id: String::new(),
        schedule: Some(ScheduleSpec::cron("0 9 * * *")),
        webhook: None,
        prompt: "p".to_string(),
        name: Some("real-job".to_string()),
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
    };
    sched.add_job(entry).unwrap();
    CronJobTool::new(sched)
}

#[test]
fn update_unknown_id_lists_real_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let result = tool
        .handle_update(&serde_json::json!({"id": "no-such-job", "enabled": false}))
        .unwrap();
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("no-such-job"));
    assert!(err.contains("real-job"), "listing must show what does exist: {err}");
}

#[test]
fn pause_unknown_id_lists_real_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let result = tool
        .handle_set_enabled(&serde_json::json!({"id": "no-such-job"}), false)
        .unwrap();
    assert!(result.error.unwrap().contains("real-job"));
}

#[test]
fn run_unknown_id_lists_real_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let result = tool
        .handle_run(&serde_json::json!({"id": "no-such-job"}))
        .unwrap();
    assert!(result.error.unwrap().contains("real-job"));
}

#[test]
fn remove_unknown_id_lists_real_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let result = tool
        .handle_remove(&serde_json::json!({"id": "no-such-job"}))
        .unwrap();
    assert!(result.error.unwrap().contains("real-job"));
}

#[test]
fn log_unknown_id_lists_real_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = test_tool(tmp.path());
    let result = tool
        .handle_log(&serde_json::json!({"id": "no-such-job"}))
        .unwrap();
    assert!(result.error.unwrap().contains("real-job"));
}

/// No cron jobs configured at all must say so plainly.
#[test]
fn empty_listing_says_so() {
    assert!(format_unknown_job_listing(vec![]).contains("No cron jobs configured"));
}

/// #133's reviewed convention: newest (`created_at`) first, and the cap
/// keeps the newest entries.
#[test]
fn newest_first_and_capped() {
    let jobs: Vec<JobEntry> = (0..25)
        .map(|i| JobEntry {
            id: format!("job{i}"),
            schedule: Some(ScheduleSpec::cron("0 9 * * *")),
            webhook: None,
            prompt: "p".to_string(),
            name: Some(format!("job-{i}")),
            tz: None,
            active_hours: None,
            enabled: true,
            last_run_at: None,
            next_run_at: None,
            // Zero-padded so string comparison sorts the same as
            // numeric comparison would — mirrors real RFC3339 timestamps.
            created_at: Some(format!("2026-01-01T00:00:{i:02}Z")),
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
        })
        .collect();
    let listing = format_unknown_job_listing(jobs);
    assert!(listing.contains("and 5 more"));
    assert!(listing.contains("job24"), "newest (i=24) must survive the cap: {listing}");
    assert!(!listing.contains("  job0 "), "oldest (i=0) must be the one dropped: {listing}");
}
