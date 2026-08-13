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

pub mod ctx;
mod delegation;
pub mod event;
mod inbound;
pub mod key;
mod recovery;
mod scheduled;
#[cfg(test)]
mod test_support;
pub mod turn;

pub use ctx::{ChannelRegistry, OrchestratorCtx};
pub use event::OrchestratorEvent;
pub(crate) use scheduled::{run_cron_task, run_distill_task, run_heartbeat_task, run_scheduled_turn};

use crate::agents::DelegationCoordinator;
use crate::agents::delegation::DelegationEvent;
use crate::channels::{Channel, ChannelInboundMessage};
use crate::storage::InboundSpool;
use anyhow::Context;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agents::session::SessionManager;

/// Buffer size for the unified `OrchestratorEvent` channel. Fixed (not
/// config-exposed) on purpose: this is an internal backpressure bound, not
/// an operator-tunable knob. The meaningful knobs (tool timeout, compaction
/// threshold, loop-breaker limits) live in `[agent]`/`[context]` config.
const CHANNEL_QUEUE_SIZE: usize = 100;

/// A cron job that fired and needs a turn run. The fields the scheduled path
/// actually consumes — delivery/enabled_tools/disabled_tools/provider, which
/// the cron turn ignores, are deliberately not carried here.
#[derive(Debug)]
pub struct CronTrigger {
    pub session_key: String,
    pub prompt: String,
    pub target_channel: Option<String>,
    pub target_account: Option<String>,
    pub target_recipient: Option<String>,
    pub job_id: String,
    pub model: Option<String>,
    /// Context policy: inject into user session or run isolated.
    pub context_policy: crate::config::scheduler::ContextPolicy,
}

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
    Cron(CronTrigger),
    /// Idle-time memory distillation check (system idle + maybe new
    /// user memories — the orchestrator verifies before running).
    Distill,
}

/// An inbound message handed from a channel listener to the event loop.
/// `seq` is the inbound-spool sequence (RFC inbound-spool §6.1); 0 means
/// "not spooled" — spool disabled, attachment message (RFC §6.3), or append
/// failure degraded delivery.
pub struct InboundEnvelope {
    pub seq: u64,
    pub channel: String,
    pub account: String,
    pub msg: ChannelInboundMessage,
}

/// Type alias for the channel message sender.
pub type ChannelMsgSender = mpsc::Sender<InboundEnvelope>;

/// Orchestrator — Application Service for message routing and session lifecycle.
///
/// Coordinates the flow: Channel → Session → Agent::run → Channel.
/// Does NOT depend on any Infrastructure concrete types.
pub struct Orchestrator {
    /// Shared dependency bundle (channels, sessions, runtime, ask, scheduler,
    /// delegator). Cloned into spawned tasks that must outlive a single turn.
    ctx: Arc<OrchestratorCtx>,
    /// Inbound user-message receiver. Consumed by `run(self)`.
    msg_rx: Option<mpsc::Receiver<InboundEnvelope>>,
    /// Listener task handles — aborted when `run` returns (it owns `self`).
    listener_handles: Vec<JoinHandle<()>>,
    /// Delegation event receiver (None when sub-agents are disabled).
    delegation_rx: Option<mpsc::Receiver<DelegationEvent>>,
    /// Scheduler event receiver (None when scheduling is disabled).
    scheduler_rx: Option<mpsc::Receiver<SchedulerEvent>>,
}

/// Return `true` if the session history ends mid-turn and needs recovery or a
/// retry/abort prompt.
///
/// Incomplete shapes:
/// - Case A: last assistant has tool_calls whose IDs lack matching tool results
/// - Case B: history ends on tool-result messages (LLM never continued)
/// - Case C: history ends on a user message (turn never produced an assistant)
///
/// A fully closed assistant text reply (no pending tool_calls) is complete even
/// if earlier tool rounds exist deeper in history.
pub(super) fn history_has_incomplete_turn(
    history: &[crate::providers::capability_chat::ChatMessage],
) -> bool {
    let Some(last) = history.last() else {
        return false;
    };

    // Case C: trailing user — no assistant response yet.
    if last.role == "user" {
        return true;
    }

    // Walk the open tool round at the tail only.
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
            // Case B: tools fulfilled and waiting for the next LLM call, or
            // Case A: some tool_calls still pending. A plain assistant text
            // reply (no tool_calls, no trailing tools) is complete.
            return found_pending || has_trailing_tools;
        } else {
            // system / other — not a mid-turn marker by itself.
            break;
        }
    }
    // Trailing tools with no preceding assistant in the reverse walk is still
    // incomplete (orphan tool tail after sanitize edge cases).
    has_trailing_tools
}

#[cfg(test)]
mod incomplete_turn_tests {
    use super::history_has_incomplete_turn;
    use crate::providers::capability_chat::ChatMessage;
    use crate::providers::ToolCall;

    fn asst_with_tools(ids: &[&str]) -> ChatMessage {
        let mut msg = ChatMessage::assistant_text("calling");
        msg.tool_calls = Some(
            ids.iter()
                .map(|id| ToolCall {
                    id: (*id).to_string(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                })
                .collect(),
        );
        msg
    }

    fn tool_result(id: &str) -> ChatMessage {
        let mut msg = ChatMessage::text("tool", "ok");
        msg.tool_call_id = Some(id.to_string());
        msg.name = Some("shell".into());
        msg
    }

    #[test]
    fn complete_assistant_text_is_not_incomplete() {
        let history = vec![
            ChatMessage::user_text("hi"),
            ChatMessage::assistant_text("hello"),
        ];
        assert!(!history_has_incomplete_turn(&history));
    }

    #[test]
    fn trailing_user_is_incomplete() {
        let history = vec![
            ChatMessage::user_text("hi"),
            ChatMessage::assistant_text("hello"),
            ChatMessage::user_text("again"),
        ];
        assert!(history_has_incomplete_turn(&history));
    }

    #[test]
    fn trailing_tools_without_final_assistant_are_incomplete() {
        let history = vec![
            ChatMessage::user_text("hi"),
            asst_with_tools(&["t1"]),
            tool_result("t1"),
        ];
        assert!(history_has_incomplete_turn(&history));
    }

    #[test]
    fn pending_tool_calls_are_incomplete() {
        let history = vec![ChatMessage::user_text("hi"), asst_with_tools(&["t1", "t2"])];
        assert!(history_has_incomplete_turn(&history));
    }

    #[test]
    fn fulfilled_tools_then_assistant_text_is_complete() {
        let history = vec![
            ChatMessage::user_text("hi"),
            asst_with_tools(&["t1"]),
            tool_result("t1"),
            ChatMessage::assistant_text("done"),
        ];
        assert!(!history_has_incomplete_turn(&history));
    }
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
    /// Global user registry — shared with slash commands.
    pub known_users: Arc<crate::agents::KnownUsersRegistry>,
    /// P4 用户实体注册表（uid/email/username）——gate 判定、命令与工具
    /// 解析 user.id / email 共用。
    pub user_registry: Arc<crate::agents::UserRegistry>,
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

        let channels = ChannelRegistry::new();
        let mut listener_handles = Vec::new();

        for (channel_type, account_id, channel) in &parts.channels {
            let handle = Self::spawn_listener(
                channel_type.clone(),
                account_id.clone(),
                Arc::clone(channel),
                Arc::clone(&msg_tx),
                inbound_spool.clone(),
            );
            channels.insert(
                (channel_type.clone(), account_id.clone()),
                Arc::clone(channel),
            );
            listener_handles.push(handle);
            info!(channel = %channel_type, account = %account_id, "listener started");
        }

        if channels.is_empty() {
            warn!("no channels enabled");
        }

        // RFC inbound-spool: persistent at-least-once spool for inbound
        // channel messages. Written at spawn_listener before the message
        // enters the event loop; marked Done after dispatch returns. `None`
        // degrades to in-memory-only delivery (fail-open, same as tests).
        let inbound_spool_dir = parts.workspace_dir.join(".state").join("inbound_spool");
        let inbound_spool = match crate::storage::InboundSpool::open(inbound_spool_dir.clone()) {
            Ok(spool) => {
                // Startup tombstone maintenance (RFC §4.2). Best-effort: a
                // failure only leaves stale tombstones around (dedup keys),
                // never loses Pending entries.
                if let Err(e) = spool.compact_if_needed() {
                    error!(
                        err = %e,
                        dir = %inbound_spool_dir.display(),
                        "inbound spool compact failed; continuing with stale tombstones"
                    );
                }
                info!(dir = %inbound_spool_dir.display(), "inbound spool opened");
                Some(Arc::new(spool))
            }
            Err(e) => {
                error!(
                    err = %e,
                    dir = %inbound_spool_dir.display(),
                    "inbound spool open failed; degraded to in-memory delivery"
                );
                None
            }
        };

        // P2 (2026-08-13, RFC delegation-notice-queue §5): open the persistent
        // completion-notice delivery queue (at-least-once across restarts).
        // `None` degrades to P1 in-memory-only delivery — a crashed notice is
        // then lost, but the daemon still runs (fail-open, same as tests).
        let completion_queue_dir = parts
            .workspace_dir
            .join(".state")
            .join("completion_queue");
        let completion_queue = match crate::storage::CompletionNoticeStore::open(
            completion_queue_dir.clone(),
        ) {
            Ok(store) => {
                info!(dir = %completion_queue_dir.display(), "completion queue opened");
                Some(Arc::new(store))
            }
            Err(e) => {
                error!(
                    err = %e,
                    dir = %completion_queue_dir.display(),
                    "completion queue open failed; degraded to in-memory delivery"
                );
                None
            }
        };

        let ctx = Arc::new(OrchestratorCtx {
            channels,
            sessions: parts.session_manager,
            ask: parts.ask_router,
            known_users: parts.known_users,
            user_registry: parts.user_registry,
            runtime: parts.agent_runtime,
            delegator: parts.delegator,
            scheduler: parts.scheduler,
            turn_tracker: Arc::new(ctx::TurnTracker::new()),
            completion_queue,
            inbound_spool,
        });

        let orchestrator = Orchestrator {
            ctx,
            msg_rx: Some(msg_rx),
            listener_handles,
            delegation_rx: parts.delegation_rx,
            scheduler_rx: parts.scheduler_rx,
        };

        info!(
            channels = orchestrator.ctx.channels.len(),
            "orchestrator initialized"
        );
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
        spool: Option<Arc<InboundSpool>>,
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
                    // RFC inbound-spool §6.1: persist BEFORE the message enters
                    // the event loop, so a crash between receive and dispatch
                    // is recoverable. Attachment messages bypass spooling
                    // entirely (RFC §6.3 — file bodies are runtime-only);
                    // dedup hits are dropped; append failures degrade to
                    // live delivery with seq 0 (fail-open).
                    let seq = if msg.content.files.is_empty() {
                        match &spool {
                            Some(spool) => match spool.append(&channel_type, &account_id, &msg) {
                                Ok(Some(seq)) => seq,
                                Ok(None) => {
                                    // Dedup: same (channel, account, msg.id)
                                    // already handled (e.g. WeChat buf
                                    // rollback) — do NOT deliver again.
                                    continue;
                                }
                                Err(e) => {
                                    error!(
                                        channel = %channel_type,
                                        account = %account_id,
                                        err = %e,
                                        "inbound spool append failed; delivering without persistence"
                                    );
                                    0
                                }
                            },
                            None => 0,
                        }
                    } else {
                        0
                    };
                    if msg_tx
                        .send(InboundEnvelope {
                            seq,
                            channel: channel_type.clone(),
                            account: account_id.clone(),
                            msg,
                        })
                        .await
                        .is_err()
                    {
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
        mut self,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        unfinished_subagents: Vec<crate::agents::UnfinishedSubAgent>,
    ) -> anyhow::Result<()> {
        use tokio_stream::wrappers::ReceiverStream;
        use tokio_stream::{Stream, StreamExt};

        let rx = self
            .msg_rx
            .take()
            .context("run() already called or msg_rx was None")?;

        // ── Startup recovery ──────────────────────────────────────────────
        // Sessions register a SessionContext synchronously; the recovery
        // LLM work spawns into background tasks so the event loop starts
        // without blocking.
        //
        // Light C: on hot-switch startups, defer recovery until the old
        // process has exited (or timeout) so incomplete tool results the
        // old process is still draining can persist before we re-exec.
        let ctx = Arc::clone(&self.ctx);
        let unfinished = unfinished_subagents;
        tokio::spawn(async move {
            #[cfg(unix)]
            if crate::hot_switch::is_hot_switch() {
                if let Some(old) = crate::hot_switch::old_pid() {
                    crate::hot_switch::wait_for_old_process_exit(
                        old,
                        crate::hot_switch::RECOVERY_WAIT_OLD_TIMEOUT,
                    )
                    .await;
                }
            }
            recovery::run_startup(&ctx, &unfinished);
        });

        // Known-users persistence flush (every 60s).
        let known_users_flush = Arc::clone(&self.ctx.known_users);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                known_users_flush.flush();
            }
        });

        // Merge the event sources (user messages / delegation / scheduler)
        // into a single stream. No adapter tasks / manual fan-in: each source
        // is a Stream<OrchestratorEvent> and `merge` interleaves them.
        let mut events: std::pin::Pin<Box<dyn Stream<Item = OrchestratorEvent> + Send>> = Box::pin(
            ReceiverStream::new(rx).map(|env| OrchestratorEvent::Inbound {
                channel_type: env.channel,
                account_id: env.account,
                message: env.msg,
                seq: env.seq,
            }),
        );
        if let Some(drx) = self.delegation_rx.take() {
            events =
                Box::pin(events.merge(ReceiverStream::new(drx).map(OrchestratorEvent::Delegation)));
        }
        if let Some(srx) = self.scheduler_rx.take() {
            events =
                Box::pin(events.merge(ReceiverStream::new(srx).map(OrchestratorEvent::Scheduled)));
        }

        loop {
            // Hot switch checkpoint: SIGUSR1 set the flag — exit loop so
            // daemon.rs can trigger fork+execv.
            if *shutdown_rx.borrow() || crate::is_shutting_down() {
                tracing::debug!("shutdown requested, exiting message loop");
                break;
            }

            let event = tokio::select! {
                ev = events.next() => match ev {
                    Some(e) => e,
                    None => break, // all sources closed
                },
                _ = shutdown_rx.changed() => {
                    tracing::info!("shutdown signal received");
                    break;
                }
            };

            match event {
                OrchestratorEvent::Scheduled(e) => {
                    self.handle_scheduler_event(e).await;
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
                    if !self.ctx.ask.fulfill(&session_id, reply) {
                        tracing::warn!(session = %session_id, "AskReply for unknown session");
                    }
                }
                OrchestratorEvent::Inbound {
                    channel_type,
                    account_id,
                    message: msg,
                    seq,
                } => {
                    inbound::dispatch(&self.ctx, (channel_type.clone(), account_id.clone()), msg).await;
                    // RFC inbound-spool §6.2: mark done AFTER dispatch returns
                    // (message is in session history). Includes the
                    // rate-limit/drop paths — replay must not bypass limits.
                    if seq != 0 {
                        if let Some(spool) = &self.ctx.inbound_spool {
                            if let Err(e) = spool.mark_done(seq) {
                                warn!(
                                    seq,
                                    err = %e,
                                    "inbound spool mark_done failed; entry stays Pending (will replay)"
                                );
                            }
                        }
                    }
                }
                OrchestratorEvent::Delegation(event) => {
                    delegation::wake(&self.ctx, event).await;
                }
            }
        }

        // `self` owns the listeners; abort them as it drops.
        for h in self.listener_handles.drain(..) {
            h.abort();
        }

        // Light C / hot switch: when SIGUSR1 requested a switch, **do not**
        // drain here. The composition root forks the new process first, then
        // drains so in-flight tools (e.g. short `myclaw update`) can finish
        // and persist while the new binary is already coming up. Immediate
        // drain-before-fork + 30s timeout was part of the restart storm:
        // drain timed out on a shell waiting for UPDATE_STATUS=completed,
        // then the old process died with orphan tool_calls.
        //
        // Normal stop (SIGINT/SIGTERM without hot switch): drain before return
        // so tool results and sub-agents persist before process exit.
        if crate::SHUTDOWN_FLAG.load(std::sync::atomic::Ordering::SeqCst) {
            info!(
                active_turns = self.ctx.turn_tracker.active_count(),
                "listeners stopped; deferring turn drain until after hot-switch fork"
            );
        } else {
            // Drain in-flight turn tasks before returning so tool results are
            // persisted before the process exits.
            self.ctx.turn_tracker.drain(Duration::from_secs(30)).await;

            // Checkpoint and cancel background sub-agents so their state
            // survives restart. Unlike drain, this does not treat a timeout
            // as a business failure — checkpointed tasks resume on restart.
            if let Some(ref delegator) = self.ctx.delegator {
                delegator.checkpoint_and_cancel_all();
            }
            info!("all listeners stopped, turns drained, exiting");
        }
        Ok(())
    }

    /// Shared turn tracker (for deferred drain after hot-switch fork).
    pub fn turn_tracker(&self) -> ctx::SharedTurnTracker {
        Arc::clone(&self.ctx.turn_tracker)
    }

    /// Shared delegator (for deferred drain after hot-switch fork).
    pub fn delegator(&self) -> Option<Arc<DelegationCoordinator>> {
        self.ctx.delegator.clone()
    }

    /// Handle a scheduler event (from the Scheduler task via mpsc).
    /// Dispatch scheduler events by spawning independent tasks.
    /// Pre-flight checks (file read, parse, due filter) run inline to avoid
    /// unnecessary task creation; the actual LLM execution is spawned.
    async fn handle_scheduler_event(&self, event: SchedulerEvent) {
        match event {
            SchedulerEvent::Heartbeat {
                target_channel,
                target_account,
            } => {
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
                    tracing::debug!(total_tasks = tasks.len(), "heartbeat skipped: no tasks due");
                    return;
                }

                let prompt = super::heartbeat_tasks::build_heartbeat_prompt(&context, &due);
                tracing::info!(
                    due_tasks = due.len(),
                    total_tasks = tasks.len(),
                    "heartbeat: running due tasks"
                );

                // Spawn: LLM execution runs independently of the main loop.
                let due_owned: Vec<_> = due.into_iter().cloned().collect();
                let self_ctx = self.ctx.clone();
                let turn_tracker = self.ctx.turn_tracker.clone();
                tokio::spawn(async move {
                    let _guard = turn_tracker.track();
                    run_heartbeat_task(
                        self_ctx,
                        target_channel,
                        target_account,
                        prompt,
                        due_owned,
                        state,
                        state_path.to_path_buf(),
                    )
                    .await;
                });
            }
            SchedulerEvent::Cron(trigger) => {
                tracing::debug!(session_key = %trigger.session_key, "cron job triggered (from scheduler)");
                let self_ctx = self.ctx.clone();
                let turn_tracker = self.ctx.turn_tracker.clone();
                tokio::spawn(async move {
                    let _guard = turn_tracker.track();
                    run_cron_task(self_ctx, trigger).await;
                });
            }
            SchedulerEvent::Distill => {
                tracing::debug!("memory_distill: check triggered (from scheduler)");
                let self_ctx = self.ctx.clone();
                let turn_tracker = self.ctx.turn_tracker.clone();
                tokio::spawn(async move {
                    let _guard = turn_tracker.track();
                    run_distill_task(self_ctx).await;
                });
            }
        }
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
