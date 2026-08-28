//! Webhook HTTP 服务器侧（P2 自 webhook.rs 拆出，纯移动）：
//! `run_webhook_server`（含 SO_REUSEPORT 预绑定热切换）、`handle_request`
//! （内置端点路由 / 实时任务投影 / 安全栈 / HMAC+Bearer 校验 / 事件白名单与
//! 条件过滤器）、`pretty_payload`、`acceptable_content_type`、
//! `verify_hmac_signature`、body 读取（`collect_body` / `collect_body_capped`）
//! 与 `ok_response`。

use super::dispatch::{
    dispatch_webhook_turn, handle_hooks_agent, handle_hooks_wake, run_scheduled_task,
};
use super::types::{
    WebhookAuth, WebhookContext, WebhookGuard, WEBHOOK_BODY_LIMIT, WEBHOOK_BODY_TIMEOUT_SECS,
    WEBHOOK_V2_MAX_SKEW_SECS, extract_event_type, filter_matches, render_template,
};
use super::*;
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

/// Pretty-print a payload for prompt context, truncated to `max_chars`.
pub(super) fn pretty_payload(payload: &serde_json::Value, max_chars: usize) -> String {
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
pub(super) fn verify_hmac_signature(body: &[u8], secret: &str, header_value: &str) -> bool {
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
pub(super) async fn collect_body<B>(body: B) -> anyhow::Result<Bytes>
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

pub(super) fn ok_response(status: StatusCode, body: &str) -> anyhow::Result<Response<Full<Bytes>>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .map_err(Into::into)
}
