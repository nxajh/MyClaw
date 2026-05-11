//! Scheduler — Heartbeat, Cron, Webhook, and pure Scheduler.
//!
//! Scheduling modes:
//!   - Scheduler (pure): sends timing events via mpsc, does NOT create agents.
//!   - Heartbeat/Cron: legacy standalone loops (retained for backward compat).
//!   - Webhook: HTTP server for external triggers.
//!
//! Job definitions come from files (`cron/*.md`, `webhooks/*.md`),
//! not from TOML config. Config only holds global settings.

use std::sync::Arc;
use std::time::Duration;

use chrono::Timelike;
use dashmap::DashMap;
use tokio::sync::{Mutex as TokioMutex, Mutex};

use crate::agents::Agent;
use crate::agents::AgentLoop;
use crate::agents::orchestrator::SchedulerEvent;
use crate::agents::webhook_loader::{WebhookAuth, WebhookJobDef, render_template};
use crate::channels::{Channel, SendMessage};
use crate::config::scheduler::{CronJob, HeartbeatConfig, WebhookConfig};
use crate::storage::SessionBackend;

// ── Pure Scheduler ──────────────────────────────────────────────────────────

/// Pure scheduler — sends timing events via mpsc, does NOT create agents.
pub struct Scheduler {
    heartbeat_config: Option<HeartbeatConfig>,
    cron_schedules: Vec<(cron::Schedule, CronJob)>,
    timezone_offset: i32,
    event_tx: tokio::sync::mpsc::Sender<SchedulerEvent>,
}

impl Scheduler {
    pub fn new(
        heartbeat_config: Option<HeartbeatConfig>,
        cron_jobs: Vec<CronJob>,
        timezone_offset: i32,
        event_tx: tokio::sync::mpsc::Sender<SchedulerEvent>,
    ) -> Self {
        let cron_schedules = cron_jobs.iter()
            .filter_map(|j| {
                match j.schedule.parse::<cron::Schedule>() {
                    Ok(s) => {
                        tracing::info!(schedule = %j.schedule, target = %j.target, "cron job registered");
                        Some((s, j.clone()))
                    }
                    Err(e) => {
                        tracing::warn!(schedule = %j.schedule, error = %e, "invalid cron expression, skipping");
                        None
                    }
                }
            })
            .collect();
        Self { heartbeat_config, cron_schedules, timezone_offset, event_tx }
    }

    /// Run the scheduler loop — sends events via mpsc.
    pub async fn run(self) {
        let mut heartbeat_ticker = self.heartbeat_config.as_ref().and_then(|cfg| {
            parse_interval(&cfg.every).map(|interval| {
                let mut t = tokio::time::interval(interval);
                t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                t
            })
        });

        let mut cron_ticker = if self.cron_schedules.is_empty() {
            None
        } else {
            let mut t = tokio::time::interval(Duration::from_secs(60));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            Some(t)
        };

        tracing::info!(
            heartbeat = heartbeat_ticker.is_some(),
            cron_jobs = self.cron_schedules.len(),
            "scheduler started"
        );

        loop {
            tokio::select! {
                _ = async {
                    if let Some(t) = heartbeat_ticker.as_mut() { t.tick().await; }
                    else { std::future::pending::<()>().await; }
                }, if heartbeat_ticker.is_some() => {
                    let config = self.heartbeat_config.as_ref().unwrap();
                    if !is_active_hours(&config.active_hours, self.timezone_offset) {
                        tracing::debug!("heartbeat skipped: outside active hours");
                        continue;
                    }
                    let _ = self.event_tx.send(SchedulerEvent::Heartbeat {
                        target: config.target.clone(),
                    }).await;
                }
                _ = async {
                    if let Some(t) = cron_ticker.as_mut() { t.tick().await; }
                    else { std::future::pending::<()>().await; }
                }, if cron_ticker.is_some() => {
                    let now = {
                        let utc = chrono::Utc::now();
                        utc + chrono::Duration::hours(self.timezone_offset as i64)
                    };
                    for (schedule, job) in &self.cron_schedules {
                        if !cron_matches(schedule, &now) { continue; }
                        if !is_active_hours(&job.active_hours, self.timezone_offset) { continue; }
                        let session_key = format!("_cron_{}",
                            job.schedule.replace([' ', '*'], "_").replace('.', "_"));
                        let _ = self.event_tx.send(SchedulerEvent::Cron {
                            session_key,
                            prompt: job.prompt.clone(),
                            target: job.target.clone(),
                        }).await;
                    }
                }
            }
        }
    }
}

// ── Webhook context ────────────────────────────────────────────────────────

/// Resources needed by the webhook server to run agent tasks.
/// Heartbeat and cron use the Orchestrator event path instead.
pub struct WebhookContext {
    pub agent: Agent,
    pub channels: Arc<DashMap<String, Arc<dyn Channel>>>,
    pub sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>>,
    /// Shared session manager — avoids creating throwaway instances per request.
    pub session_manager: Arc<crate::agents::session_manager::SessionManager>,
    /// Backend kept separately for persist hooks (BackendPersistHook needs it).
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

// ── Webhook execution helpers ──────────────────────────────────────────────

/// Create or get an AgentLoop for a webhook session and run a prompt.
pub async fn run_scheduled_task(
    ctx: &WebhookContext,
    session_key: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let loop_ = get_or_create_loop(ctx, session_key);
    let mut guard = loop_.lock().await;
    guard.run(prompt, None, None).await
}

fn get_or_create_loop(ctx: &WebhookContext, session_key: &str) -> Arc<TokioMutex<AgentLoop>> {
    if let Some(existing) = ctx.sessions.get(session_key) {
        return existing.clone();
    }

    let session = ctx.session_manager.get_or_create(session_key);
    let persist_hook: Arc<dyn crate::agents::PersistHook> = Arc::new(
        crate::agents::BackendPersistHook::new(ctx.session_backend.clone())
    );
    let mut loop_ = ctx.agent.loop_for_with_persist(session, Some(persist_hook));

    if let Some(rx) = ctx.change_rx.clone() {
        loop_ = loop_.with_change_rx(rx);
    }

    let entry: Arc<TokioMutex<AgentLoop>> = Arc::new(TokioMutex::new(loop_));
    ctx.sessions.insert(session_key.to_string(), entry.clone());
    entry
}

/// Send a response to the configured target channel.
pub async fn send_to_target(ctx: &WebhookContext, target: &str, content: &str) {
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

    let msg = SendMessage {
        content: content.to_string(),
        recipient: String::new(),
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

/// Check if a cron schedule matches the current time.
/// `now` should be a local time as DateTime<Utc> with the timezone offset applied.
pub fn cron_matches(schedule: &cron::Schedule, now: &chrono::DateTime<chrono::Utc>) -> bool {
    let from = *now - chrono::Duration::seconds(61);
    schedule.after(&from).next().is_some_and(|next| {
        let diff = (next - *now).num_seconds().abs();
        diff <= 30
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
pub async fn run_webhook_server(
    ctx: Arc<WebhookContext>,
    config: WebhookConfig,
    jobs: Vec<WebhookJobDef>,
) {
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(port = config.port, error = %e, "webhook: failed to bind");
            return;
        }
    };

    let global_secret = config.secret.clone();
    let jobs = Arc::new(jobs);

    tracing::info!(
        port = config.port,
        routes = jobs.len(),
        "webhook server started"
    );

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
        let global_secret = global_secret.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                let jobs = jobs.clone();
                let global_secret = global_secret.clone();
                async move { handle_request(req, ctx, &jobs, &global_secret).await }
            });

            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                tracing::debug!(error = %e, "webhook: connection error");
            }
        });
    }
}

/// Main request dispatcher — routes to built-in endpoints or custom webhook jobs.
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<WebhookContext>,
    jobs: &[WebhookJobDef],
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    if req.method() != Method::POST {
        return ok_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }

    let path = req.uri().path().to_string();

    // ── Built-in endpoints ────────────────────────────────────────────
    match path.as_str() {
        "/hooks/agent" => return handle_hooks_agent(req, ctx, global_secret).await,
        "/hooks/wake" => return handle_hooks_wake(req, global_secret).await,
        _ => {}
    }

    // ── Custom webhook routes ─────────────────────────────────────────
    let job = match jobs.iter().find(|j| j.path == path) {
        Some(j) => j,
        None => return ok_response(StatusCode::NOT_FOUND, "no webhook at this path"),
    };

    // Extract auth headers before consuming body.
    let sig_header = req
        .headers()
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let auth_header = req
        .headers()
        .get("Authorization")
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

    // Verify auth per-route.
    if let Some(ref secret) = job.secret {
        match job.auth {
            WebhookAuth::Hmac => {
                match sig_header {
                    Some(ref sig) if !verify_hmac_signature(&body_bytes, secret, sig) => {
                        tracing::warn!(path = %path, "webhook: HMAC verification failed");
                        return ok_response(StatusCode::UNAUTHORIZED, "invalid signature");
                    }
                    None => {
                        tracing::warn!(path = %path, "webhook: missing signature header");
                        return ok_response(StatusCode::UNAUTHORIZED, "missing signature");
                    }
                    _ => {}
                }
            }
            WebhookAuth::Bearer => {
                let expected = format!("Bearer {}", secret);
                match auth_header {
                    Some(ref h) if h.as_str() == expected => {}
                    _ => {
                        tracing::warn!(path = %path, "webhook: Bearer auth failed");
                        return ok_response(StatusCode::UNAUTHORIZED, "invalid token");
                    }
                }
            }
        }
    }

    tracing::info!(path = %path, "webhook triggered");

    // Parse payload as JSON for template rendering.
    let payload: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);

    // Render template with payload.
    let prompt = render_template(&job.prompt_template, &payload);

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

/// `POST /hooks/agent` — Run an isolated agent turn.
/// Body: `{"message": "...", "target": "last"}`
async fn handle_hooks_agent(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<WebhookContext>,
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    // Verify global Bearer token.
    if let Some(secret) = global_secret {
        let expected = format!("Bearer {}", secret);
        match req.headers().get("Authorization").and_then(|v| v.to_str().ok()) {
            Some(h) if h == expected => {}
            _ => return ok_response(StatusCode::UNAUTHORIZED, "invalid token"),
        }
    }

    let body_bytes = collect_body(req.into_body()).await?;
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "/hooks/agent: invalid JSON body");
            return ok_response(StatusCode::BAD_REQUEST, "invalid JSON");
        }
    };

    let message = payload.get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if message.is_empty() {
        return ok_response(StatusCode::BAD_REQUEST, "missing 'message' field");
    }

    let target = payload.get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("last");

    tracing::info!(target = target, "/hooks/agent triggered");

    let result = run_scheduled_task(&ctx, "_hooks_agent", &message).await;

    match result {
        Ok(response) => {
            if !response.trim().is_empty() && target != "none" {
                send_to_target(&ctx, target, &response).await;
            }
            ok_response(StatusCode::OK, "ok")
        }
        Err(e) => {
            tracing::warn!(error = %e, "/hooks/agent: agent run failed");
            ok_response(StatusCode::INTERNAL_SERVER_ERROR, "agent error")
        }
    }
}

/// `POST /hooks/wake` — Trigger an immediate heartbeat.
/// Body: `{"text": "..."}`
async fn handle_hooks_wake(
    req: Request<hyper::body::Incoming>,
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    // Verify global Bearer token.
    if let Some(secret) = global_secret {
        let expected = format!("Bearer {}", secret);
        match req.headers().get("Authorization").and_then(|v| v.to_str().ok()) {
            Some(h) if h == expected => {}
            _ => return ok_response(StatusCode::UNAUTHORIZED, "invalid token"),
        }
    }

    let body_bytes = collect_body(req.into_body()).await?;
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "/hooks/wake: invalid JSON body");
            return ok_response(StatusCode::BAD_REQUEST, "invalid JSON");
        }
    };

    let text = payload.get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    tracing::info!(text = %text, "/hooks/wake triggered");

    // TODO: integrate with heartbeat wakeup mechanism (enqueue system event)
    // For now, just acknowledge.
    ok_response(StatusCode::OK, "wake acknowledged")
}

// ── Auth helpers ───────────────────────────────────────────────────────────

/// Verify HMAC-SHA256 signature against the `X-Hub-Signature-256` header value.
fn verify_hmac_signature(body: &[u8], secret: &str, header_value: &str) -> bool {
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

// ── HTTP helpers ───────────────────────────────────────────────────────────

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
    use crate::agents::orchestrator::is_silent_ok;

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
        assert!(!verify_hmac_signature(b"test payload", "secret", "sha256=bad_hex"));
    }

    #[test]
    fn verify_hmac_signature_wrong_length() {
        assert!(!verify_hmac_signature(b"body", "secret", "sha256=abc"));
    }
}
