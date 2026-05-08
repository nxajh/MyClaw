//! Scheduler — Heartbeat, Cron, Webhook.
//!
//! Three scheduling modes share the same execution path:
//!   construct prompt → AgentLoop::run() → handle response
//!
//! Each runs as an independent tokio task, sharing resources via Arc<SchedulerContext>.

use std::sync::Arc;
use std::time::Duration;

use chrono::Timelike;
use dashmap::DashMap;
use tokio::sync::{Mutex as TokioMutex, Mutex};

use crate::agents::Agent;
use crate::agents::AgentLoop;
use crate::agents::session_manager::SessionManager;
use crate::channels::{Channel, SendMessage};
use crate::config::scheduler::{CronConfig, CronJob, HeartbeatConfig, WebhookConfig, WebhookJob};
use crate::storage::SessionBackend;

// ── Shared context ─────────────────────────────────────────────────────────

/// Resources shared by all scheduler tasks.
pub struct SchedulerContext {
    pub agent: Agent,
    pub channels: Arc<DashMap<String, Arc<dyn Channel>>>,
    pub sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>>,
    pub session_backend: Arc<dyn SessionBackend>,
    pub timezone_offset: i32,
    /// Last channel that received a user message.
    pub last_channel: Arc<Mutex<Option<String>>>,
    pub change_rx: Option<tokio::sync::watch::Receiver<crate::agents::ChangeSet>>,
}

// ── Interval parsing ───────────────────────────────────────────────────────

/// Parse interval string like "5m", "30m", "1h" to Duration.
pub fn parse_interval(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s == "0" {
        return None;
    }

    let (num_part, multiplier) = if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else {
        // Default to minutes if no suffix
        (s, 60)
    };

    let num: u64 = num_part.parse().ok()?;
    Some(Duration::from_secs(num * multiplier))
}

// ── Active hours ───────────────────────────────────────────────────────────

/// Check if current time (in configured timezone) is within active hours.
/// Format: "HH:MM-HH:MM" e.g. "08:00-24:00".
pub fn is_active_hours(active_hours: &Option<String>, timezone_offset: i32) -> bool {
    let Some(hours) = active_hours else {
        return true; // No restriction = always active
    };

    let (start_mins, end_mins) = match parse_hours(hours) {
        Some(h) => h,
        None => return true, // Invalid format = always active
    };

    let now_utc = chrono::Utc::now();
    let local = now_utc + chrono::Duration::hours(timezone_offset as i64);
    let now_mins = local.hour() * 60 + local.minute();

    now_mins >= start_mins && now_mins < end_mins
}

/// Parse "HH:MM-HH:MM" → (start_minutes, end_minutes).
fn parse_hours(s: &str) -> Option<(u32, u32)> {
    let (start, end) = s.split_once('-')?;
    Some((parse_hhmm(start.trim())?, parse_hhmm(end.trim())?))
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let hours: u32 = h.trim().parse().ok()?;
    let mins: u32 = m.trim().parse().ok()?;
    Some(hours * 60 + mins)
}

// ── Silent response detection ──────────────────────────────────────────────

/// Check if a response is a silent "nothing to do" signal.
fn is_silent_ok(response: &str, prefix: &str) -> bool {
    let trimmed = response.trim().to_lowercase();
    let marker = format!("{}_ok", prefix);
    trimmed == marker || trimmed.contains(&marker)
}

// ── Default prompts ────────────────────────────────────────────────────────

const HEARTBEAT_PROMPT: &str =
    "read heartbeat.md if it exists. follow it strictly. \
     if nothing needs attention, reply heartbeat_ok.";

// ── Shared execution ───────────────────────────────────────────────────────

/// Create or get an AgentLoop for a scheduler session and run a prompt.
pub async fn run_scheduled_task(
    ctx: &SchedulerContext,
    session_key: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let loop_ = get_or_create_loop(ctx, session_key);
    let mut guard = loop_.lock().await;
    guard.run(prompt, None, None).await
}

fn get_or_create_loop(
    ctx: &SchedulerContext,
    session_key: &str,
) -> Arc<TokioMutex<AgentLoop>> {
    if let Some(existing) = ctx.sessions.get(session_key) {
        return existing.clone();
    }

    // Scheduler creates its own SessionManager sharing the same backend.
    let sm = SessionManager::new(ctx.session_backend.clone());
    let session = sm.get_or_create(session_key);

    let persist_hook: Arc<dyn crate::agents::PersistHook> = Arc::new(
        crate::agents::BackendPersistHook::new(ctx.session_backend.clone())
    );
    let loop_ = ctx.agent.loop_for_with_persist(session, Some(persist_hook));

    let mut loop_ = loop_;

    // Wire up file change receiver for hot-reload.
    if let Some(rx) = ctx.change_rx.clone() {
        loop_ = loop_.with_change_rx(rx);
    }

    let entry: Arc<TokioMutex<AgentLoop>> = Arc::new(TokioMutex::new(loop_));
    ctx.sessions.insert(session_key.to_string(), entry.clone());
    entry
}

/// Send a response to the configured target channel.
pub async fn send_to_target(ctx: &SchedulerContext, target: &str, content: &str) {
    let channel_name = match target {
        "none" => return,
        "last" => ctx.last_channel.lock().await.clone(),
        name => Some(name.to_string()),
    };

    let Some(ch_name) = channel_name else {
        tracing::warn!("no target channel for scheduled response");
        return;
    };

    let channel = match ctx.channels.get(&ch_name) {
        Some(ch) => ch.clone(),
        None => {
            tracing::warn!(channel = %ch_name, "target channel not found");
            return;
        }
    };

    // Scheduler messages don't have a specific recipient; the channel
    // adapter decides where to send (e.g. last active chat).
    let msg = SendMessage {
        content: content.to_string(),
        recipient: String::new(), // Channel-specific routing handled by adapter
        subject: None,
        thread_ts: None,
        cancellation_token: None,
        attachments: vec![],
        image_urls: None,
    };

    if let Err(e) = channel.send(&msg).await {
        tracing::warn!(channel = %ch_name, error = %e, "failed to send scheduled response");
    }
}

// ── Heartbeat ──────────────────────────────────────────────────────────────

/// Run the heartbeat scheduler loop.
pub async fn run_heartbeat(ctx: Arc<SchedulerContext>, config: HeartbeatConfig) {
    let interval = match parse_interval(&config.every) {
        Some(d) => d,
        None => {
            tracing::warn!(every = %config.every, "invalid heartbeat interval, disabling");
            return;
        }
    };

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        every = ?interval,
        target = %config.target,
        "heartbeat scheduler started"
    );

    loop {
        ticker.tick().await;

        if !is_active_hours(&config.active_hours, ctx.timezone_offset) {
            tracing::debug!("heartbeat skipped: outside active hours");
            continue;
        }

        // Check HEARTBEAT.md existence.
        if !std::path::Path::new("HEARTBEAT.md").exists() {
            tracing::debug!("heartbeat skipped: no HEARTBEAT.md");
            continue;
        }

        tracing::info!("heartbeat triggered");

        let prompt = config.prompt.as_deref().unwrap_or(HEARTBEAT_PROMPT);
        let result = run_scheduled_task(&ctx, "_heartbeat", prompt).await;

        match result {
            Ok(response) if is_silent_ok(&response, "heartbeat") => {
                tracing::info!("heartbeat: nothing needs attention");
            }
            Ok(response) if !response.trim().is_empty() => {
                tracing::info!(resp_len = response.len(), "heartbeat produced output");
                send_to_target(&ctx, &config.target, &response).await;
            }
            Ok(_) => {
                tracing::info!("heartbeat: empty response");
            }
            Err(e) => {
                tracing::warn!(error = %e, "heartbeat run failed");
            }
        }
    }
}

// ── Cron ────────────────────────────────────────────────────────────────────

/// Run the cron scheduler loop.
pub async fn run_cron_scheduler(ctx: Arc<SchedulerContext>, config: CronConfig) {
    let mut jobs: Vec<(cron::Schedule, CronJob)> = Vec::new();
    for job in &config.jobs {
        match job.schedule.parse::<cron::Schedule>() {
            Ok(schedule) => {
                tracing::info!(
                    schedule = %job.schedule,
                    target = %job.target,
                    prompt_preview = %job.prompt.chars().take(50).collect::<String>(),
                    "cron job registered"
                );
                jobs.push((schedule, job.clone()));
            }
            Err(e) => {
                tracing::warn!(schedule = %job.schedule, error = %e, "invalid cron expression, skipping");
            }
        }
    }

    if jobs.is_empty() {
        tracing::info!("no valid cron jobs, cron scheduler idle");
        return;
    }

    // Check every minute (cron minimum granularity).
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(job_count = jobs.len(), "cron scheduler started");

    loop {
        ticker.tick().await;

        let now = {
            let utc = chrono::Utc::now();
            utc + chrono::Duration::hours(ctx.timezone_offset as i64)
        };

        for (schedule, job) in &jobs {
            if cron_matches(schedule, &now) {
                let session_key = format!(
                    "_cron_{}",
                    job.schedule.replace([' ', '*'], "").replace('.', "_")
                );

                tracing::info!(
                    schedule = %job.schedule,
                    "cron job triggered"
                );

                let result = run_scheduled_task(&ctx, &session_key, &job.prompt).await;

                match result {
                    Ok(response) if !response.trim().is_empty() => {
                        send_to_target(&ctx, &job.target, &response).await;
                    }
                    Ok(_) => {
                        tracing::info!(schedule = %job.schedule, "cron job: empty response");
                    }
                    Err(e) => {
                        tracing::warn!(schedule = %job.schedule, error = %e, "cron job failed");
                    }
                }
            }
        }
    }
}

/// Check if a cron schedule matches the current time.
/// `now` should be a local time as DateTime<Utc> with the timezone offset applied.
fn cron_matches(schedule: &cron::Schedule, now: &chrono::DateTime<chrono::Utc>) -> bool {
    // cron::Schedule::after returns an iterator of upcoming datetimes.
    // We check if the next fire time is within the current minute.
    let from = *now - chrono::Duration::seconds(61);
    schedule.after(&from).next().is_some_and(|next| {
        let diff = (next - *now).num_seconds().abs();
        diff <= 30 // Within 30 seconds tolerance
    })
}

// ── Webhook ────────────────────────────────────────────────────────────────

use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use http_body_util::Full;

/// Run the webhook HTTP server.
pub async fn run_webhook_server(ctx: Arc<SchedulerContext>, config: WebhookConfig) {
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(port = config.port, error = %e, "webhook: failed to bind");
            return;
        }
    };

    tracing::info!(port = config.port, "webhook server started");

    // Build a route map: path → (job_index)
    let routes: Vec<(String, usize)> = config
        .jobs
        .iter()
        .enumerate()
        .map(|(i, job)| {
            let path = if job.path.starts_with('/') {
                job.path.clone()
            } else {
                format!("/{}", job.path)
            };
            (path, i)
        })
        .collect();

    let jobs = Arc::new(config.jobs);
    let secret = config.secret.clone();

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "webhook: accept failed");
                continue;
            }
        };

        let io = TokioIo::new(stream);
        let ctx = ctx.clone();
        let jobs = jobs.clone();
        let secret = secret.clone();
        let routes = routes.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                let jobs = jobs.clone();
                let secret = secret.clone();
                let routes = routes.clone();
                async move { handle_webhook(req, ctx, &jobs, &secret, &routes).await }
            });

            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                tracing::debug!(error = %e, "webhook: connection error");
            }
        });
    }
}

async fn handle_webhook(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<SchedulerContext>,
    jobs: &[WebhookJob],
    secret: &Option<String>,
    routes: &[(String, usize)],
) -> anyhow::Result<Response<Full<Bytes>>> {
    // Only accept POST.
    if req.method() != Method::POST {
        return ok_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }

    // Find matching job.
    let path = req.uri().path().to_string();
    let job_idx = match routes.iter().find(|(p, _)| p == &path) {
        Some((_, idx)) => *idx,
        None => return ok_response(StatusCode::NOT_FOUND, "no webhook at this path"),
    };
    let job = &jobs[job_idx];

    // Extract signature header before consuming body.
    let sig_header = req
        .headers()
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Collect body bytes.
    let body_bytes = match collect_body(req.into_body()).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "webhook: failed to read body");
            return ok_response(StatusCode::BAD_REQUEST, "failed to read body");
        }
    };

    // Verify HMAC signature if configured.
    if let Some(secret) = secret {
        match sig_header {
            Some(ref sig) if !verify_signature(&body_bytes, secret, sig) => {
                tracing::warn!(path = %path, "webhook: HMAC verification failed");
                return ok_response(StatusCode::UNAUTHORIZED, "invalid signature");
            }
            None => {
                tracing::warn!(path = %path, "webhook: missing signature header");
                return ok_response(StatusCode::UNAUTHORIZED, "missing signature");
            }
            _ => {} // Signature valid
        }
    }

    tracing::info!(path = %path, "webhook triggered");

    // Extract payload field if configured.
    let payload_context = match &job.payload_key {
        Some(key) => extract_payload(&body_bytes, key),
        None => None,
    };

    // Build prompt.
    let prompt = match payload_context {
        Some(ctx_text) => format!("{}\n\n---\nWebhook payload:\n{}", job.prompt, ctx_text),
        None => job.prompt.clone(),
    };

    let session_key = format!("_webhook_{}", path.trim_start_matches('/').replace('/', "_"));
    let result = run_scheduled_task(&ctx, &session_key, &prompt).await;

    match result {
        Ok(response) => {
            if !response.trim().is_empty() && job.target != "none" {
                send_to_target(&ctx, &job.target, &response).await;
            }
            ok_response(StatusCode::OK, "ok")
        }
        Err(e) => {
            tracing::warn!(error = %e, "webhook: agent run failed");
            ok_response(StatusCode::INTERNAL_SERVER_ERROR, "agent error")
        }
    }
}

/// Verify HMAC-SHA256 signature against the `X-Hub-Signature-256` header value.
fn verify_signature(body: &[u8], secret: &str, header_value: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let result = mac.finalize();
    let expected_hex = format!("sha256={}", hex::encode(result.into_bytes()));

    // Constant-time comparison.
    let a = expected_hex.as_bytes();
    let b = header_value.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Extract a value from a JSON body by dot-separated key path.
/// e.g. "commits[0].message" or "action"
fn extract_payload(body: &[u8], key: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_slice(body).ok()?;
    let result = navigate_json(&val, key)?;
    match result {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => None,
        other => Some(serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string())),
    }
}

/// Navigate a JSON value by a dot-separated path, supporting array indices.
/// e.g. "commits[0].message" → val["commits"][0]["message"]
fn navigate_json<'a>(val: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = val;
    for segment in path.split('.') {
        // Check for array index: "field[0]"
        if let Some(bracket) = segment.find('[') {
            let field = &segment[..bracket];
            if !field.is_empty() {
                current = current.get(field)?;
            }
            // Parse all indices: [0][1] etc.
            let rest = &segment[bracket..];
            for idx_str in rest.split(']').filter(|s| !s.is_empty()) {
                let idx: usize = idx_str.trim_start_matches('[').parse().ok()?;
                current = current.get(idx)?;
            }
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current)
}

/// Collect full body bytes from an incoming body stream.
async fn collect_body<B>(body: B) -> anyhow::Result<Bytes>
where
    B: hyper::body::Body,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    use http_body_util::BodyExt;
    let collected = body.collect().await?;
    Ok(collected.to_bytes())
}

fn ok_response(status: StatusCode, body: &str) -> anyhow::Result<Response<Full<Bytes>>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .map_err(Into::into)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_hours_valid() {
        assert_eq!(parse_hhmm("08:00"), Some(480));
        assert_eq!(parse_hhmm("24:00"), Some(1440));
        assert_eq!(parse_hhmm("13:30"), Some(810));
    }

    #[test]
    fn is_active_hours_no_restriction() {
        assert!(is_active_hours(&None, 8));
    }

    #[test]
    fn is_active_hours_invalid_format_always_active() {
        assert!(is_active_hours(&Some("bad".to_string()), 8));
    }

    #[test]
    fn silent_heartbeat_ok() {
        assert!(is_silent_ok("heartbeat_ok", "heartbeat"));
        assert!(is_silent_ok("Heartbeat_OK", "heartbeat"));
        assert!(is_silent_ok(" heartbeat_ok ", "heartbeat"));
        assert!(!is_silent_ok("I found something", "heartbeat"));
    }

    // ── Webhook tests ──────────────────────────────────────────────────────

    #[test]
    fn verify_signature_valid() {
        let body = b"test payload";
        let secret = "my-secret";
        // Compute expected signature
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature(body, secret, &sig));
    }

    #[test]
    fn verify_signature_invalid() {
        let body = b"test payload";
        assert!(!verify_signature(body, "secret", "sha256=bad_hex"));
    }

    #[test]
    fn verify_signature_wrong_length() {
        assert!(!verify_signature(b"body", "secret", "sha256=abc"));
    }

    #[test]
    fn extract_payload_string() {
        let body = br#"{"action":"opened","title":"PR #1"}"#;
        assert_eq!(extract_payload(body, "action"), Some("opened".to_string()));
    }

    #[test]
    fn extract_payload_nested() {
        let body = br#"{"pull_request":{"title":"Fix bug","number":42}}"#;
        assert_eq!(extract_payload(body, "pull_request.title"), Some("Fix bug".to_string()));
    }

    #[test]
    fn extract_payload_array_index() {
        let body = br#"{"commits":[{"message":"fix"},{"message":"feat"}]}"#;
        assert_eq!(extract_payload(body, "commits[0].message"), Some("fix".to_string()));
    }

    #[test]
    fn extract_payload_missing_key() {
        let body = br#"{"action":"opened"}"#;
        assert_eq!(extract_payload(body, "nonexistent"), None);
    }

    #[test]
    fn extract_payload_invalid_json() {
        let body = b"not json";
        assert_eq!(extract_payload(body, "key"), None);
    }

    #[test]
    fn navigate_json_simple() {
        let val: serde_json::Value = serde_json::json!({"foo": "bar"});
        let result = navigate_json(&val, "foo");
        assert_eq!(result.unwrap().as_str(), Some("bar"));
    }

    #[test]
    fn navigate_json_deep() {
        let val: serde_json::Value = serde_json::json!({"a": {"b": {"c": 42}}});
        let result = navigate_json(&val, "a.b.c");
        assert_eq!(result.unwrap().as_u64(), Some(42));
    }
}
