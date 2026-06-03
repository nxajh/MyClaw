//! Orchestrator — Application Service that connects channels and per-turn
//! `Agent::run` dispatch.
//!
//! This is the core Application Service in DDD terms:
//! - Receives messages from Interface (Channel) adapters
//! - Coordinates Domain objects (Agent, Session, Tools)
//! - Routes responses back through Interface adapters
//!
//! Assembly of Infrastructure components (Registry, Providers, Tools, Storage)
//! is done in the Composition Root (orchestration/orchestrator main.rs + daemon.rs),
//! not here. This struct receives fully-assembled components via its constructor.

mod delegation;
mod inbound;
mod recovery;
mod scheduled;
mod turn;
pub mod ctx;
pub mod event;
pub mod key;

pub use ctx::OrchestratorCtx;
pub use event::OrchestratorEvent;

use anyhow::Context;
use crate::agents::delegation::DelegationEvent;
use crate::agents::DelegationCoordinator;
use crate::channels::{Channel, ChannelMessage};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agents::session::SessionManager;
use scheduled::{run_cron_task, run_heartbeat_task};

/// Buffer size for the unified `OrchestratorEvent` channel. Fixed (not
/// config-exposed) on purpose: this is an internal backpressure bound, not
/// an operator-tunable knob. The meaningful knobs (tool timeout, compaction
/// threshold, loop-breaker limits) live in `[agent]`/`[context]` config.
const CHANNEL_QUEUE_SIZE: usize = 100;

/// Events from the Scheduler (heartbeat ticks, cron triggers).
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SchedulerEvent {
    /// Heartbeat tick — run agent with heartbeat prompt.
    Heartbeat {
        target_channel: Option<String>,
        target_account: Option<String>,
    },
    /// Cron job matched — run agent with specific prompt.
    Cron {
        session_key: String,
        prompt: String,
        target_channel: Option<String>,
        target_account: Option<String>,
        job_id: String,
        delivery: Option<crate::agents::scheduling::cron_types::DeliveryConfig>,
        enabled_tools: Option<Vec<String>>,
        disabled_tools: Option<Vec<String>>,
        model: Option<String>,
        provider: Option<String>,
    },
}

/// Type alias for the channel message sender.
pub type ChannelMsgSender = mpsc::Sender<((String, String), ChannelMessage)>;

/// Orchestrator — Application Service for message routing and session lifecycle.
///
/// Coordinates the flow: Channel → Session → Agent::run → Channel.
/// Does NOT depend on any Infrastructure concrete types.
pub struct Orchestrator {
    /// Shared dependency bundle (channels, sessions, runtime, ask, scheduler,
    /// delegator). Cloned into spawned tasks that must outlive a single turn.
    ctx: Arc<OrchestratorCtx>,
    /// The message receiver, owned and consumed by run().
    #[allow(clippy::type_complexity)]
    msg_rx: Arc<TokioMutex<Option<mpsc::Receiver<((String, String), ChannelMessage)>>>>,
    /// Listener task handles — taken and awaited on shutdown.
    /// Wrapped in a TokioMutex so `shutdown_listeners(&self)` can drain
    /// them without requiring `&mut self` (which would block the
    /// `Arc<Self>` sharing pattern that scheduler dispatch relies on).
    listener_handles: Arc<TokioMutex<Vec<JoinHandle<()>>>>,
    /// Delegation event receiver.
    delegation_rx: Arc<TokioMutex<Option<mpsc::Receiver<DelegationEvent>>>>,
    /// Scheduler event receiver (heartbeat ticks, cron triggers).
    scheduler_rx: Arc<TokioMutex<Option<mpsc::Receiver<SchedulerEvent>>>>,
}

/// Return `true` if the session history ends with an incomplete tool execution:
/// either trailing tool-result messages, or assistant tool_calls whose IDs have
/// no matching tool-result — indicating the turn was interrupted mid-execution.
pub(super) fn history_has_incomplete_turn(history: &[crate::providers::capability_chat::ChatMessage]) -> bool {
    let mut completed_ids = std::collections::HashSet::new();
    let mut has_trailing_tools = false;
    let mut found_pending = false;
    for msg in history.iter().rev() {
        if msg.role == "tool" {
            if let Some(ref id) = msg.tool_call_id {
                completed_ids.insert(id.clone());
            }
            has_trailing_tools = true;
        } else if msg.role == "assistant" {
            if let Some(ref calls) = msg.tool_calls {
                for call in calls {
                    if !completed_ids.contains(&call.id) {
                        found_pending = true;
                    }
                }
            }
            break;
        } else {
            // Case C: last message is user/system — turn was never started.
            break;
        }
    }
    // Check if the very last message is a user message (no assistant response yet).
    let last_is_user = history.last().is_some_and(|m| m.role == "user");
    found_pending || has_trailing_tools || last_is_user
}

/// Fully-assembled components ready for the Orchestrator to use.
///
/// Built by the Composition Root (daemon.rs).  This struct is the seam that
/// decouples the Application layer from Infrastructure assembly logic.
pub struct OrchestratorParts {
    pub session_manager: Arc<SessionManager>,
    /// Pre-built channels: (channel_type, account_id, channel_instance).
    pub channels: Vec<(String, String, Arc<dyn Channel>)>,
    /// Delegation manager (conditional — only when sub-agents are configured).
    pub delegator: Option<Arc<DelegationCoordinator>>,
    /// Delegation event receiver (conditional).
    pub delegation_rx: Option<mpsc::Receiver<DelegationEvent>>,
    /// Scheduler event receiver (heartbeat ticks, cron triggers from Scheduler task).
    pub scheduler_rx: Option<mpsc::Receiver<SchedulerEvent>>,
    /// AskRouter shared with the daemon-built `AskUserTool` (same
    /// `Arc<AskRouter>`). The orchestrator's inbound dispatch calls
    /// `ask_router.fulfill(session.id, msg)` to wake any pending ask.
    pub ask_router: Arc<crate::agents::AskRouter>,
    /// AgentRuntime for the `Agent::run` per-turn path.
    pub agent_runtime: crate::agents::AgentRuntime,
    /// Workspace directory for persisting runtime state.
    pub workspace_dir: std::path::PathBuf,
    /// Shared scheduler for run result tracking and auto-delete.
    pub scheduler: Option<crate::agents::SharedScheduler>,
}

impl Orchestrator {
    /// Create a new Orchestrator from pre-assembled parts.
    ///
    /// The Composition Root is responsible for building `OrchestratorParts`
    /// (creating Registry, registering Providers/Tools, opening Storage, etc.).
    pub fn new(parts: OrchestratorParts) -> (Self, ChannelMsgSender) {
        let (msg_tx, msg_rx) = mpsc::channel(CHANNEL_QUEUE_SIZE);
        let msg_tx = Arc::new(msg_tx);

        let channels_map: Arc<DashMap<(String, String), Arc<dyn Channel>>> = Arc::new(DashMap::new());
        let mut listener_handles = Vec::new();

        for (channel_type, account_id, channel) in &parts.channels {
            let handle = Self::spawn_listener(
                channel_type.clone(),
                account_id.clone(),
                Arc::clone(channel),
                Arc::clone(&msg_tx),
            );
            channels_map.insert((channel_type.clone(), account_id.clone()), Arc::clone(channel));
            listener_handles.push(handle);
            info!(channel = %channel_type, account = %account_id, "listener started");
        }

        if channels_map.is_empty() {
            warn!("no channels enabled");
        }

        let ctx = Arc::new(OrchestratorCtx {
            channels: channels_map,
            session_manager: parts.session_manager,
            ask_router: parts.ask_router,
            agent_runtime: parts.agent_runtime,
            delegator: parts.delegator,
            scheduler: parts.scheduler,
        });

        let orchestrator = Orchestrator {
            ctx,
            msg_rx: Arc::new(TokioMutex::new(Some(msg_rx))),
            listener_handles: Arc::new(TokioMutex::new(listener_handles)),
            delegation_rx: Arc::new(TokioMutex::new(parts.delegation_rx)),
            scheduler_rx: Arc::new(TokioMutex::new(parts.scheduler_rx)),
        };

        info!(channels = orchestrator.ctx.channels.len(), "orchestrator initialized");
        (orchestrator, (*msg_tx).clone())
    }

    /// The shared dependency bundle. The webhook server and other long-lived
    /// consumers hold this `Arc` directly instead of going through per-field
    /// accessor methods.
    pub fn ctx(&self) -> &Arc<OrchestratorCtx> {
        &self.ctx
    }

    fn spawn_listener(
        channel_type: String,
        account_id: String,
        channel: Arc<dyn Channel>,
        msg_tx: Arc<ChannelMsgSender>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                let mut rx = match channel.listen().await {
                    Ok(r) => {
                        backoff = Duration::from_secs(1);
                        r
                    }
                    Err(e) => {
                        error!(
                            channel = %channel_type,
                            account = %account_id,
                            err = %e,
                            delay_secs = backoff.as_secs(),
                            "listen failed, retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        continue;
                    }
                };
                while let Some(msg) = rx.recv().await {
                    if msg_tx.send(((channel_type.clone(), account_id.clone()), msg)).await.is_err() {
                        // Orchestrator is gone; no point reconnecting.
                        return;
                    }
                }
                // Stream ended cleanly (channel disconnected) — reconnect.
                warn!(
                    channel = %channel_type,
                    account = %account_id,
                    delay_secs = backoff.as_secs(),
                    "listener stream ended, reconnecting"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        })
    }

    /// Main message loop. Consumes self.msg_rx.
    /// Call from the Composition Root; blocks until shutdown.
    ///
    /// Takes `self: Arc<Self>` so scheduler-event spawned tasks
    /// (heartbeat / cron) can hold an Arc reference to the orchestrator
    /// for the duration of the LLM round-trip.
    pub async fn run(
        self: Arc<Self>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        unfinished_subagents: Vec<crate::agents::UnfinishedSubAgent>,
    ) -> anyhow::Result<()> {
        let rx = {
            let mut guard = self.msg_rx.lock().await;
            guard.take().context("run() already called or msg_rx was None")?
        };

        // Take the delegation event receiver if available.
        let mut delegation_rx = {
            let mut guard = self.delegation_rx.lock().await;
            guard.take()
        };

        // Take the scheduler event receiver if available.
        let mut scheduler_rx = {
            let mut guard = self.scheduler_rx.lock().await;
            guard.take()
        };

        let mut rx = rx;

        // ── Startup recovery ──────────────────────────────────────────────
        // Sessions register a SessionContext synchronously; the recovery
        // LLM work spawns into background tasks so the event loop starts
        // without blocking.
        recovery::run_startup(
            &self.ctx.session_manager,
            &self.ctx.agent_runtime,
            &self.ctx.channels,
            &unfinished_subagents,
            &self.ctx.delegator,
        );

        // Unify the three event sources (user messages / delegation /
        // scheduler) onto a single mpsc<OrchestratorEvent>. Adapter tasks
        // pump from each source channel; the main loop selects on the
        // unified channel + shutdown. AskReply variant is reserved for
        // future ask_router wiring inside Agent.run.
        let (event_tx, mut event_rx) = mpsc::channel::<OrchestratorEvent>(CHANNEL_QUEUE_SIZE);

        // Adapter: user messages → Inbound
        let inbound_handle = {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                while let Some(((ct, ac), msg)) = rx.recv().await {
                    if event_tx
                        .send(OrchestratorEvent::Inbound {
                            channel_type: ct,
                            account_id: ac,
                            message: msg,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };

        // Adapter: delegation events → Delegation
        let delegation_handle = if let Some(mut drx) = delegation_rx.take() {
            let event_tx = event_tx.clone();
            Some(tokio::spawn(async move {
                while let Some(e) = drx.recv().await {
                    if event_tx
                        .send(OrchestratorEvent::Delegation(e))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }))
        } else {
            None
        };

        // Adapter: scheduler events → Scheduled
        let scheduler_handle = if let Some(mut srx) = scheduler_rx.take() {
            let event_tx = event_tx.clone();
            Some(tokio::spawn(async move {
                while let Some(e) = srx.recv().await {
                    if event_tx
                        .send(OrchestratorEvent::Scheduled(e))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }))
        } else {
            None
        };

        // Drop our local copy of the sender; the three adapters hold clones.
        // When all adapters exit (all source channels closed), event_rx.recv()
        // returns None and the main loop breaks.
        drop(event_tx);

        loop {
            if *shutdown_rx.borrow() {
                tracing::debug!("shutdown requested, exiting message loop");
                break;
            }

            // Hot switch checkpoint: SIGUSR1 set the flag — exit loop so
            // daemon.rs can trigger fork+execv.
            if crate::is_shutting_down() {
                tracing::debug!("shutdown flag detected in orchestrator, exiting for hot switch");
                break;
            }

            let event = tokio::select! {
                ev = event_rx.recv() => match ev {
                    Some(e) => e,
                    None => break, // all adapters exited
                },
                _ = shutdown_rx.changed() => {
                    tracing::info!("shutdown signal received");
                    break;
                }
            };

            match event {
                OrchestratorEvent::Scheduled(e) => {
                    Arc::clone(&self).handle_scheduler_event(e).await;
                }
                OrchestratorEvent::Shutdown => {
                    tracing::info!("OrchestratorEvent::Shutdown received");
                    break;
                }
                OrchestratorEvent::AskReply { session_id, reply } => {
                    // F35 forward path: AskRouter.fulfill on inbound is the
                    // normal mechanism; the AskReply variant exists for
                    // future explicit routing (e.g. webhook-delivered
                    // replies). For now log and discard if unfulfilled.
                    if !self.ctx.ask_router.fulfill(&session_id, reply) {
                        tracing::warn!(session = %session_id, "AskReply for unknown session");
                    }
                }
                OrchestratorEvent::Inbound { channel_type, account_id, message: msg } => {
                    inbound::dispatch(&self.ctx, (channel_type, account_id), msg).await;
                }
                OrchestratorEvent::Delegation(event) => {
                    delegation::wake(&self.ctx, event).await;
                }
            }
        }

        // Abort adapter tasks so they don't outlive the orchestrator.
        inbound_handle.abort();
        if let Some(h) = delegation_handle {
            h.abort();
        }
        if let Some(h) = scheduler_handle {
            h.abort();
        }

        info!("all listeners stopped, exiting");
        Ok(())
    }


    /// Handle a scheduler event (from the Scheduler task via mpsc).
    /// Dispatch scheduler events by spawning independent tasks.
    /// Pre-flight checks (file read, parse, due filter) run inline to avoid
    /// unnecessary task creation; the actual LLM execution is spawned.
    async fn handle_scheduler_event(self: Arc<Self>, event: SchedulerEvent) {
        match event {
            SchedulerEvent::Heartbeat { target_channel, target_account } => {
                tracing::debug!("heartbeat triggered (from scheduler)");
                // Pre-flight: cheap checks before spawning.
                let heartbeat_path = std::path::Path::new("HEARTBEAT.md");
                if !heartbeat_path.exists() {
                    tracing::debug!("heartbeat skipped: no HEARTBEAT.md");
                    return;
                }
                let content = match std::fs::read_to_string(heartbeat_path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(err = %e, "heartbeat skipped: cannot read HEARTBEAT.md");
                        return;
                    }
                };
                let (context, tasks) = super::heartbeat_tasks::parse_heartbeat(&content);
                if tasks.is_empty() {
                    tracing::debug!("heartbeat skipped: no tasks in HEARTBEAT.md");
                    return;
                }
                let state_path = std::path::Path::new("HEARTBEAT_STATE.json");
                let state = super::heartbeat_tasks::HeartbeatState::load(state_path);
                let due = super::heartbeat_tasks::due_tasks(&tasks, &state);
                if due.is_empty() {
                    tracing::debug!(
                        total_tasks = tasks.len(),
                        "heartbeat skipped: no tasks due"
                    );
                    return;
                }

                let prompt = super::heartbeat_tasks::build_heartbeat_prompt(&context, &due);
                tracing::info!(
                    due_tasks = due.len(),
                    total_tasks = tasks.len(),
                    "heartbeat: running due tasks"
                );

                // Spawn: LLM execution runs independently of the main loop.
                tokio::spawn(run_heartbeat_task(
                    self.ctx.clone(),
                    target_channel,
                    target_account,
                    prompt,
                    due.into_iter().cloned().collect(),
                    state,
                    state_path.to_path_buf(),
                ));
            }
            SchedulerEvent::Cron { session_key, prompt, target_channel, target_account, job_id, delivery, enabled_tools, disabled_tools, model, provider } => {
                tracing::debug!(session_key = %session_key, "cron job triggered (from scheduler)");
                tokio::spawn(run_cron_task(
                    self.ctx.clone(),
                    session_key,
                    prompt,
                    target_channel,
                    target_account,
                    job_id,
                    delivery,
                    enabled_tools,
                    disabled_tools,
                    model,
                    provider,
                ));
            }
        }
    }

    /// Abort all listener handles (call after run() returns).
    pub async fn shutdown_listeners(&self) {
        let handles = std::mem::take(&mut *self.listener_handles.lock().await);
        for h in handles {
            h.abort();
        }
        tracing::debug!("all listener tasks aborted");
    }
}

/// Check if a response is a silent "nothing to do" signal (used by heartbeat).
pub(crate) fn is_silent_ok(response: &str, prefix: &str) -> bool {
    let trimmed = response.trim().to_lowercase();
    let marker = format!("{}_ok", prefix);
    // Only match if the response IS the marker (possibly with surrounding whitespace).
    // Don't use contains() — a response with real content should never be silenced
    // just because it happens to mention the marker.
    trimmed == marker
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_key() {
        assert_eq!(
            key::SessionKey::new("wechat", "default", "o9cq80zXpSX1Hz0ph_QNs591k4PA").to_string(),
            "wechat:default:o9cq80zXpSX1Hz0ph_QNs591k4PA"
        );
    }
}
