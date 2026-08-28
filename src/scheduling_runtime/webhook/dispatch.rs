//! Webhook 任务与分发侧（P2 自 webhook.rs 拆出，纯移动）：
//! `run_scheduled_task`（单一定时轮次入口）、`send_to_target`（统一投递分发）、
//! `dispatch_webhook_turn`（fire-and-forget 202，解耦轮次与连接生命周期）、
//! `/hooks/agent` 与 `/hooks/wake` 内置端点。

use super::server::{collect_body, ok_response, pretty_payload};
use super::types::{InflightGuard, WebhookContext, WebhookJobDef};
use super::*;
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
pub(super) async fn dispatch_webhook_turn<F, Fut>(
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
pub(super) async fn handle_hooks_agent(
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
pub(super) async fn handle_hooks_wake(
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
