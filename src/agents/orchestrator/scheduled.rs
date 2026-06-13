//! Scheduled-turn dispatch for the [`Orchestrator`](super::Orchestrator).
//!
//! Heartbeat ticks and cron jobs run as independent spawned tasks that
//! drive a synthetic turn through `SessionContext::process_turn` (the same
//! path as user inbound turns) and then deliver the output to a target
//! channel. Extracted from `orchestrator.rs` so the scheduling concern is
//! isolated from the main event-loop wiring; all access to private
//! Orchestrator state goes through its public accessors.

use std::sync::Arc;

use super::{OrchestratorCtx, is_silent_ok};
use crate::channels::ChannelInboundMessage;

/// Run a scheduled turn for `session_key` with `prompt`, forcing Background
/// run_mode and applying an optional per-call model override. Returns the
/// turn's text; delivery is handled by the caller.
pub(crate) async fn run_scheduled_turn(
    orch: &OrchestratorCtx,
    session_key: &str,
    prompt: &str,
    model_override: Option<String>,
) -> anyhow::Result<String> {
    // Get-or-create the SessionContext, forcing Background run_mode on
    // first materialization. Per-call model override is applied on
    // every invocation so cron jobs that change `model` mid-stream are
    // honored on the next turn.
    let model_for_init = model_override.clone();
    let session_ctx = orch
        .sessions
        .get_or_create_context_with(session_key, move |session| {
            session.session_override.run_mode = Some(crate::config::agent::RunMode::Background);
            if let Some(m) = model_for_init {
                session.session_override.model = Some(m);
            }
        });
    if let Some(ref m) = model_override {
        let mut session = session_ctx.session.lock().await;
        session.session_override.model = Some(m.clone());
    }

    // Synthetic ChannelInboundMessage so process_turn drives the same code path
    // as user inbound turns. No channel — scheduled output is delivered
    // by the caller via `send_to_target_internal` after process_turn returns.
    let inbound = ChannelInboundMessage {
        id: format!("scheduled:{}", session_key),
        sender: crate::channels::MessageSender::new(format!("scheduler:{}", session_key)),
        receiver: crate::channels::MessageReceiver::new(String::new()),
        content: crate::channels::ChannelMessageContent::text(prompt.to_string()),
        timestamp: chrono::Utc::now().timestamp() as u64,
        interruption_scope_id: None,
    };
    let runtime = orch.runtime.clone();
    session_ctx
        .process_turn(inbound, None, runtime)
        .await
        .map(|tr| tr.text)
}

/// Execute a heartbeat turn as an independent spawned task.
pub(crate) async fn run_heartbeat_task(
    orch: Arc<OrchestratorCtx>,
    target_channel: Option<String>,
    target_account: Option<String>,
    prompt: String,
    due: Vec<crate::agents::heartbeat_tasks::HeartbeatTask>,
    mut state: crate::agents::heartbeat_tasks::HeartbeatState,
    state_path: std::path::PathBuf,
) {
    let session_key = format!("_heartbeat_{}", uuid::Uuid::new_v4());
    let result = run_scheduled_turn(&orch, &session_key, &prompt, None).await;

    // Update task state on success.
    if result.is_ok() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        for task in &due {
            state.last_run.insert(task.name.clone(), now_ms);
        }
        state.save(&state_path);
    }

    match result {
        Ok(response) if is_silent_ok(&response, "heartbeat") => {
            tracing::debug!("heartbeat: nothing needs attention");
        }
        Ok(response) if !response.trim().is_empty() => {
            send_to_target_internal(&orch, target_channel, target_account, &response).await;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(err = %e, "heartbeat run failed");
        }
    }
}

/// Execute a cron job turn as an independent spawned task.
pub(crate) async fn run_cron_task(orch: Arc<OrchestratorCtx>, trigger: super::CronTrigger) {
    let super::CronTrigger {
        session_key,
        prompt,
        target_channel,
        target_account,
        job_id,
        model,
    } = trigger;

    let start = std::time::Instant::now();
    let result = run_scheduled_turn(&orch, &session_key, &prompt, model.clone()).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    // Build run record and mark result in scheduler.
    let record = match &result {
        Ok(response) => crate::agents::scheduling::cron_types::RunRecord::now(
            crate::agents::scheduling::cron_types::RunStatus::Ok,
        )
        .with_duration(duration_ms)
        .with_output_preview(response),
        Err(e) => {
            let err_str = e.to_string();
            tracing::warn!(session_key = %session_key, err = %err_str, "cron job failed");
            crate::agents::scheduling::cron_types::RunRecord::now(
                crate::agents::scheduling::cron_types::RunStatus::Error,
            )
            .with_duration(duration_ms)
            .with_error(err_str)
        }
    };

    // Record run result in scheduler (returns failure alert message if needed).
    let failure_alert = if let Some(scheduler) = orch.scheduler.as_ref() {
        scheduler.mark_run_result(&job_id, record)
    } else {
        None
    };

    // Send output to target channel (on success with non-empty output).
    if let Ok(ref response) = result {
        if !response.trim().is_empty() {
            send_to_target_internal(
                &orch,
                target_channel.clone(),
                target_account.clone(),
                response,
            )
            .await;
        }
    }

    // Send failure alert to channel if generated.
    if let Some(alert_msg) = failure_alert {
        tracing::warn!(job_id = %job_id, alert = %alert_msg, "sending failure alert");
        send_to_target_internal(&orch, target_channel, target_account, &alert_msg).await;
    }
}

/// Send a response to the configured target channel (used by heartbeat/cron).
async fn send_to_target_internal(
    orch: &OrchestratorCtx,
    target_channel: Option<String>,
    target_account: Option<String>,
    content: &str,
) {
    let (ch_type, acc_id) = match (target_channel, target_account) {
        (Some(ch), Some(acc)) => (ch, acc),
        (Some(ch), None) => (ch, "default".to_string()),
        (None, _) => {
            // Resolve from scheduler.last_channel (set by handle_channel_event).
            let last = match orch.scheduler.as_ref() {
                Some(s) => s.last_channel.lock().await.clone(),
                None => None,
            };
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
    };

    let channel = match orch.channels.get(&(ch_type.clone(), acc_id.clone())) {
        Some(ch) => ch.clone(),
        None => {
            tracing::warn!(channel = %ch_type, account = %acc_id, "target channel not found");
            return;
        }
    };

    // Resolve recipient from scheduler.last_recipient.
    let recipient = match orch.scheduler.as_ref() {
        Some(s) => s.last_recipient.lock().await.clone().unwrap_or_default(),
        None => String::new(),
    };

    let message = crate::channels::ChannelOutboundMessage {
        receiver: crate::channels::MessageReceiver::new(recipient),
        content: crate::channels::ChannelMessageContent::text(content.to_string()),
        options: Default::default(),
    };
    if let Err(e) = channel.send_message(&message).await {
        tracing::warn!(channel = %ch_type, account = %acc_id, err = %e, "failed to send scheduled response");
    }
}
