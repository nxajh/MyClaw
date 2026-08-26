//! Webhook server & dispatch — split from `scheduler.rs` (#151 Phase 8a).
//!
//! Owns the hyper-based webhook HTTP surface and per-request turn dispatch:
//!   - `WebhookContext` app state (holds the dependency-inverted
//!     [`OrchestratorHook`] wired by the daemon)
//!   - webhook job projection (`WebhookJobDef`/`WebhookFilter`) from the
//!     unified jobs store
//!   - `run_webhook_server` + request handling, HMAC auth, rate-limit /
//!     inflight guards
//!   - template rendering and delivery dispatch (`send_to_target`)
//!
//! Shares the unified jobs store and schedule helpers with `scheduler.rs`
//! (same module tree, `scheduling_runtime`).

use std::sync::Arc;

use anyhow::Context as _;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use tokio::net::TcpListener;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use crate::scheduling_types::cron_types::{
    DeliveryConfig, DeliveryMode, RunRecord, RunStatus,
};
use crate::api::message::{
    Channel, ChannelMessageContent, ChannelOutboundMessage, MessageReceiver,
};
use crate::config::scheduler::WebhookConfig;

use super::scheduler::{
    OrchestratorHook, Scheduler, SharedScheduler, WebhookDef, WebhookFilter, parse_target_string,
};



impl WebhookDef {
    pub fn auth_kind(&self) -> WebhookAuth {
        match self.auth.as_str() {
            "bearer" => WebhookAuth::Bearer,
            _ => WebhookAuth::Hmac,
        }
    }
}

// ── Webhook app state ──────────────────────────────────────────────────────

/// Axum app state for the webhook server. Holds the orchestrator callback
/// hook ([`OrchestratorHook`] — dependency-inverted #151 Phase 3d; previously
/// a direct `Arc<OrchestratorCtx>`), the webhook-specific timezone for cron
/// parsing, and the live scheduler handle.
pub struct WebhookContext {
    /// Orchestrator callbacks (run scheduled turn / channel lookup),
    /// implemented by agents, wired in by the daemon.
    pub hook: Arc<dyn OrchestratorHook>,
    /// Timezone string used for cron evaluation in the webhook server.
    pub timezone: String,
    /// Live scheduler handle — routes are projected per request from the
    /// unified jobs store, so webhook jobs created/updated/removed after
    /// startup are picked up without a daemon restart.
    pub scheduler: SharedScheduler,
}

// ── Webhook channel view + template rendering (migrated from the removed
// webhook_loader.rs per §4 checklist) ───────────────────────────────────────

/// Webhook job definition (server-facing view projected from a JobEntry's
/// `webhook` channel). Route derives from the job name: `POST /hooks/{route}`.
#[derive(Debug, Clone)]
pub struct WebhookJobDef {
    /// Owning job id — FQID `<ns>/job/<uuid>`; used for the `_job_{uuid}`
    /// session key.
    pub id: String,
    /// Route segment (the job's name, URL-safe slug): full route is
    /// `POST /hooks/{route}`.
    pub route: String,
    /// HMAC secret or Bearer token (required — validated at projection).
    pub secret: String,
    /// Auth method: hmac (default) or bearer.
    pub auth: WebhookAuth,
    /// Output delivery configuration (from the job).
    pub delivery: DeliveryConfig,
    /// Prompt template (the job's prompt field), with `{{a.b.c}}`
    /// placeholders rendered from the payload.
    pub prompt_template: String,
    /// Event-type whitelist (optional; non-matching events are ignored).
    pub events: Option<Vec<String>>,
    /// Condition filters (AND semantics, optional).
    pub filters: Option<Vec<WebhookFilter>>,
    /// Disable the automatic full-payload appendix.
    pub payload_off: bool,
}

/// Webhook auth method.
#[derive(Debug, Clone, PartialEq)]
pub enum WebhookAuth {
    /// HMAC-SHA256 via the X-Hub-Signature-256 header.
    Hmac,
    /// Bearer token via the Authorization header.
    Bearer,
}

/// Route-segment validity: lowercase URL-safe slug `[a-z0-9-]`, 1-64 chars.
/// Names are user-facing, so validation is strict (no `_`, no unicode).
pub fn is_route_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Render a prompt template, replacing `{{path.to.field}}` placeholders with
/// values from the JSON payload.
///
/// - `{{issue.title}}` → reads `issue.title` from the payload
/// - `{{commits[0].message}}` → array indexing supported
/// - missing fields render as an empty string
pub fn render_template(template: &str, payload: &serde_json::Value) -> String {
    let mut result = template.to_string();
    let mut start = 0;

    while let Some(open) = result[start..].find("{{") {
        let abs_open = start + open;
        let Some(close) = result[abs_open..].find("}}") else {
            break;
        };
        let abs_close = abs_open + close;

        let key = result[abs_open + 2..abs_close].trim();
        let replacement = match navigate_json_value(payload, key) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::Bool(b)) => b.to_string(),
            Some(serde_json::Value::Null) => String::new(),
            Some(other) => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
            None => String::new(),
        };

        let placeholder_len = abs_close + 2 - abs_open; // includes {{ and }}
        result.replace_range(abs_open..abs_open + placeholder_len, &replacement);
        // Move past the replacement to avoid infinite loops
        start = abs_open + replacement.len();
    }

    result
}

/// Navigate a JSON value by dot-separated path with array index support.
fn navigate_json_value<'a>(
    val: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = val;
    for segment in path.split('.') {
        if let Some(bracket) = segment.find('[') {
            let field = &segment[..bracket];
            if !field.is_empty() {
                current = current.get(field)?;
            }
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

// ── Webhook safety stack (§3.4.1) ──────────────────────────────────────────

/// Per-route + per-IP request guard: rate limit, in-flight cap, delivery-id
/// idempotency cache. Cheap checks first; body-size caps live in
/// `collect_body_capped`.
#[derive(Default)]
pub struct WebhookGuard {
    /// (route, ip) → request timestamps within the sliding 60s window.
    rate: std::sync::Mutex<std::collections::HashMap<(String, String), std::collections::VecDeque<std::time::Instant>>>,
    /// route → in-flight request count (cap 8 per route).
    inflight: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    /// delivery id → first-seen instant (dedupe window 1h).
    deliveries: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// RAII in-flight slot release.
pub struct InflightGuard {
    route: String,
    guard: Arc<WebhookGuard>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.guard.inflight.lock() {
            if let Some(c) = m.get_mut(&self.route) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    m.remove(&self.route);
                }
            }
        }
    }
}

const WEBHOOK_RATE_MAX: usize = 120;
const WEBHOOK_RATE_WINDOW_SECS: u64 = 60;
const WEBHOOK_CONCURRENCY_MAX: usize = 8;
const WEBHOOK_DELIVERY_TTL_SECS: u64 = 3600;
/// Hard body cap for custom webhook routes.
pub const WEBHOOK_BODY_LIMIT: usize = 256 * 1024;
/// Body read timeout.
pub const WEBHOOK_BODY_TIMEOUT_SECS: u64 = 15;
/// Allowed clock skew for V2 timestamped signatures.
const WEBHOOK_V2_MAX_SKEW_SECS: i64 = 300;

impl WebhookGuard {
    /// Sliding-window rate check: ≤120 requests / 60s per (route, ip).
    pub fn check_rate(&self, route: &str, ip: &str) -> bool {
        let now = std::time::Instant::now();
        let Ok(mut m) = self.rate.lock() else { return true };
        let key = (route.to_string(), ip.to_string());
        let win = m.entry(key).or_default();
        let cutoff = now - std::time::Duration::from_secs(WEBHOOK_RATE_WINDOW_SECS);
        while win.front().is_some_and(|t| *t < cutoff) {
            win.pop_front();
        }
        if win.len() >= WEBHOOK_RATE_MAX {
            return false;
        }
        win.push_back(now);
        true
    }

    /// Acquire an in-flight slot; None when the route already has 8 running.
    pub fn acquire(&self, route: &str, guard: Arc<WebhookGuard>) -> Option<InflightGuard> {
        let Ok(mut m) = self.inflight.lock() else { return None };
        let c = m.entry(route.to_string()).or_insert(0);
        if *c >= WEBHOOK_CONCURRENCY_MAX {
            return None;
        }
        *c += 1;
        Some(InflightGuard { route: route.to_string(), guard })
    }

    /// Delivery-id idempotency: true when this id is seen for the first time
    /// within the 1h TTL (and is now recorded); false = duplicate.
    pub fn check_delivery(&self, delivery_id: &str) -> bool {
        let now = std::time::Instant::now();
        let Ok(mut m) = self.deliveries.lock() else { return true };
        m.retain(|_, t| now.duration_since(*t).as_secs() < WEBHOOK_DELIVERY_TTL_SECS);
        if m.contains_key(delivery_id) {
            return false;
        }
        m.insert(delivery_id.to_string(), now);
        true
    }
}


// ── Webhook execution helpers ──────────────────────────────────────────────

/// Execute one webhook turn. Delegates to the orchestrator's shared
/// `run_scheduled_turn` (the single scheduled-turn entry point) — scheduled
/// output (no channel during the turn) is dispatched by the webhook caller via
/// `send_to_target` after this returns.
pub async fn run_scheduled_task(
    ctx: &WebhookContext,
    session_key: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    ctx.hook.run_scheduled_turn(session_key, prompt).await
}

/// Send a response per a resolved delivery config. Single dispatch path
/// for both custom webhook routes (`job.delivery`) and the ad-hoc
/// `/hooks/agent` endpoint (its raw `target` string, converted via
/// `parse_target_string`) — previously the webhook route ignored
/// `delivery` entirely, so `to`/`thread_id` never reached delivery here
/// even though the cron dispatch path already honored `to` (#78).
pub async fn send_to_target(ctx: &WebhookContext, delivery: &DeliveryConfig, content: &str) {
    let Some((ch_type, acc_id, recipient)) = ctx.scheduler.resolve_delivery(delivery).await else {
        return; // mode=None, or Last with no prior channel to reply to yet.
    };

    let channel = match ctx.hook.outbound_channel(&ch_type, &acc_id) {
        Some(ch) => ch,
        None => {
            tracing::warn!(channel = %ch_type, account = %acc_id, "target channel not found");
            return;
        }
    };

    let mut receiver = MessageReceiver::new(recipient.unwrap_or_default());
    if let Some(thread) = &delivery.thread_id {
        receiver = receiver.with_thread(thread.clone());
    }
    let msg = ChannelOutboundMessage {
        receiver,
        content: ChannelMessageContent::text(content),
        options: Default::default(),
    };

    if let Err(e) = channel.send_message(&msg).await {
        tracing::warn!(channel = %ch_type, account = %acc_id, err = %e, "failed to send scheduled response");
    }
}

// ── Webhook ────────────────────────────────────────────────────────────────

/// Run the webhook HTTP server.
///
/// If `pre_bound` is `Some`, use the pre-bound `SO_REUSEPORT` listener instead
/// of binding a fresh socket.  This is used during hot switch so the new process
/// can accept connections on the same port before the old process releases it.
pub async fn run_webhook_server(
    ctx: Arc<WebhookContext>,
    config: WebhookConfig,
    pre_bound: Option<std::net::TcpListener>,
) {
    let listener = if let Some(std_listener) = pre_bound {
        let _ = std_listener.set_nonblocking(true);
        match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(port = config.port, err = %e, "webhook: failed to convert pre-bound listener");
                return;
            }
        }
    } else {
        match tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(port = config.port, err = %e, "webhook: failed to bind");
                return;
            }
        }
    };

    let global_secret = config.secret.clone();
    let guard = Arc::new(WebhookGuard::default());

    tracing::info!(
        port = config.port,
        routes = ctx.scheduler.webhook_jobs().len(),
        "webhook server started (routes projected live from the jobs store)"
    );

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(err = %e, "webhook: accept failed");
                continue;
            }
        };
        let remote_ip = addr.ip().to_string();

        let io = TokioIo::new(stream);
        let ctx = ctx.clone();
        let global_secret = global_secret.clone();
        let guard = guard.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                let global_secret = global_secret.clone();
                let guard = guard.clone();
                let ip = remote_ip.clone();
                async move { handle_request(req, ctx, &global_secret, &guard, ip).await }
            });

            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                tracing::debug!(err = %e, "webhook: connection error");
            }
        });
    }
}

/// Main request dispatcher — routes to built-in endpoints or custom webhook jobs.
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<WebhookContext>,
    global_secret: &Option<String>,
    guard: &Arc<WebhookGuard>,
    remote_ip: String,
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

    // ── Custom webhook routes: /hooks/{name}, name = job name slug ────
    let Some(route_name) = path.strip_prefix("/hooks/") else {
        return ok_response(StatusCode::NOT_FOUND, "no webhook at this path");
    };
    if route_name.contains('/') {
        return ok_response(StatusCode::NOT_FOUND, "no webhook at this path");
    }
    // Live route table: projected from the unified jobs store on every
    // request, so webhook jobs created/updated/removed after startup take
    // effect immediately (no restart, no stale snapshot).
    let jobs = ctx.scheduler.webhook_jobs();
    let job = match jobs.iter().find(|j| j.route == route_name) {
        Some(j) => j,
        None => return ok_response(StatusCode::NOT_FOUND, "no webhook at this path"),
    };

    // ── Safety stack (§3.4.1): method/Content-Type → rate limit →
    // concurrency → (auth → idempotency → body below). Cheap rejections
    // come first. ─────────
    // Content-Type: JSON parses as payload; text/form bodies pass through
    // verbatim as string payloads (§3.4.1); multipart is rejected (we do
    // not decode it and should not read large uploads).
    if !acceptable_content_type(req.headers().get("Content-Type").and_then(|v| v.to_str().ok())) {
        return ok_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json, text/* or application/x-www-form-urlencoded",
        );
    }
    if !guard.check_rate(&job.route, &remote_ip) {
        return ok_response(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
    }
    let _inflight = match guard.acquire(&job.route, Arc::clone(guard)) {
        Some(g) => g,
        None => {
            tracing::warn!(route = %job.route, "webhook: route at concurrency cap");
            return ok_response(StatusCode::SERVICE_UNAVAILABLE, "route busy");
        }
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

    // Event-type header (§3.4.1 fallback chain head; payload fallbacks are
    // consulted after the body is parsed).
    let event_header = req
        .headers()
        .get("X-GitHub-Event")
        .or_else(|| req.headers().get("X-GitLab-Event"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // V2 replay protection: optional timestamped signature binding.
    let ts_header = req
        .headers()
        .get("X-MyClaw-Timestamp")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Delivery id for idempotency (provider fallback chain, §3.4.1).
    let delivery_id = req
        .headers()
        .get("X-GitHub-Delivery")
        .or_else(|| req.headers().get("svix-id"))
        .or_else(|| req.headers().get("X-Request-ID"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Pre-read size gate: Content-Length over the cap rejects without reading.
    if let Some(cl) = req
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if cl > WEBHOOK_BODY_LIMIT {
            return ok_response(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }
    }

    // Collect body bytes (256KB cap + 15s read timeout).
    let body_bytes = match collect_body_capped(req.into_body()).await {
        Ok(b) => b,
        Err(e) if e.to_string().contains("too large") => {
            return ok_response(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }
        Err(e) if e.to_string().contains("timeout") => {
            return ok_response(StatusCode::REQUEST_TIMEOUT, "body read timeout");
        }
        Err(e) => {
            tracing::warn!(err = %e, "webhook: failed to read body");
            return ok_response(StatusCode::BAD_REQUEST, "failed to read body");
        }
    };

    // Verify auth per-route (§3.4.1: secret is required on every custom
    // route — projection already rejected empty secrets, defense here).
    if job.secret.is_empty() {
        return ok_response(StatusCode::INTERNAL_SERVER_ERROR, "route misconfigured");
    }
    match job.auth {
        WebhookAuth::Hmac => {
            // V2 replay protection: when the client sends X-MyClaw-Timestamp,
            // the signature must cover "v2:{ts}:{body}" and the timestamp
            // must be within ±300s. Without the header, plain GitHub-style
            // body signature (V1) applies.
            if let Some(ts) = ts_header.as_deref() {
                let skew = ts
                    .parse::<i64>()
                    .ok()
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|dt| (chrono::Utc::now() - dt).num_seconds().abs())
                    .unwrap_or(i64::MAX);
                if skew > WEBHOOK_V2_MAX_SKEW_SECS {
                    tracing::warn!(route = %job.route, "webhook: V2 timestamp outside allowed skew");
                    return ok_response(StatusCode::UNAUTHORIZED, "stale timestamp");
                }
                let signed = format!("v2:{}:{}", ts, String::from_utf8_lossy(&body_bytes));
                let ok = sig_header
                    .as_deref()
                    .map(|sig| verify_hmac_signature(signed.as_bytes(), &job.secret, sig))
                    .unwrap_or(false);
                if !ok {
                    tracing::warn!(route = %job.route, "webhook: V2 HMAC verification failed");
                    return ok_response(StatusCode::UNAUTHORIZED, "invalid signature");
                }
            } else {
                match sig_header {
                    Some(ref sig) if !verify_hmac_signature(&body_bytes, &job.secret, sig) => {
                        tracing::warn!(route = %job.route, "webhook: HMAC verification failed");
                        return ok_response(StatusCode::UNAUTHORIZED, "invalid signature");
                    }
                    None => {
                        tracing::warn!(route = %job.route, "webhook: missing signature header");
                        return ok_response(StatusCode::UNAUTHORIZED, "missing signature");
                    }
                    _ => {}
                }
            }
        }
        WebhookAuth::Bearer => {
            let expected = format!("Bearer {}", job.secret);
            match auth_header {
                Some(ref h) if h.as_str() == expected => {}
                _ => {
                    tracing::warn!(route = %job.route, "webhook: Bearer auth failed");
                    return ok_response(StatusCode::UNAUTHORIZED, "invalid token");
                }
            }
        }
    }

    // Idempotency: duplicate delivery ids are acknowledged and dropped.
    if let Some(did) = delivery_id.as_deref() {
        if !guard.check_delivery(did) {
            tracing::info!(route = %job.route, delivery_id = %did, "webhook: duplicate delivery, ignored");
            return ok_response(StatusCode::OK, "duplicate");
        }
    }

    tracing::info!(route = %job.route, "webhook triggered");

    // Parse payload: JSON bodies become objects; anything else is passed
    // through as a plain string (§3.4.1 — no silent Null).
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(
            String::from_utf8_lossy(&body_bytes).to_string(),
        ),
    };

    // Event whitelist + condition filters (§3.4.1). Non-matching requests
    // are acknowledged 200 "ignored" and leave a Skipped history entry.
    let event_type = extract_event_type(event_header.as_deref(), &payload);
    let ignore_reason = if let Some(events) = job.events.as_ref() {
        let et = event_type.as_deref().unwrap_or("");
        if !events.iter().any(|e| e == et) {
            Some(format!("event '{}' not in whitelist", et))
        } else {
            None
        }
    } else {
        None
    }
    .or_else(|| {
        job.filters
            .as_ref()
            .filter(|fs| !fs.iter().all(|f| filter_matches(f, &payload)))
            .map(|_| "filters not satisfied".to_string())
    });
    if let Some(reason) = ignore_reason {
        tracing::info!(route = %job.route, reason = %reason, "webhook: ignored");
        {
            let sched = &ctx.scheduler;
            sched.record_webhook_run(
                &job.id,
                RunRecord {
                    run_at: chrono::Utc::now().to_rfc3339(),
                    status: RunStatus::Skipped,
                    trigger: Some("webhook".to_string()),
                    error: Some(reason),
                    payload: Some(pretty_payload(&payload, 8192)),
                    ..Default::default()
                },
            );
        }
        return ok_response(StatusCode::OK, "ignored");
    }

    // Render template with payload, then append the full payload for context
    // (§3.4.1: full-payload default, 4000-char truncation, {{payload}} and
    // {{event_type}} reserved placeholders).
    let mut prompt = render_template(&job.prompt_template, &payload);
    if prompt.contains("{{payload}}") {
        prompt = prompt.replace("{{payload}}", &pretty_payload(&payload, 4000));
    } else if let Some(et) = event_type.as_deref() {
        prompt = prompt.replace("{{event_type}}", et);
    }
    if !job.payload_off && !job.prompt_template.contains("{{payload}}") {
        let appendix = pretty_payload(&payload, 4000);
        if !appendix.is_empty() {
            prompt.push_str("\n\n--- webhook payload ---\n");
            prompt.push_str(&appendix);
        }
    }

    let session_key = format!(
        "_job_{}",
        crate::ids::bare_dir_name(&job.id)
    );

    // Fire-and-forget dispatch: the turn runs in a background task so the
    // HTTP response does not depend on LLM latency. Awaiting the turn inside
    // the hyper service future couples its lifetime to the client connection
    // — when the peer disconnects (GitHub webhooks time out at 10s), hyper
    // drops the connection task and the turn is cancelled mid-flight with
    // no run record. 202 acknowledges acceptance; the turn's outcome lands
    // in the job's run log (`record_webhook_run`) and `send_to_target`.
    dispatch_webhook_turn(
        Arc::clone(&ctx),
        job.clone(),
        session_key,
        prompt,
        payload,
        _inflight,
        |ctx, session_key, prompt| async move {
            run_scheduled_task(&ctx, &session_key, &prompt).await
        },
    )
    .await
}

/// Fire-and-forget dispatch of a webhook turn: spawn the turn in a
/// background task and return 202 immediately, so the turn's lifetime is
/// decoupled from the client connection.
///
/// `run` executes the turn (production: [`run_scheduled_task`]); it is a
/// parameter so tests can inject a controllable future and assert that the
/// 202 returns without awaiting the turn — a regression back to a
/// synchronous `.await` would re-couple the turn to the connection (the
/// bug PR #74 fixes) and fail
/// `webhook_dispatch_returns_202_without_awaiting_turn`. The turn's
/// outcome lands in the job's run log (`record_webhook_run`) and, when
/// non-empty, `send_to_target`. The inflight slot is held for the whole
/// turn and released only after the record is written.
async fn dispatch_webhook_turn<F, Fut>(
    ctx: Arc<WebhookContext>,
    job: WebhookJobDef,
    session_key: String,
    prompt: String,
    payload: serde_json::Value,
    inflight: InflightGuard,
    run: F,
) -> anyhow::Result<Response<Full<Bytes>>>
where
    F: FnOnce(Arc<WebhookContext>, String, String) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<String>> + Send + 'static,
{
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let result = run(Arc::clone(&ctx), session_key, prompt.clone()).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        // History record (§3.4: trigger field + webhook audit fields).
        {
            let sched = &ctx.scheduler;
            let mut record = RunRecord::now(match &result {
                Ok(_) => RunStatus::Ok,
                Err(_) => RunStatus::Error,
            });
            record.trigger = Some("webhook".to_string());
            record.duration_ms = duration_ms;
            record.payload = Some(pretty_payload(&payload, 8192));
            record.prompt_head = Some(prompt.chars().take(512).collect());
            if let Ok(response) = &result {
                record = record.with_output_preview(response);
            }
            if let Err(e) = &result {
                record = record.with_error(e.to_string());
            }
            sched.record_webhook_run(&job.id, record);
        }

        match result {
            Ok(response) => {
                if !response.trim().is_empty() {
                    send_to_target(&ctx, &job.delivery, &response).await;
                }
            }
            Err(e) => {
                tracing::warn!(route = %job.route, err = %e, "webhook: agent run failed");
            }
        }
        drop(inflight);
    });

    ok_response(StatusCode::ACCEPTED, "accepted")
}

/// `POST /hooks/agent` — Run an isolated agent turn.
/// Body: `{"message": "...", "target": "last"}`
async fn handle_hooks_agent(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<WebhookContext>,
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    // JSON API: cheap Content-Type rejection before auth/body work.
    let ct_json = req
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false);
    if !ct_json {
        return ok_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Content-Type must be application/json");
    }

    // Verify global Bearer token.
    if let Some(secret) = global_secret {
        let expected = format!("Bearer {}", secret);
        match req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
        {
            Some(h) if h == expected => {}
            _ => return ok_response(StatusCode::UNAUTHORIZED, "invalid token"),
        }
    }

    let body_bytes = collect_body(req.into_body()).await?;
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(err = %e, "/hooks/agent: invalid JSON body");
            return ok_response(StatusCode::BAD_REQUEST, "invalid JSON");
        }
    };

    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if message.is_empty() {
        return ok_response(StatusCode::BAD_REQUEST, "missing 'message' field");
    }

    let target = payload
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("last");

    tracing::info!(target = target, "/hooks/agent triggered");

    // Fire-and-forget dispatch (same rationale as custom webhook routes):
    // awaiting the turn inside the hyper service future ties it to the
    // client connection — a peer disconnect cancels the turn mid-flight.
    // 202 acknowledges acceptance; the response (if any) goes to `target`.
    let bg_ctx = Arc::clone(&ctx);
    let bg_delivery = parse_target_string(target);
    tokio::spawn(async move {
        match run_scheduled_task(&bg_ctx, "_hooks_agent", &message).await {
            Ok(response) => {
                if !response.trim().is_empty() {
                    send_to_target(&bg_ctx, &bg_delivery, &response).await;
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "/hooks/agent: agent run failed");
            }
        }
    });

    ok_response(StatusCode::ACCEPTED, "accepted")
}

/// `POST /hooks/wake` — Wake endpoint (kept for URL contract; the old
/// heartbeat wakeup mechanism was removed with the heartbeat system).
/// Body: `{"text": "..."}`
async fn handle_hooks_wake(
    req: Request<hyper::body::Incoming>,
    global_secret: &Option<String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    // Verify global Bearer token.
    if let Some(secret) = global_secret {
        let expected = format!("Bearer {}", secret);
        match req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
        {
            Some(h) if h == expected => {}
            _ => return ok_response(StatusCode::UNAUTHORIZED, "invalid token"),
        }
    }

    let body_bytes = collect_body(req.into_body()).await?;
    let payload: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(err = %e, "/hooks/wake: invalid JSON body");
            return ok_response(StatusCode::BAD_REQUEST, "invalid JSON");
        }
    };

    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    tracing::info!(text = %text, "/hooks/wake triggered");

    // TODO: enqueue a system event to wake the agent loop
    // For now, just acknowledge.
    ok_response(StatusCode::OK, "wake acknowledged")
}

// ── Auth helpers ───────────────────────────────────────────────────────────

/// Extract the event type: header fallback chain first (X-GitHub-Event →
/// X-GitLab-Event, captured before body consumption), then payload fields
/// (`event_type` → `type`) — hermes 5-level chain, §3.4.1.
fn extract_event_type(event_header: Option<&str>, payload: &serde_json::Value) -> Option<String> {
    if let Some(h) = event_header.filter(|h| !h.is_empty()) {
        return Some(h.to_string());
    }
    payload
        .get("event_type")
        .or_else(|| payload.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Evaluate a single webhook filter condition against the payload.
/// AND semantics across the filter list; `not` negates the match.
fn filter_matches(
    f: &WebhookFilter,
    payload: &serde_json::Value,
) -> bool {
    let mut cur = payload;
    for seg in f.field.split('.') {
        cur = match cur.get(seg) {
            Some(v) => v,
            None => return f.not, // missing field: only a `not` filter passes
        };
    }
    // Strings compare directly; numbers/bools against their string form.
    let actual = match cur {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => return f.not,
    };
    let matched = if let Some(eq) = f.equals.as_deref() {
        actual == eq
    } else if let Some(re) = f.matches.as_deref() {
        regex::Regex::new(re).map(|r| r.is_match(&actual)).unwrap_or(false)
    } else {
        true
    };
    matched != f.not
}

/// Pretty-print a payload for prompt context, truncated to `max_chars`.
fn pretty_payload(payload: &serde_json::Value, max_chars: usize) -> String {
    let s = serde_json::to_string_pretty(payload).unwrap_or_default();
    if s.chars().count() <= max_chars {
        s
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}\n…[truncated]", truncated)
    }
}

/// Content-Type gate (§3.4.1 first stack step): application/json parses as
/// the payload; text/* and form-urlencoded bodies pass through verbatim as
/// string payloads; anything else (missing, multipart, …) is rejected cheap
/// before any body read.
fn acceptable_content_type(ct: Option<&str>) -> bool {
    let Some(ct) = ct else { return false };
    let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    mime == "application/json" || mime.starts_with("text/") || mime == "application/x-www-form-urlencoded"
}

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

/// Capped body read for custom webhook routes (§3.4.1): 256KB hard limit,
/// 15s read timeout. `Err(anyhow!("too large"))` maps to 413 upstream.
async fn collect_body_capped<B>(body: B) -> anyhow::Result<Bytes>
where
    B: hyper::body::Body,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let fut = async {
        use http_body_util::BodyExt;
        let mut body = std::pin::pin!(body);
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        while let Some(frame) = body.as_mut().frame().await {
            let frame = frame?;
            if let Ok(mut data) = frame.into_data() {
                use bytes::Buf;
                while data.has_remaining() {
                    let chunk = data.chunk();
                    if buf.len() + chunk.len() > WEBHOOK_BODY_LIMIT {
                        anyhow::bail!("too large");
                    }
                    let n = chunk.len();
                    buf.extend_from_slice(chunk);
                    data.advance(n);
                }
            }
        }
        Ok(Bytes::from(buf))
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(WEBHOOK_BODY_TIMEOUT_SECS),
        fut,
    )
    .await
    {
        Ok(res) => res,
        Err(_) => anyhow::bail!("body read timeout"),
    }
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
    use super::scheduler::tests::{test_entry, test_scheduler};
    use crate::scheduling_runtime::scheduler::JobEntry;
    use crate::scheduling_types::cron_types::ScheduleSpec;
    use std::time::Duration;

    /// Moved from `agents::orchestrator::is_silent_ok` (#151 Phase 3d, SCC
    #[test]
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

}
