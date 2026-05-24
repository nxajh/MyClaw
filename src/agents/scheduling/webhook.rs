//! Webhook server and execution helpers.
//!
//! Extracted from scheduler.rs.

use std::sync::Arc;

use dashmap::DashMap;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use http_body_util::Full;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::Mutex;

use crate::agents::runtime::AgentRuntime;
use crate::agents::agent_impl::AgentSession;
use crate::agents::webhook_loader::{WebhookAuth, WebhookJobDef, render_template};
use crate::channels::{Channel, SendMessage};
use crate::config::scheduler::WebhookConfig;
use crate::storage::SessionBackend;

/// Resources needed by the webhook server to run agent tasks.
/// Heartbeat and cron use the Orchestrator event path instead.
pub struct WebhookContext {
    pub runtime: AgentRuntime,
    pub channels: Arc<DashMap<(String, String), Arc<dyn Channel>>>,
    pub sessions: Arc<DashMap<String, Arc<crate::agents::SessionHandle>>>,
    /// Shared session manager — avoids creating throwaway instances per request.
    pub session_manager: Arc<crate::agents::session_manager::SessionManager>,
    /// Backend kept separately for persist hooks (BackendPersistHook needs it).
    pub session_backend: Arc<dyn SessionBackend>,
    pub timezone: String,
    /// Last channel that received a user message (format: "channel_type:account_id").
    pub last_channel: Arc<Mutex<Option<String>>>,
    pub change_rx: Option<tokio::sync::watch::Receiver<crate::agents::ChangeSet>>,
}

/// Create or get an AgentSession for a webhook session and run a prompt.
pub async fn run_scheduled_task(
    ctx: &WebhookContext,
    session_key: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let loop_ = get_or_create_loop(ctx, session_key);
    let mut guard = loop_.lock().await;
    guard.run(prompt, None, None).await
}

fn get_or_create_loop(ctx: &WebhookContext, session_key: &str) -> Arc<TokioMutex<AgentSession>> {
    if let Some(existing) = ctx.sessions.get(session_key) {
        return existing.loop_.clone();
    }

    let session = ctx.session_manager.get_or_create(session_key);
    let persist_hook: Arc<dyn crate::agents::PersistHook> = Arc::new(
        crate::agents::BackendPersistHook::new(ctx.session_backend.clone())
    );
    let mut loop_ = ctx.runtime.create_session(session, Some(persist_hook));

    if let Some(rx) = ctx.change_rx.clone() {
        loop_ = loop_.with_change_rx(rx);
    }

    let loop_arc: Arc<TokioMutex<AgentSession>> = Arc::new(TokioMutex::new(loop_));
    let handle = Arc::new(crate::agents::SessionHandle::new_direct(loop_arc.clone()));
    ctx.sessions.insert(session_key.to_string(), handle);
    loop_arc
}

/// Send a response to the configured target channel.
pub async fn send_to_target(ctx: &WebhookContext, target: &str, content: &str) {
    let (ch_type, acc_id) = match target {
        "none" => return,
        "last" => {
            let last = ctx.last_channel.lock().await.clone();
            match last {
                Some(ref key) => match key.split_once(':') {
                    Some((ch, acc)) => (ch.to_string(), acc.to_string()),
                    None => {
                        tracing::warn!(key = %key, "invalid last_channel format");
                        return;
                    }
                },
                None => {
                    tracing::warn!("no target channel for scheduled response");
                    return;
                }
            }
        }
        name => {
            // Parse "channel:account" or just "channel" (default account)
            match name.split_once(':') {
                Some((ch, acc)) => (ch.to_string(), acc.to_string()),
                None => (name.to_string(), "default".to_string()),
            }
        }
    };

    let channel = match ctx.channels.get(&(ch_type.clone(), acc_id.clone())) {
        Some(ch) => ch.clone(),
        None => {
            tracing::warn!(channel = %ch_type, account = %acc_id, "target channel not found");
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
        inline_buttons: None,
    };

    if let Err(e) = channel.send(&msg).await {
        tracing::warn!(channel = %ch_type, account = %acc_id, err = %e, "failed to send scheduled response");
    }
}

/// Run the webhook HTTP server.
///
/// If `pre_bound` is `Some`, use the pre-bound `SO_REUSEPORT` listener instead
/// of binding a fresh socket.  This is used during hot switch so the new process
/// can accept connections on the same port before the old process releases it.
pub async fn run_webhook_server(
    ctx: Arc<WebhookContext>,
    config: WebhookConfig,
    jobs: Vec<WebhookJobDef>,
    pre_bound: Option<std::net::TcpListener>,
) {
    let listener = if let Some(std_listener) = pre_bound {
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
                tracing::warn!(err = %e, "webhook: accept failed");
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
                tracing::debug!(err = %e, "webhook: connection error");
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
            tracing::warn!(err = %e, "webhook: failed to read body");
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
            tracing::warn!(err = %e, "webhook: agent run failed");
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
            tracing::warn!(err = %e, "/hooks/agent: invalid JSON body");
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
            tracing::warn!(err = %e, "/hooks/agent: agent run failed");
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
            tracing::warn!(err = %e, "/hooks/wake: invalid JSON body");
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
