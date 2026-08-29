//! Scheduled-turn dispatch for the [`Orchestrator`](super::Orchestrator).
//!
//! Heartbeat ticks and cron jobs run as independent spawned tasks that
//! drive a synthetic turn through `SessionContext::process_turn` (the same
//! path as user inbound turns) and then deliver the output to a target
//! channel. Extracted from `orchestrator.rs` so the scheduling concern is
//! isolated from the main event-loop wiring; all access to private
//! Orchestrator state goes through its public accessors.

use std::sync::Arc;

use super::OrchestratorCtx;
use crate::api::message::ChannelInboundMessage;

/// Run a scheduled turn for `session_key` with `prompt`, forcing Background
/// run_mode on the synthetic message and applying an optional per-call model
/// override. Returns the turn's text; delivery is handled by the caller.
pub(crate) async fn run_scheduled_turn(
    orch: &OrchestratorCtx,
    session_key: &str,
    prompt: &str,
    model_override: Option<String>,
    creator: Option<String>,
) -> anyhow::Result<String> {
    // Get-or-create the SessionContext, applying the per-call model override
    // on every invocation so cron jobs that change `model` mid-stream are
    // honored on the next turn.
    //
    // RFC channel-role-split §1.4-2: no session-level
    // `session_override.run_mode = Background` write — run_mode now travels
    // on the synthetic message below (turn-scoped). Writing it here poisoned
    // the user's session when an Inject-mode cron reused it.
    let model_for_init = model_override.clone();
    let session_ctx = orch
        .sessions
        .get_or_create_context_with(session_key, move |session| {
            if let Some(m) = model_for_init {
                session.session_override.model = Some(m);
            }
        });
    if let Some(ref m) = model_override {
        let mut session = session_ctx.session.lock().await;
        session.session_override.model = Some(m.clone());
    }

    // #101 P2: attribute the turn to the job creator. The session's
    // owner_fqid is only "real" when load_session resolved a bound
    // routing key; job session keys (`_job_*`) are unbound and resolve
    // to themselves, and freshly-created sessions default to empty —
    // overwrite exactly those placeholders with the creator FQID.
    // Inject-mode turns reuse the user's session, whose owner_fqid is
    // already correctly resolved (≠ session_key), so it is never touched.
    if let Some(creator) = creator {
        let mut session = session_ctx.session.lock().await;
        if session.owner_fqid.is_empty() || session.owner_fqid == session_key {
            tracing::debug!(
                session_key = %session_key,
                creator = %creator,
                "scheduled turn: attributing session to job creator"
            );
            session.owner_fqid = creator;
        }
    }

    // Synthetic ChannelInboundMessage so process_turn drives the same code path
    // as user inbound turns. No channel — scheduled output is delivered
    // by the caller via `send_to_target_internal` after process_turn returns.
    // RFC channel-role-split §1.1: run_mode = Background marks the turn as
    // headless (ask_user errors instead of hanging; prompt rules switch to
    // the autonomous section for THIS turn only).
    let inbound = ChannelInboundMessage {
        id: format!("scheduled:{}", session_key),
        sender: crate::api::message::MessageSender::new(format!("scheduler:{}", session_key)),
        receiver: crate::api::message::MessageReceiver::new(String::new()),
        content: crate::api::message::ChannelMessageContent::text(prompt.to_string()),
        timestamp: chrono::Utc::now().timestamp() as u64,
        interruption_scope_id: None,
        silenced_override: None,
        run_mode: crate::config::agent::RunMode::Background,
    };
    let runtime = orch.runtime.clone();
    session_ctx
        .process_turn(inbound, None, runtime)
        .await
        .map(|tr| tr.text)
}

/// Execute a cron job turn as an independent spawned task.
pub(crate) async fn run_cron_task(orch: Arc<OrchestratorCtx>, trigger: super::CronTrigger) {
    let super::CronTrigger {
        session_key,
        prompt,
        target_channel,
        target_account,
        target_recipient,
        target_thread,
        delivery_suppressed,
        job_id,
        model,
        context_policy,
        creator,
    } = trigger;

    let start = std::time::Instant::now();

    // Choose session key based on context policy.
    // Inject: resolve the user's active session routing key and inject into it.
    // Isolated: use the cron job's session key (each job has its own session).
    let effective_session_key = if context_policy == crate::config::scheduler::ContextPolicy::Inject
    {
        // Resolve the user's routing key from last_channel + last_recipient.
        let routing_key = resolve_user_routing_key(
            &orch,
            target_channel.as_deref(),
            target_recipient.as_deref(),
        )
        .await;
        match routing_key {
            Some(key) => {
                tracing::debug!(job_id = %job_id, routing_key = %key, "cron job injecting into user session");
                key
            }
            None => {
                tracing::warn!(job_id = %job_id, "cron job Inject mode but no user routing key found, falling back to Isolated");
                session_key
            }
        }
    } else {
        tracing::debug!(job_id = %job_id, session_key = %session_key, "cron job running in Isolated mode");
        session_key
    };

    let result = run_scheduled_turn(
        &orch,
        &effective_session_key,
        &prompt,
        model.clone(),
        creator,
    )
    .await;
    let duration_ms = start.elapsed().as_millis() as u64;

    // Build run record and mark result in scheduler.
    let record = match &result {
        Ok(response) => crate::scheduling_types::cron_types::RunRecord::now(
            crate::scheduling_types::cron_types::RunStatus::Ok,
        )
        .with_duration(duration_ms)
        .with_output_preview(response)
        .with_trigger("cron"),
        Err(e) => {
            let err_str = e.to_string();
            tracing::warn!(session_key = %effective_session_key, err = %err_str, "cron job failed");
            crate::scheduling_types::cron_types::RunRecord::now(
                crate::scheduling_types::cron_types::RunStatus::Error,
            )
            .with_duration(duration_ms)
            .with_error(err_str)
            .with_trigger("cron")
        }
    };

    // Record run result in scheduler (returns failure alert message if needed).
    let failure_alert = if let Some(scheduler) = orch.scheduler.as_ref() {
        scheduler.mark_run_result(&job_id, record)
    } else {
        None
    };

    // Send output to target channel (on success with non-empty output).
    // RFC channel-role-split §1.4-1: `send_to_target_internal` is the ONLY
    // delivery exit for scheduled turns (run_scheduled_turn passes
    // channel=None, so the turn itself delivers nothing). The former
    // ffa0317 `should_send = !is_active` gate is removed — it starved
    // Inject-mode crons doubly (turn had no channel AND send was skipped
    // because the user's session was active), leaving output only in the
    // session history (2026-08-14 weekly digest never delivered).
    // #78: `delivery_suppressed` (mode=None) skips this — the turn still
    // ran, but nothing was configured to receive its output.
    if !delivery_suppressed {
        if let Ok(ref response) = result {
            if !response.trim().is_empty() {
                send_to_target_internal(
                    &orch,
                    target_channel.clone(),
                    target_account.clone(),
                    target_recipient.clone(),
                    target_thread.clone(),
                    response,
                )
                .await;
            }
        }
    }

    // Send failure alert to channel if generated. Unconditional even under
    // mode=None: a silently-configured job's failures should still surface
    // somewhere via the last-known channel fallback — matches pre-#78
    // behavior (the cron path never actually enforced "none" here).
    if let Some(alert_msg) = failure_alert {
        tracing::warn!(job_id = %job_id, alert = %alert_msg, "sending failure alert");
        send_to_target_internal(
            &orch,
            target_channel,
            target_account,
            target_recipient,
            target_thread,
            &alert_msg,
        )
        .await;
    }
}

/// Resolve the user's routing key from target channel and recipient.
/// Returns a routing key in the format "channel:account:sender".
async fn resolve_user_routing_key(
    orch: &OrchestratorCtx,
    target_channel: Option<&str>,
    target_recipient: Option<&str>,
) -> Option<String> {
    // Resolve channel:account from target or last_channel.
    let (ch_type, acc_id) = match target_channel {
        Some(ch) => {
            // If target_channel is specified, use "default" as account.
            (ch.to_string(), "default".to_string())
        }
        None => {
            let last = orch.scheduler.as_ref()?.last_channel.lock().await.clone()?;
            match last.split_once(':') {
                Some((ch, acc)) => (ch.to_string(), acc.to_string()),
                None => return None,
            }
        }
    };

    // Resolve recipient from target or last_recipient.
    let recipient = match target_recipient {
        Some(r) => r.to_string(),
        None => orch
            .scheduler
            .as_ref()?
            .last_recipient
            .lock()
            .await
            .clone()?,
    };

    Some(format!("{}:{}:{}", ch_type, acc_id, recipient))
}

/// Send a response to the configured target channel (used by cron/webhook).
async fn send_to_target_internal(
    orch: &OrchestratorCtx,
    target_channel: Option<String>,
    target_account: Option<String>,
    target_recipient: Option<String>,
    target_thread: Option<String>,
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

    // Resolve recipient: explicit target_recipient first, else last_recipient.
    let recipient = if let Some(r) = target_recipient {
        r
    } else {
        match orch.scheduler.as_ref() {
            Some(s) => s.last_recipient.lock().await.clone().unwrap_or_default(),
            None => String::new(),
        }
    };

    let mut receiver = crate::api::message::MessageReceiver::new(recipient);
    if let Some(thread) = target_thread {
        receiver = receiver.with_thread(thread);
    }
    let message = crate::api::message::ChannelOutboundMessage {
        receiver,
        content: crate::api::message::ChannelMessageContent::text(content.to_string()),
        options: Default::default(),
    };
    if let Err(e) = channel.send_message(&message).await {
        tracing::warn!(channel = %ch_type, account = %acc_id, err = %e, "failed to send scheduled response");
    }
}

/// Execute an idle-time memory distillation pass as an independent spawned
/// task. Pre-flight checks (pending memories, backoff) run inline; the LLM
/// pass itself runs inside `run_memory_distill`.
pub(crate) async fn run_distill_task(orch: Arc<OrchestratorCtx>) {
    use crate::agents::memory_distill::{
        DistillState, has_pending_user_memories, run_memory_distill,
    };

    let memory_root = orch.runtime.defaults.prompt.memory_root.clone();
    if memory_root.is_empty() {
        tracing::warn!("memory_distill: memory_root not configured, skipped");
        return;
    }

    // Backoff: after 3 consecutive failures, pause for 2 hours.
    // P1-B2: distill state is runtime state — sibling of the memory root
    // ({base_dir}/state/memory/distill.json), not inside the memory pool.
    let base_dir = &orch.runtime.defaults.prompt.base_dir;
    let state_dir = if base_dir.is_empty() {
        std::path::PathBuf::from("state/memory")
    } else {
        crate::config::memory_distill_state_dir(std::path::Path::new(base_dir))
    };
    let mut state = DistillState::load(&state_dir);
    if state.in_backoff() {
        tracing::warn!(
            failures = state.consecutive_failures,
            "memory_distill: in backoff, skipped"
        );
        return;
    }

    // Only distill when at least one user memory changed since the last pass.
    if !has_pending_user_memories(&memory_root, state.last_distill_ts.as_deref()) {
        tracing::debug!("memory_distill: no new user memories, skipped");
        return;
    }

    // Resolve the chat provider + default model (same routing as agent turns).
    let (provider, model_id) = match orch
        .runtime
        .providers
        .get_chat_provider(crate::providers::Capability::Chat)
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(err = %e, "memory_distill: no chat provider available");
            return;
        }
    };

    // Tool specs for the restricted memory tool set.
    let tool_specs: Vec<crate::providers::capability_chat::ToolSpec> = [
        "memory_list",
        "memory_view",
        "memory_search",
        "memory_manage",
    ]
    .iter()
    .filter_map(|name| orch.runtime.tools.get(name))
    .map(|t| {
        let s = t.spec();
        crate::providers::capability_chat::ToolSpec {
            name: s.name,
            description: Some(s.description),
            input_schema: s.parameters,
        }
    })
    .collect();
    if tool_specs.len() != 4 {
        tracing::warn!(
            specs = tool_specs.len(),
            "memory_distill: memory tools incomplete, skipped"
        );
        return;
    }

    let input = crate::agents::memory_distill::DistillInput {
        model_id,
        provider,
        tool_specs,
        tool_registry: Arc::clone(&orch.runtime.tools),
        memory_root,
        registry: Arc::clone(&orch.runtime.providers)
            as Arc<dyn crate::providers::ProviderRegistry>,
    };

    let result = run_memory_distill(input).await;
    let success = result.is_ok();
    state.record_attempt(success, &state_dir);
    if success {
        tracing::info!(
            files_written = result.unwrap_or(0),
            "memory_distill: pass recorded"
        );
    } else {
        tracing::warn!(
            failures = state.consecutive_failures,
            "memory_distill: pass failed"
        );
    }
}

/// Execute an idle-time skill internalization proposer pass (RFC #101 §2.4)
/// as an independent spawned task. Pre-flight checks (pending user-layer
/// skills, backoff) run inline; the LLM classification pass itself runs
/// inside `run_skill_proposer`.
pub(crate) async fn run_proposer_task(orch: Arc<OrchestratorCtx>) {
    use crate::agents::skill_proposer::{
        ProposerState, has_pending_user_skills, run_skill_proposer,
    };

    let base_dir = &orch.runtime.defaults.prompt.base_dir;
    if base_dir.is_empty() {
        tracing::warn!("skill_proposer: base_dir not configured, skipped");
        return;
    }
    let base = std::path::Path::new(base_dir);
    let users_dir = crate::config::users_root(base);
    let state_dir = crate::config::skill_proposer_state_dir(base);

    let mut state = ProposerState::load(&state_dir);
    if state.in_backoff() {
        tracing::warn!(
            failures = state.consecutive_failures,
            "skill_proposer: in backoff, skipped"
        );
        return;
    }

    if !has_pending_user_skills(&users_dir, state.last_propose_ts.as_deref()) {
        tracing::debug!("skill_proposer: no changed user-layer skills, skipped");
        return;
    }

    // Resolve the chat provider + default model (same routing as agent turns).
    let (provider, model_id) = match orch
        .runtime
        .providers
        .get_chat_provider(crate::providers::Capability::Chat)
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(err = %e, "skill_proposer: no chat provider available");
            return;
        }
    };

    let input = crate::agents::skill_proposer::ProposerInput {
        model_id,
        provider,
        users_dir,
        skills_root: base.join("skills"), // same as AppConfig::skills_root()
        proposals_dir: crate::config::skill_proposals_dir(base),
        classified: state.classified_shas.clone(),
    };

    let result = run_skill_proposer(input).await;
    let success = result.is_ok();
    if let Ok((_, _, classified)) = &result {
        // Single writer: merge the pass's sha index into the one persistent
        // state instance, then record the attempt — one save, no stale copy
        // overwriting the index (bug seen on first production pass).
        state.classified_shas.extend(classified.clone());
    }
    state.record_attempt(success, &state_dir);
    if let Ok((promoted, tier_b, _)) = &result {
        tracing::info!(
            promoted = promoted,
            tier_b = tier_b,
            "skill_proposer: pass recorded"
        );
    } else {
        tracing::warn!(
            failures = state.consecutive_failures,
            "skill_proposer: pass failed"
        );
    }
}
