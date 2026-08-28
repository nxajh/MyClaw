use super::*;
use crate::scheduling_runtime::scheduler::tests::{test_entry, test_scheduler};
use crate::api::message::Channel;
use crate::scheduling_runtime::scheduler::JobEntry;
use crate::scheduling_types::cron_types::{DeliveryMode, ScheduleSpec};
use parking_lot::Mutex as ParkMutex;
use std::time::Duration;

/// Moved from `agents::orchestrator::is_silent_ok` (#151 Phase 3d, SCC
fn is_silent_ok(response: &str, prefix: &str) -> bool {
    let trimmed = response.trim().to_lowercase();
    let marker = format!("{}_ok", prefix);
    trimmed == marker
}

/// local, replaces `agents::orchestrator::test_support::MockChannel`
/// so the test no longer reaches into agents).
struct MockChannel {
    sent: std::sync::Arc<std::sync::Mutex<Vec<ChannelOutboundMessage>>>,
}

impl MockChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }
}

#[async_trait::async_trait]
impl Channel for MockChannel {
    fn name(&self) -> &str {
        "mock"
    }
    async fn listen(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<crate::api::message::ChannelInboundMessage>> {
        anyhow::bail!("mock channel does not listen")
    }
    async fn health_check(&self) -> bool {
        true
    }
    async fn send_message(&self, msg: &ChannelOutboundMessage) -> anyhow::Result<crate::api::message::OutboundSendResult> {
        self.sent.lock().unwrap().push(msg.clone());
        Ok(crate::api::message::OutboundSendResult::empty())
    }
}

/// OrchestratorHook test double: channel table + recorded turns.
struct TestHook {
    channels: std::collections::HashMap<(String, String), Arc<dyn Channel>>,
    turns: ParkMutex<Vec<(String, String)>>,
}

impl TestHook {
    fn new(
        channels: Vec<((String, String), Arc<dyn Channel>)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            channels: channels.into_iter().collect(),
            turns: ParkMutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl OrchestratorHook for TestHook {
    async fn run_scheduled_turn(
        &self,
        session_key: &str,
        prompt: &str,
    ) -> anyhow::Result<String> {
        self.turns
            .lock()
            .push((session_key.to_string(), prompt.to_string()));
        Ok("ok".to_string())
    }
    fn outbound_channel(
        &self,
        channel_type: &str,
        account_id: &str,
    ) -> Option<Arc<dyn Channel>> {
        self.channels
            .get(&(channel_type.to_string(), account_id.to_string()))
            .cloned()
    }
}

#[test]
fn silent_marker_ok() {
    assert!(is_silent_ok("cron_ok", "cron"));
    assert!(is_silent_ok("Cron_OK", "cron"));
    assert!(is_silent_ok(" cron_ok ", "cron"));
    assert!(!is_silent_ok("I found something", "cron"));
}

#[test]
fn verify_hmac_signature_valid() {
    let body = b"test payload";
    let secret = "my-secret";
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    assert!(verify_hmac_signature(body, secret, &sig));
}

#[test]
fn verify_hmac_signature_invalid() {
    assert!(!verify_hmac_signature(
        b"test payload",
        "secret",
        "sha256=bad_hex"
    ));
}

#[test]
fn verify_hmac_signature_wrong_length() {
    assert!(!verify_hmac_signature(b"body", "secret", "sha256=abc"));
}

fn wh_entry(name: Option<&str>, secret: &str, schedule: Option<&str>) -> JobEntry {
    JobEntry {
        webhook: Some(WebhookDef {
            auth: "hmac".to_string(),
            secret: secret.to_string(),
            events: None,
            filters: None,
            payload_off: false,
        }),
        name: name.map(|s| s.to_string()),
        schedule: schedule.map(ScheduleSpec::cron),
        ..test_entry("0 0 9 * * *")
    }
}

#[test]
fn webhook_projection_derives_route_from_name() {
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    sched
        .add_job(wh_entry(Some("gh-issues"), "s3cret", None))
        .unwrap();
    let jobs = sched.webhook_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].route, "gh-issues");
    assert_eq!(jobs[0].secret, "s3cret");
}

#[test]
fn add_job_rejects_nameless_entry() {
    // §3.4: name is required at the write boundary; the load path
    // backfills legacy files, so the store invariant always holds.
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    let err = sched.add_job(wh_entry(None, "s", None));
    assert!(err.is_err());
}

#[test]
fn webhook_routes_reflect_live_store_changes() {
    // The HTTP server projects routes per request from this same store
    // (no startup snapshot) — adding/removing a webhook job must be
    // visible in the projection immediately.
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    assert!(sched.webhook_jobs().is_empty());
    let id = sched
        .add_job(wh_entry(Some("live-route"), "s", None))
        .unwrap();
    let jobs = sched.webhook_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].route, "live-route");
    sched.remove_job(&id).unwrap();
    assert!(sched.webhook_jobs().is_empty());
}

#[test]
fn webhook_projection_rejects_bad_slug_secret_and_builtin_names() {
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    // Empty secret → rejected at load.
    sched
        .add_job(wh_entry(Some("no-secret"), "", None))
        .unwrap();
    // Non-slug name → rejected.
    sched
        .add_job(wh_entry(Some("Bad Slug"), "s", None))
        .unwrap();
    // Built-in route collision → rejected.
    sched.add_job(wh_entry(Some("agent"), "s", None)).unwrap();
    assert!(sched.webhook_jobs().is_empty());
}

#[test]
fn webhook_projection_duplicate_routes_keep_first() {
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    sched
        .add_job(wh_entry(Some("dup"), "s1", None))
        .unwrap();
    sched
        .add_job(wh_entry(Some("dup"), "s2", None))
        .unwrap();
    let jobs = sched.webhook_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].secret, "s1");
}

#[test]
fn extract_event_type_header_chain() {
    let payload = serde_json::json!({"type": "payload-type"});
    assert_eq!(
        extract_event_type(Some("push"), &payload),
        Some("push".to_string())
    );
    assert_eq!(
        extract_event_type(None, &payload),
        Some("payload-type".to_string())
    );
    let payload2 = serde_json::json!({"event_type": "et"});
    assert_eq!(
        extract_event_type(None, &payload2),
        Some("et".to_string())
    );
    assert_eq!(extract_event_type(None, &serde_json::json!({})), None);
}

#[test]
fn filter_matches_equals_matches_not() {
    let payload = serde_json::json!({
        "action": "opened",
        "issue": {"state": "open", "number": 7}
    });
    let f = |field: &str, equals: Option<&str>, matches_: Option<&str>, not: bool| WebhookFilter {
        field: field.to_string(),
        equals: equals.map(|s| s.to_string()),
        matches: matches_.map(|s| s.to_string()),
        not,
    };
    assert!(filter_matches(&f("action", Some("opened"), None, false), &payload));
    assert!(!filter_matches(&f("action", Some("closed"), None, false), &payload));
    assert!(filter_matches(&f("action", Some("closed"), None, true), &payload));
    assert!(filter_matches(&f("issue.state", Some("open"), None, false), &payload));
    assert!(filter_matches(&f("issue.number", Some("7"), None, false), &payload));
    assert!(filter_matches(&f("issue.title", Some("x"), None, true), &payload)); // missing + not
    assert!(filter_matches(&f("action", None, Some("open.*"), false), &payload));
}

#[test]
fn pretty_payload_truncates_to_limit() {
    let big = serde_json::json!({"data": "x".repeat(10_000)});
    let out = pretty_payload(&big, 4000);
    assert!(out.chars().count() < 4100);
    assert!(out.ends_with("…[truncated]"));
    assert_eq!(
        pretty_payload(&serde_json::json!({"a": 1}), 4000),
        "{\n  \"a\": 1\n}"
    );
}

#[test]
fn guard_rate_limit_window() {
    let g = WebhookGuard::default();
    for _ in 0..120 {
        assert!(g.check_rate("r", "1.2.3.4"));
    }
    assert!(!g.check_rate("r", "1.2.3.4")); // over cap
    assert!(g.check_rate("r", "5.6.7.8")); // other IP fine
    assert!(g.check_rate("other", "1.2.3.4")); // other route fine
}

#[test]
fn guard_inflight_cap_and_release() {
    let g = std::sync::Arc::new(WebhookGuard::default());
    let a = g.acquire("r", std::sync::Arc::clone(&g)).unwrap();
    assert!(g.acquire("r", std::sync::Arc::clone(&g)).is_some());
    drop(a);
    // Slot released → acquire succeeds again.
    assert!(g.acquire("r", std::sync::Arc::clone(&g)).is_some());
}

#[test]
fn guard_inflight_enforces_cap_of_8() {
    let g = std::sync::Arc::new(WebhookGuard::default());
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(g.acquire("r", std::sync::Arc::clone(&g)).unwrap());
    }
    assert!(g.acquire("r", std::sync::Arc::clone(&g)).is_none());
}

#[test]
fn guard_delivery_idempotency() {
    let g = WebhookGuard::default();
    assert!(g.check_delivery("d-1"));
    assert!(!g.check_delivery("d-1")); // duplicate
    assert!(g.check_delivery("d-2"));
}

#[tokio::test]
async fn send_to_target_webhook_path_now_honors_delivery_to_and_thread() {
    // #78: `send_to_target` (the webhook dispatch path) used to take a
    // raw `target: &str` and never looked at `delivery` at all — `to`
    // and `thread_id` were silently dropped even though the cron
    // dispatch path already honored `to`. Prove the unified path
    // actually delivers to the configured recipient/thread now.
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    let mock = MockChannel::new();
    let ctx = WebhookContext {
        hook: TestHook::new(vec![(
            ("wechat".to_string(), "default".to_string()),
            mock.clone() as Arc<dyn Channel>,
        )]),
        timezone: "UTC".to_string(),
        scheduler: sched.clone(),
    };

    let delivery = DeliveryConfig {
        mode: DeliveryMode::Fixed,
        channel: Some("wechat".to_string()),
        to: Some("user-42".to_string()),
        thread_id: Some("thread-7".to_string()),
        ..Default::default()
    };
    send_to_target(&ctx, &delivery, "hello from webhook").await;

    let sent = mock.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].receiver.id, "user-42");
    assert_eq!(sent[0].receiver.thread_id.as_deref(), Some("thread-7"));
    assert_eq!(sent[0].content.text, "hello from webhook");
}

#[tokio::test]
async fn send_to_target_none_mode_delivers_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    let mock = MockChannel::new();
    let ctx = WebhookContext {
        hook: TestHook::new(vec![(
            ("wechat".to_string(), "default".to_string()),
            mock.clone() as Arc<dyn Channel>,
        )]),
        timezone: "UTC".to_string(),
        scheduler: sched.clone(),
    };
    let delivery = DeliveryConfig {
        mode: DeliveryMode::None,
        channel: Some("wechat".to_string()),
        ..Default::default()
    };
    send_to_target(&ctx, &delivery, "should not be sent").await;
    assert!(mock.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn webhook_dispatch_returns_202_without_awaiting_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let sched = test_scheduler(tmp.path());
    let ctx = Arc::new(WebhookContext {
        hook: TestHook::new(vec![]),
        timezone: "UTC".to_string(),
        scheduler: sched.clone(),
    });

    let job = WebhookJobDef {
        id: "myclaw/job/019fffff-0000-7000-8000-000000000001".to_string(),
        route: "slow-job".to_string(),
        secret: "s".to_string(),
        auth: WebhookAuth::Hmac,
        delivery: DeliveryConfig {
            mode: DeliveryMode::None,
            ..Default::default()
        },
        prompt_template: "p".to_string(),
        events: None,
        filters: None,
        payload_off: false,
    };
    let guard = Arc::new(WebhookGuard::default());
    let inflight = guard.acquire(&job.route, Arc::clone(&guard)).unwrap();

    // Mock turn: it cannot complete until the test releases it — and the
    // release happens only AFTER the 202 assert below. If dispatch ever
    // awaits the turn inline (the connection-coupling regression), the
    // timeout trips and the test fails instead of hanging.
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        dispatch_webhook_turn(
            ctx,
            job.clone(),
            "_job_slow-job".to_string(),
            "prompt".to_string(),
            serde_json::json!({"verify": "mock"}),
            inflight,
            move |_ctx, _session_key, _prompt| async move {
                let _ = release_rx.await;
                let out: anyhow::Result<String> = Ok("MOCK_OUTPUT".to_string());
                out
            },
        ),
    )
    .await
    .expect("202 must return without awaiting the turn")
    .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    release_tx.send(()).unwrap();

    // Background task completes: the run record lands with the webhook
    // trigger and the mock output — proving the turn outlived the
    // response instead of being cancelled with it.
    let dir = crate::ids::dir_name(&job.id);
    let log_path = tmp
        .path()
        .join("jobs")
        .join(&dir)
        .join("run_logs")
        .join(format!("{}.jsonl", dir));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut record = None;
    while std::time::Instant::now() < deadline {
        if let Ok(line) = std::fs::read_to_string(&log_path) {
            if let Some(last) = line.lines().last() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(last) {
                    record = Some(v);
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let record = record.expect("run record must be written after the turn completes");
    assert_eq!(record["trigger"], "webhook");
    assert!(record["output_preview"]
        .as_str()
        .unwrap_or_default()
        .contains("MOCK_OUTPUT"));
}

#[test]
fn render_template_simple() {
    let template = "Hello {{name}}!";
    let payload = serde_json::json!({"name": "world"});
    assert_eq!(render_template(template, &payload), "Hello world!");
}

#[test]
fn render_template_nested() {
    let template = "Issue: {{issue.title}} by {{issue.user.login}}";
    let payload = serde_json::json!({
        "issue": {
            "title": "Fix bug",
            "user": {"login": "alice"}
        }
    });
    assert_eq!(
        render_template(template, &payload),
        "Issue: Fix bug by alice"
    );
}

#[test]
fn render_template_array_index() {
    let template = "First commit: {{commits[0].message}}";
    let payload = serde_json::json!({
        "commits": [{"message": "fix"}, {"message": "feat"}]
    });
    assert_eq!(render_template(template, &payload), "First commit: fix");
}

#[test]
fn render_template_missing_field() {
    let template = "Hello {{name}}!";
    let payload = serde_json::json!({});
    assert_eq!(render_template(template, &payload), "Hello !");
}

#[test]
fn render_template_multiple_same_field() {
    let template = "{{x}} and {{x}}";
    let payload = serde_json::json!({"x": "foo"});
    assert_eq!(render_template(template, &payload), "foo and foo");
}

#[test]
fn render_template_no_placeholders() {
    let template = "No placeholders here.";
    let payload = serde_json::json!({});
    assert_eq!(render_template(template, &payload), "No placeholders here.");
}

#[test]
fn render_template_number_and_bool() {
    let template = "Count: {{count}}, Active: {{active}}";
    let payload = serde_json::json!({"count": 42, "active": true});
    assert_eq!(
        render_template(template, &payload),
        "Count: 42, Active: true"
    );
}

#[test]
fn render_template_unclosed_braces_ignored() {
    let template = "Hello {{name} not closed";
    let payload = serde_json::json!({"name": "world"});
    assert_eq!(
        render_template(template, &payload),
        "Hello {{name} not closed"
    );
}

#[test]
fn route_slug_accepts_lowercase_slugs() {
    assert!(is_route_slug("github-issues"));
    assert!(is_route_slug("r2d2"));
    assert!(is_route_slug("a"));
}

#[test]
fn route_slug_rejects_bad_names() {
    assert!(!is_route_slug("")); // empty
    assert!(!is_route_slug("GitHub")); // uppercase
    assert!(!is_route_slug("under_score")); // underscore
    assert!(!is_route_slug("中文")); // unicode
    assert!(!is_route_slug("has/slash"));
    assert!(!is_route_slug(&"x".repeat(65))); // too long
}

