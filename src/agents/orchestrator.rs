//! Orchestrator — Application Service that connects channels and agent loops.
//!
//! This is the core Application Service in DDD terms:
//! - Receives messages from Interface (Channel) adapters
//! - Coordinates Domain objects (Agent, Session, Tools)
//! - Routes responses back through Interface adapters
//!
//! Assembly of Infrastructure components (Registry, Providers, Tools, Storage)
//! is done in the Composition Root (orchestration/orchestrator main.rs + daemon.rs),
//! not here. This struct receives fully-assembled components via its constructor.

use anyhow::Context;
use crate::agents::delegation::{DelegationEvent, DelegationManager};
use crate::agents::{OrchestratorEvent, SessionContext};
use crate::channels::{Channel, ChannelMessage, SendMessage, InlineButton};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agents::session::{SessionManager, PersistHook, BackendPersistHook};

const CHANNEL_QUEUE_SIZE: usize = 100;

// ── User-facing message strings ────────────────────────────────────────────────
const MSG_NO_PENDING_RETRY: &str = "没有待重试的消息，请重新发送。";
const MSG_ABORT_ACK: &str = "已取消";
const MSG_INCOMPLETE_TURN: &str = "⚠️ 检测到上次请求未处理完成（可能是服务重启）。\n\n请选择重试或放弃。";
const BTN_RETRY: &str = "🔄 重试";
const BTN_ABORT: &str = "✖ 放弃";

/// Internal enum for the run loop's select.
enum ChannelEvent {
    UserMessage(((String, String), ChannelMessage)),
    Delegation(DelegationEvent),
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
/// Coordinates the flow: Channel → Session → AgentLoop → Channel.
/// Does NOT depend on any Infrastructure concrete types.
pub struct Orchestrator {
    /// Channels, keyed by (channel_type, account_id).
    channels: Arc<DashMap<(String, String), Arc<dyn Channel>>>,
    /// SessionManager owns the SessionContext table — see
    /// `SessionManager::get_or_create_context`. The Orchestrator no longer
    /// keeps its own copy; reach for `session_manager.get_context(rk)` or
    /// `get_or_create_context(rk)` instead.
    session_manager: Arc<SessionManager>,
    /// The message receiver, owned and consumed by run().
    #[allow(clippy::type_complexity)]
    msg_rx: Arc<TokioMutex<Option<mpsc::Receiver<((String, String), ChannelMessage)>>>>,
    /// Listener task handles — taken and awaited on shutdown.
    /// Wrapped in a TokioMutex so `shutdown_listeners(&self)` can drain
    /// them without requiring `&mut self` (which would block the
    /// `Arc<Self>` sharing pattern that scheduler dispatch relies on).
    listener_handles: Arc<TokioMutex<Vec<JoinHandle<()>>>>,
    /// AskRouter (RFC v2 §三.B): indexed by session.id, fulfilled by inbound
    /// messages ahead of process_turn. Wired here so the new
    /// `AskUserTool::with_router` path is reachable end-to-end before
    /// AgentLoop is deleted (H45). Shared with daemon-side AskUserTool
    /// construction.
    ask_router: Arc<crate::agents::AskRouter>,
    /// AgentRuntime for `Agent::run` (RFC v2 §三.A). Held alongside
    /// the legacy `agent` field — E29 will eventually swap the main-
    /// loop dispatch onto Agent + agent_runtime, then H45 deletes the
    /// legacy fields.
    agent_runtime: crate::agents::AgentRuntime,
    /// Delegation manager (shared with DelegateTaskTool via handler).
    delegation_manager: Option<Arc<DelegationManager>>,
    /// Delegation event receiver.
    delegation_rx: Arc<TokioMutex<Option<mpsc::Receiver<DelegationEvent>>>>,
    /// MCP manager (for /mcp command).
    mcp_manager: Option<Arc<crate::agents::McpManager>>,
    /// Last channel that received a user message (shared with schedulers).
    /// Format: "channel_type:account_id"
    pub last_channel: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Path to persist last_channel across restarts.
    last_channel_file: std::path::PathBuf,
    /// Last recipient (reply_target) that received a user message (shared with schedulers).
    pub last_recipient: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Path to persist last_recipient across restarts.
    last_recipient_file: std::path::PathBuf,
    /// Scheduler event receiver (heartbeat ticks, cron triggers).
    scheduler_rx: Arc<TokioMutex<Option<mpsc::Receiver<SchedulerEvent>>>>,
    /// Search provider cooldown tracker (shared with WebSearchTool).
    search_cooldown: Option<Arc<crate::tools::search_cooldown::SearchProviderCooldown>>,
    /// Sub-agents that were interrupted by a hot-switch restart.
    /// Injected as a system reminder on the first session interaction, then cleared.
    unfinished_subagents: parking_lot::Mutex<Vec<crate::agents::UnfinishedSubAgent>>,
    /// Shared scheduler for run result tracking from cron tasks.
    scheduler: Option<crate::agents::SharedScheduler>,
}

/// Resources shared between Orchestrator and scheduler tasks.
///
/// SessionContext lookup now lives on `SessionManager` (1:1 invariant);
/// callers that previously held an `Arc<DashMap<_, Arc<SessionContext>>>`
/// reach for `session_manager.get_or_create_context(rk)` instead.
pub struct SharedSessions {
    /// AgentRuntime for Agent dispatch in webhook tasks.
    pub agent_runtime: crate::agents::AgentRuntime,
    pub channels: Arc<DashMap<(String, String), Arc<dyn Channel>>>,
    pub last_channel: Arc<tokio::sync::Mutex<Option<String>>>,
    pub last_recipient: Arc<tokio::sync::Mutex<Option<String>>>,
}

/// Parse a session key like "telegram:ops:12345" into (channel_type, account_id, sender).
fn parse_session_key(sk: &str) -> Option<(&str, &str, &str)> {
    let mut parts = sk.splitn(3, ':');
    let channel_type = parts.next()?;
    let account_id = parts.next()?;
    let sender = parts.next()?;
    if channel_type.is_empty() || account_id.is_empty() || sender.is_empty() {
        return None;
    }
    Some((channel_type, account_id, sender))
}

/// Return `true` if the session history ends with an incomplete tool execution:
/// either trailing tool-result messages, or assistant tool_calls whose IDs have
/// no matching tool-result — indicating the turn was interrupted mid-execution.
fn history_has_incomplete_turn(history: &[crate::providers::capability_chat::ChatMessage]) -> bool {
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
    pub delegation_manager: Option<Arc<DelegationManager>>,
    /// Delegation event receiver (conditional).
    pub delegation_rx: Option<mpsc::Receiver<DelegationEvent>>,
    /// MCP manager (conditional — only when MCP servers are configured).
    pub mcp_manager: Option<Arc<crate::agents::McpManager>>,
    /// Scheduler event receiver (heartbeat ticks, cron triggers from Scheduler task).
    pub scheduler_rx: Option<mpsc::Receiver<SchedulerEvent>>,
    /// Search provider cooldown tracker (shared with WebSearchTool).
    pub search_cooldown: Option<Arc<crate::tools::search_cooldown::SearchProviderCooldown>>,
    /// AskRouter shared with the daemon-side `AskUserTool::with_router`
    /// construction. The orchestrator's inbound dispatch calls
    /// `ask_router.fulfill(session.id, msg.content)` ahead of the legacy
    /// `pending_asks` check so both paths work during the transition.
    pub ask_router: Arc<crate::agents::AskRouter>,
    /// AgentRuntime for the new `Agent::run` per-turn path. Coexists
    /// with `agent` (legacy AgentLoop factory) until E29 swaps the
    /// orchestrator's main-loop dispatch over to Agent::run and H45
    /// deletes AgentLoop.
    pub agent_runtime: crate::agents::AgentRuntime,
    /// Sub-agents that were still running when the previous daemon was killed.
    /// Injected as a recovery hint into the first session interaction.
    pub unfinished_subagents: Vec<crate::agents::UnfinishedSubAgent>,
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

        let last_channel_file = parts.workspace_dir.join(".last_channel");

        // Load persisted last_channel from disk.
        let last_channel_value = std::fs::read_to_string(&last_channel_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let last_recipient_file = parts.workspace_dir.join(".last_recipient");

        // Load persisted last_recipient from disk.
        let last_recipient_value = std::fs::read_to_string(&last_recipient_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let orchestrator = Orchestrator {
            channels: channels_map,
            session_manager: parts.session_manager,
            msg_rx: Arc::new(TokioMutex::new(Some(msg_rx))),
            listener_handles: Arc::new(TokioMutex::new(listener_handles)),
            ask_router: parts.ask_router,
            agent_runtime: parts.agent_runtime,
            delegation_manager: parts.delegation_manager,
            delegation_rx: Arc::new(TokioMutex::new(parts.delegation_rx)),
            mcp_manager: parts.mcp_manager,
            last_channel: Arc::new(tokio::sync::Mutex::new(last_channel_value)),
            last_channel_file,
            last_recipient: Arc::new(tokio::sync::Mutex::new(last_recipient_value)),
            last_recipient_file,
            scheduler_rx: Arc::new(TokioMutex::new(parts.scheduler_rx)),
            search_cooldown: parts.search_cooldown,
            unfinished_subagents: parking_lot::Mutex::new(parts.unfinished_subagents),
            scheduler: parts.scheduler,
        };

        info!(channels = orchestrator.channels.len(), "orchestrator initialized");
        (orchestrator, (*msg_tx).clone())
    }

    /// Get shared resources for scheduler tasks.
    pub fn shared(&self) -> SharedSessions {
        SharedSessions {
            agent_runtime: self.agent_runtime.clone(),
            channels: self.channels.clone(),
            last_channel: self.last_channel.clone(),
            last_recipient: self.last_recipient.clone(),
        }
    }

    /// Accessor for the webhook server's axum app state. Avoids
    /// exposing the field directly while still letting the webhook
    /// task reach the manager via `orchestrator.session_manager()`.
    pub fn session_manager(&self) -> &Arc<SessionManager> {
        &self.session_manager
    }

    /// Accessor for the AgentRuntime — used by the webhook server's
    /// per-request Agent dispatch.
    pub fn agent_runtime(&self) -> &crate::agents::AgentRuntime {
        &self.agent_runtime
    }

    /// Accessor for the channels map — used by the webhook server to
    /// resolve `target_channel:account` references at delivery time.
    #[allow(clippy::type_complexity)]
    pub fn channels(&self) -> &Arc<DashMap<(String, String), Arc<dyn Channel>>> {
        &self.channels
    }

    /// Accessor for the persisted session backend (the BackendPersistHook
    /// constructor target). Used by the webhook server to materialize a
    /// persist hook for in-flight scheduled turns.
    pub fn persist_backend(&self) -> &Arc<dyn crate::storage::SessionBackend> {
        self.session_manager.backend()
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

    fn session_key(channel_type: &str, account_id: &str, sender: &str) -> String {
        format!("{}:{}:{}", channel_type, account_id, sender)
    }

    /// Get or create the `SessionContext` for a routing_key.
    ///
    /// First call for a routing_key loads the Session from SessionManager
    /// (which reads/restores from backend), wraps it in `SessionContext`,
    /// and caches the result. Subsequent calls return the same `Arc`.
    ///
    /// The session's transient `persist` and `channel` fields are NOT
    /// populated here — callers wire them per-turn before locking the
    /// session to call `Agent::run`.
    fn session_context_for(&self, sk: &str) -> Arc<SessionContext> {
        self.session_manager.get_or_create_context(sk)
    }

    /// Main message loop. Consumes self.msg_rx.
    /// Call from the Composition Root; blocks until shutdown.
    ///
    /// Takes `self: Arc<Self>` so scheduler-event spawned tasks
    /// (heartbeat / cron) can hold an Arc reference to the orchestrator
    /// for the duration of the LLM round-trip.
    pub async fn run(self: Arc<Self>, mut shutdown_rx: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()> {
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

        // Build recovery hint for sub-agents interrupted by previous hot-switch.
        let unfinished_subagents = {
            let guard = self.unfinished_subagents.lock();
            guard.clone()
        };

        // ── Startup recovery ──────────────────────────────────────────────
        // Sessions register a SessionContext synchronously; the recovery
        // LLM work spawns into background tasks so the event loop starts
        // without blocking.
        self.startup_recover_sessions();
        self.startup_recover_subagents(&unfinished_subagents, &self.delegation_manager);

        // E29: unify the three event sources (user messages / delegation /
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
                OrchestratorEvent::AskReply { session_id, answer } => {
                    // F35 forward path: AskRouter.fulfill on inbound is the
                    // normal mechanism; the AskReply variant exists for
                    // future explicit routing (e.g. webhook-delivered
                    // replies). For now log and discard if unfulfilled.
                    if !self.ask_router.fulfill(&session_id, answer) {
                        tracing::warn!(session = %session_id, "AskReply for unknown session");
                    }
                }
                OrchestratorEvent::Inbound { channel_type, account_id, message: msg } => {
                    let event = ChannelEvent::UserMessage(((channel_type, account_id), msg));
                    self.handle_channel_event(event).await;
                }
                OrchestratorEvent::Delegation(event) => {
                    let event = ChannelEvent::Delegation(event);
                    self.handle_channel_event(event).await;
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

    /// Dispatch for the existing `ChannelEvent` variants. Kept as a private
    /// helper so the E29 main-loop migration to `OrchestratorEvent` can
    /// reuse the well-tested 400-line inbound dispatch logic verbatim
    /// rather than rewriting it inline.
    async fn handle_channel_event(&self, event: ChannelEvent) {
        let channels = self.channels.clone();
        match event {
            ChannelEvent::UserMessage(((channel_type, account_id), mut msg)) => {
                // Track last channel for scheduler target resolution.
                let lc_val = format!("{}:{}", channel_type, account_id);
                {
                    let mut lc = self.last_channel.lock().await;
                    if lc.as_deref() != Some(&lc_val) {
                        *lc = Some(lc_val.clone());
                        let _ = std::fs::write(&self.last_channel_file, &lc_val);
                    }
                }
                // Track last recipient for heartbeat/cron target resolution.
                let recipient = msg.reply_target.clone();
                {
                    let mut lr = self.last_recipient.lock().await;
                    if lr.as_deref() != Some(&recipient) {
                        *lr = Some(recipient.clone());
                        let _ = std::fs::write(&self.last_recipient_file, &recipient);
                    }
                }

                let sk = Self::session_key(&channel_type, &account_id, &msg.sender);
                let channel_key = (channel_type.clone(), account_id.clone());

                // RFC v2 §三.B: check the new `AskRouter` first (indexed
                // by session.id, used by AskUserTool::with_router). If
                // it fulfilled an outstanding ask, the inbound message
                // is consumed and no fresh turn is spawned.
                {
                    let session_id = self
                        .session_manager
                        .get_or_create(&sk)
                        .id
                        .clone();
                    if self.ask_router.fulfill(&session_id, msg.content.clone()) {
                        tracing::debug!(
                            session = %session_id,
                            "ask_router fulfilled pending ask, consuming inbound"
                        );
                        return;
                    }
                }


                // Check if this is a retry/abort callback from an EmptyResponse prompt.
                // E29 final: pending_retry lives on SessionContext. For
                // retry, we extract the saved text and rewrite the
                // incoming msg.content to it — then fall through to the
                // standard dispatch below so the regular Agent path
                // handles it. For abort, we clear pending_retry and
                // ack inline.
                if msg.content.starts_with("__retry:") || msg.content.starts_with("__abort:") {
                    let is_retry = msg.content.starts_with("__retry:");
                    let reply_target = msg.reply_target.clone();

                    let channel: Option<Arc<dyn Channel>> = {
                        channels.get(&channel_key).map(|r| r.clone())
                    };
                    let channel = match channel {
                        Some(c) => c,
                        None => return,
                    };

                    let session_ctx = self.session_context_for(&sk);
                    let pending = if is_retry {
                        session_ctx.pending_retry.lock().await.take()
                    } else {
                        *session_ctx.pending_retry.lock().await = None;
                        None
                    };

                    if is_retry {
                        match pending {
                            Some(user_msg) => {
                                // Rewrite content and fall through.
                                msg.content = user_msg;
                            }
                            None => {
                                let send_msg = SendMessage::new(
                                    MSG_NO_PENDING_RETRY,
                                    reply_target.clone(),
                                );
                                let _ = channel.send(&send_msg).await;
                                return;
                            }
                        }
                    } else {
                        let send_msg = SendMessage::new(MSG_ABORT_ACK, reply_target.clone());
                        let _ = channel.send(&send_msg).await;
                        return;
                    }
                }

                // Check for an incomplete turn loaded from a previous crash/SIGKILL.
                // E29 final: read incomplete_turn from SessionContext.session
                // and stash the orphaned user message on SessionContext.pending_retry.
                {
                    let session_ctx = self.session_context_for(&sk);
                    if let Ok(mut session) = session_ctx.session.try_lock() {
                        if session.incomplete_turn {
                            session.incomplete_turn = false;

                            let last_user_msg = session.history.last()
                                .filter(|m| m.role == "user")
                                .map(|m| m.text_content().to_string())
                                .unwrap_or_default();
                            *session_ctx.pending_retry.lock().await = Some(last_user_msg.clone());
                            drop(session);

                            let channel = match channels.get(&channel_key).map(|r| r.clone()) {
                                Some(c) => c,
                                None => return,
                            };
                            let send_msg = retry_abort_prompt(
                                MSG_INCOMPLETE_TURN,
                                &sk,
                                msg.reply_target.clone(),
                                Some(msg.id.clone()),
                            );
                            if let Err(e) = channel.send(&send_msg).await {
                                error!(session = %sk, err = %e, "failed to send incomplete-turn prompt");
                            }
                            return;
                        }
                    }
                }

                let content = msg.content.clone();
                let image_urls = msg.image_urls.clone();
                let image_base64 = msg.image_base64.clone();
                let reply_target = msg.reply_target.clone();
                let reply_to_id = Some(msg.id.clone());

                // Intercept slash commands before reaching agent loop.
                if let Some((cmd, cmd_args)) = super::commands::parse_command(&content) {
                    if super::commands::is_known_command(cmd) {
                        let sk_cmd        = sk.clone();
                        let cmd_owned     = cmd.to_string();
                        let cmd_args_owned = cmd_args.to_string();
                        let session_ctx_cmd = self.session_manager.get_context(&sk);
                        let registry_cmd  = Arc::clone(&self.agent_runtime.providers);
                        let sm_cmd        = self.session_manager.clone();
                        let runtime_cmd   = self.agent_runtime.clone();
                        let mcp_cmd       = self.mcp_manager.clone();
                        let cooldown_cmd  = self.search_cooldown.clone();
                        let channel_cmd   = channels.get(&channel_key).map(|r| r.clone());
                        let rt_cmd        = reply_target.clone();
                        let rid_cmd       = reply_to_id.clone();

                        tokio::spawn(async move {
                            let cmd_ctx = super::commands::CommandContext {
                                user_id:        &sk_cmd,
                                registry:       &registry_cmd,
                                session_manager: &sm_cmd,
                                runtime:        &runtime_cmd,
                                session_ctx:    session_ctx_cmd.as_ref(),
                                mcp_manager:    mcp_cmd.as_ref(),
                                search_cooldown: cooldown_cmd.as_ref(),
                            };
                            if let Some(response) = super::commands::dispatch(
                                &cmd_owned, &cmd_args_owned, cmd_ctx,
                            ).await {
                                if let Some(channel) = channel_cmd {
                                    let send_msg = SendMessage {
                                        recipient:          rt_cmd,
                                        content:            response,
                                        subject:            None,
                                        thread_ts:          rid_cmd,
                                        cancellation_token: None,
                                        attachments:        vec![],
                                        image_urls:         None,
                                        inline_buttons:     None,
                                    };
                                    if let Err(e) = channel.send(&send_msg).await {
                                        error!(session = %sk_cmd, err = %e,
                                            "command response send failed");
                                    }
                                }
                            }
                        });
                        return;
                    }
                }

                // B12: store full inbound ChannelMessage on session.
                {
                    let mut session = self.session_manager.get_or_create(&sk);
                    session.record_inbound(msg.clone());
                }
                if let Err(e) = self.session_manager.backend().save_last_message(&sk, &msg) {
                    tracing::warn!(session = %sk, err = %e, "failed to persist last_message");
                }

                let channel = match channels.get(&channel_key).map(|r| r.clone()) {
                    Some(c) => c,
                    None => return,
                };

                // Dispatch via SessionContext.process_turn — the canonical
                // RFC v2 per-turn entry point. Spawn on a background task so
                // the main event loop is not blocked by the LLM round-trip.
                let session_ctx = self.session_manager.get_or_create_context(&sk);
                let runtime = self.agent_runtime.clone();
                let inbound_msg = msg.clone();
                let _ = (image_urls, image_base64, reply_to_id);

                tokio::spawn(async move {
                    // image_urls / image_base64 are attached via
                    // `session.last_message` (recorded via `record_inbound`
                    // upstream); Agent.run reads them from there.
                    // reply_to_id is unused — channels send replies in-line
                    // without thread context for now.
                    session_ctx
                        .process_turn(inbound_msg, content, channel, reply_target, runtime)
                        .await;
                });

            }
            ChannelEvent::Delegation(event) => {
                // handle_delegation_event re-enters handle_channel_event with
                // a synthetic Inbound message; the standard Agent dispatch
                // (including its own tokio::spawn for the turn) takes it
                // from there.
                self.handle_delegation_event(event).await;
            }
        }
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
                    Arc::clone(&self),
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
                    Arc::clone(&self),
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

    /// Scan all persisted sessions for incomplete turns and resume them.
    ///
    /// Registers each session's actor synchronously (so new messages can be
    /// queued immediately), then spawns the LLM recovery work in background
    /// tasks so the event loop starts without waiting for them to finish.
    fn startup_recover_sessions(&self) {
        let all_sessions = self.session_manager.list_all_sessions();
        for session_info in &all_sessions {
            let sk = &session_info.owner;
            let session_snap = self.session_manager.get_or_create(sk);
            let history = &session_snap.history;
            if history.is_empty() || !history_has_incomplete_turn(history) {
                continue;
            }
            tracing::info!(session = %sk, "startup recovery: found incomplete turn, spawning background task");
            let session_ctx = self.session_context_for(sk);
            let sk_owned = sk.clone();
            let persist_backend = Arc::clone(self.session_manager.backend());
            let channels = self.channels.clone();
            let runtime = self.agent_runtime.clone();
            let prompt_config_base = self.agent_runtime.defaults.prompt.clone();
            let skills_arc = Arc::clone(&self.agent_runtime.skills);
            let cached_prompt = self.agent_runtime.defaults.system_prompt.clone();
            let persist_hook: Arc<dyn PersistHook> = Arc::new(
                BackendPersistHook::new(Arc::clone(self.session_manager.backend()))
            );

            tokio::spawn(async move {
                let _turn_guard = session_ctx.turn_lock.lock().await;
                let mut session = session_ctx.session.lock().await;
                session.persist = Some(persist_hook.clone());

                let session_override = session.session_override.clone();
                let mut prompt_config = prompt_config_base.clone();
                if let Some(pm) = session_override.permission_mode {
                    prompt_config.permission_mode = pm;
                }
                if let Some(rm) = session_override.run_mode {
                    prompt_config.run_mode = rm;
                }
                let system_prompt = if !cached_prompt.is_empty() {
                    cached_prompt
                } else {
                    let s = skills_arc.read();
                    crate::agents::SystemPromptBuilder::new(prompt_config.clone()).build(&s)
                };

                let thinking = session_override.to_thinking_config();
                let model_id = session_override.model.as_deref();
                let turn_ctx = crate::agents::TurnContext {
                    system_prompt: &system_prompt,
                    model_id,
                    thinking: thinking.as_ref(),
                    permission_mode: prompt_config.permission_mode,
                    run_mode: prompt_config.run_mode,
                };

                match session_ctx.agent.run_recovery(&mut session, turn_ctx, &runtime).await {
                    Ok(Some(tr)) if !tr.text.is_empty() => {
                        tracing::info!(session = %sk_owned, "startup recovery: turn completed");
                        let recipient = persist_backend.load_last_message(&sk_owned)
                            .map(|m| m.reply_target)
                            .unwrap_or_else(|| {
                                parse_session_key(&sk_owned)
                                    .map(|(_, _, sender)| sender.to_string())
                                    .unwrap_or_default()
                            });
                        if let Some((ch_type, acc_id, _)) = parse_session_key(&sk_owned) {
                            if let Some(channel) = channels.get(&(ch_type.to_string(), acc_id.to_string())).map(|r| r.clone()) {
                                let send_msg = SendMessage::new(&tr.text, &recipient);
                                if let Err(e) = channel.send(&send_msg).await {
                                    tracing::warn!(session = %sk_owned, err = %e, "startup recovery: failed to send response");
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(session = %sk_owned, err = %e, "startup recovery failed");
                    }
                }

                session.persist = None;
            });
        }
    }

    /// Recover sub-agents that were interrupted by a previous daemon shutdown.
    ///
    /// Registers each sub-agent actor synchronously, then spawns the LLM
    /// recovery work in background tasks (same pattern as startup_recover_sessions).
    fn startup_recover_subagents(
        &self,
        unfinished: &[crate::agents::UnfinishedSubAgent],
        delegation_manager: &Option<Arc<DelegationManager>>,
    ) {
        for sa in unfinished {
            if sa.sub_session_id.is_empty() || sa.session_key.is_empty() {
                tracing::debug!(task_id = %sa.task_id, "sub-agent recovery: skipping (no session_id or session_key)");
                continue;
            }
            let sub_sk = format!("{}:{}", sa.agent_name, sa.sub_session_id);
            let session_snap = self.session_manager.get_or_create(&sub_sk);
            let history = &session_snap.history;
            if history.is_empty() || !history_has_incomplete_turn(history) {
                continue;
            }
            tracing::info!(task_id = %sa.task_id, agent = %sa.agent_name, "sub-agent startup recovery: found incomplete turn, spawning background task");
            let session_ctx = self.session_context_for(&sub_sk);
            let task_id = sa.task_id.clone();
            let session_key = sa.session_key.clone();
            let sa_reply_target = sa.reply_target.clone();
            let dm = delegation_manager.clone();
            let runtime = self.agent_runtime.clone();
            let prompt_config_base = self.agent_runtime.defaults.prompt.clone();
            let skills_arc = Arc::clone(&self.agent_runtime.skills);
            let cached_prompt = self.agent_runtime.defaults.system_prompt.clone();
            let persist_hook: Arc<dyn PersistHook> = Arc::new(
                BackendPersistHook::new(Arc::clone(self.session_manager.backend()))
            );

            tokio::spawn(async move {
                let _turn_guard = session_ctx.turn_lock.lock().await;
                let mut session = session_ctx.session.lock().await;
                session.persist = Some(persist_hook.clone());

                let session_override = session.session_override.clone();
                let mut prompt_config = prompt_config_base.clone();
                if let Some(pm) = session_override.permission_mode {
                    prompt_config.permission_mode = pm;
                }
                if let Some(rm) = session_override.run_mode {
                    prompt_config.run_mode = rm;
                }
                let system_prompt = if !cached_prompt.is_empty() {
                    cached_prompt
                } else {
                    let s = skills_arc.read();
                    crate::agents::SystemPromptBuilder::new(prompt_config.clone()).build(&s)
                };
                let thinking = session_override.to_thinking_config();
                let model_id = session_override.model.as_deref();
                let turn_ctx = crate::agents::TurnContext {
                    system_prompt: &system_prompt,
                    model_id,
                    thinking: thinking.as_ref(),
                    permission_mode: prompt_config.permission_mode,
                    run_mode: prompt_config.run_mode,
                };

                match session_ctx.agent.run_recovery(&mut session, turn_ctx, &runtime).await {
                    Ok(Some(tr)) if !tr.text.is_empty() => {
                        tracing::info!(task_id = %task_id, "sub-agent startup recovery: turn completed");
                        if let Some(dm) = dm {
                            let _ = dm.event_sender().send(DelegationEvent::Completed {
                                task_id,
                                parent_session_id: session_key,
                                reply_target: sa_reply_target,
                                summary: tr.text,
                                duration_secs: 0,
                            }).await;
                        }
                    }
                    Ok(_) => {
                        tracing::debug!(task_id = %task_id, "sub-agent startup recovery: no recovery needed");
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %task_id, err = %e, "sub-agent startup recovery failed");
                    }
                }
                session.persist = None;
            });
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

// ── retry_abort_prompt ────────────────────────────────────────────────────────

/// Build a `SendMessage` that presents the user with **Retry / Abort** inline
/// buttons.
///
/// Centralises the construction that previously appeared 6–7 times verbatim in
/// `Orchestrator::run()`.  The callback data is prefixed with `__retry:` /
/// `__abort:` and a 32-char prefix of the session key so it fits within
/// Telegram's 64-byte limit.
fn retry_abort_prompt(
    content: impl Into<String>,
    sk: &str,
    reply_target: impl Into<String>,
    thread_ts: Option<String>,
) -> SendMessage {
    let sk_prefix: String = sk.chars().take(32).collect();
    SendMessage {
        content: content.into(),
        recipient: reply_target.into(),
        subject: None,
        thread_ts,
        cancellation_token: None,
        attachments: vec![],
        image_urls: None,
        inline_buttons: Some(vec![
            InlineButton {
                label: BTN_RETRY.to_string(),
                callback_data: format!("__retry:{}", sk_prefix),
            },
            InlineButton {
                label: BTN_ABORT.to_string(),
                callback_data: format!("__abort:{}", sk_prefix),
            },
        ]),
    }
}

impl Orchestrator {
    /// Wake the parent agent on a `DelegationEvent` (sub-agent completion
    /// or failure). The synthesized system notification becomes a user
    /// message on the parent session; Agent.run handles the rest.
    ///
    /// Returns a boxed future because this function calls back into
    /// `handle_channel_event`, and Rust requires recursive async fns to
    /// be boxed.
    fn handle_delegation_event<'a>(
        &'a self,
        event: DelegationEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(self.handle_delegation_event_inner(event))
    }

    async fn handle_delegation_event_inner(&self, event: DelegationEvent) {
        let (task_id, parent_session_id, reply_target, content) = match event {
            DelegationEvent::Completed { task_id, parent_session_id, reply_target, summary, duration_secs } => {
                tracing::info!(task_id = %task_id, duration_secs, "delegation completed, waking main agent");
                let content = format!(
                    "[系统通知] 子代理已完成后台任务 (task_id: {}, 耗时: {}s)，结果如下：\n{}",
                    task_id, duration_secs, summary
                );
                (task_id, parent_session_id, reply_target, content)
            }
            DelegationEvent::Failed { task_id, parent_session_id, reply_target, error } => {
                tracing::warn!(task_id = %task_id, "delegation failed, waking main agent");
                let content = format!(
                    "[系统通知] 子代理后台任务失败 (task_id: {})，错误：\n{}",
                    task_id, error
                );
                (task_id, parent_session_id, reply_target, content)
            }
        };

        // Synthesize the delegation message as a full ChannelMessage so the
        // dispatch path is structurally identical to a real inbound. The
        // standard handle_channel_event Inbound arm runs Agent.
        //
        // handle_channel_event recomputes the session key from `msg.sender`,
        // so the synthetic.sender must equal the parent's original sender —
        // otherwise the synthetic message lands on a fresh session
        // ("channel:account:system") instead of the user's parent session.
        let (ct, ac, parent_sender) = match parse_session_key(&parent_session_id) {
            Some(t) => (t.0.to_string(), t.1.to_string(), t.2.to_string()),
            None => {
                tracing::warn!(parent = %parent_session_id, "invalid session key in delegation event");
                return;
            }
        };
        // Verify channel exists (warn-and-skip if not) — handle_channel_event
        // will look it up again from self.channels, but failing fast here gives
        // a clearer log line.
        if self.channels.get(&(ct.clone(), ac.clone())).is_none() {
            tracing::warn!(parent = %parent_session_id, "channel for delegation event not found");
            return;
        }
        let synthetic = ChannelMessage {
            id: format!("delegation:{}", task_id),
            sender: parent_sender,
            reply_target,
            content,
            timestamp: chrono::Utc::now().timestamp() as u64,
            thread_ts: None,
            interruption_scope_id: None,
            attachments: Vec::new(),
            image_urls: None,
            image_base64: None,
        };
        self.handle_channel_event(ChannelEvent::UserMessage(((ct, ac), synthetic))).await;
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

/// Execute one scheduled turn (heartbeat / cron) via Agent. Shared
/// helper extracted from run_heartbeat_task / run_cron_task. Returns
/// the assistant's text response, or an Err.
///
/// `session_key` is a routing_key — synthetic ("_heartbeat_<uuid>") for
/// heartbeats, real-looking for cron. The SessionContext comes from
/// `SessionManager.get_or_create_context` or, on first use, is built
/// with `session_override.run_mode = Background` so the prompt builder
/// knows this is unattended work.
async fn run_scheduled_turn(
    orch: &Orchestrator,
    session_key: &str,
    prompt: &str,
    model_override: Option<String>,
) -> anyhow::Result<String> {
    let model_override_for_init = model_override.clone();
    let session_ctx = orch.session_manager.get_or_create_context_with(
        session_key,
        move |session| {
            session.session_override.run_mode =
                Some(crate::config::agent::RunMode::Background);
            if let Some(m) = model_override_for_init {
                session.session_override.model = Some(m);
            }
        },
    );

    let runtime = orch.agent_runtime.clone();
    let prompt_config_base = orch.agent_runtime.defaults.prompt.clone();
    let skills_arc = Arc::clone(&orch.agent_runtime.skills);
    let cached_prompt = orch.agent_runtime.defaults.system_prompt.clone();
    let persist_hook: Arc<dyn PersistHook> = Arc::new(
        BackendPersistHook::new(Arc::clone(orch.session_manager.backend()))
    );

    let _turn_guard = session_ctx.turn_lock.lock().await;
    let mut session = session_ctx.session.lock().await;

    // Apply per-call model override.
    if let Some(m) = model_override.as_ref() {
        session.session_override.model = Some(m.clone());
    }

    // Wire transient handles for this turn (no channel — scheduled
    // tasks deliver via send_to_target_internal after the turn).
    session.persist = Some(persist_hook.clone());

    // Resolve TurnContext.
    let session_override = session.session_override.clone();
    let mut prompt_config = prompt_config_base.clone();
    if let Some(pm) = session_override.permission_mode {
        prompt_config.permission_mode = pm;
    }
    if let Some(rm) = session_override.run_mode {
        prompt_config.run_mode = rm;
    }
    let system_prompt = if !cached_prompt.is_empty() {
        cached_prompt
    } else {
        let s = skills_arc.read();
        crate::agents::SystemPromptBuilder::new(prompt_config.clone()).build(&s)
    };

    session.add_user(prompt.to_string());
    if let Some(last) = session.history.last().cloned() {
        if let Some(id) = persist_hook.persist_message(&session.id, &last) {
            if let Some(slot) = session.message_ids.last_mut() {
                *slot = id;
            }
        }
    }

    let thinking = session_override.to_thinking_config();
    let model_id = session_override.model.as_deref();
    let turn_ctx = crate::agents::TurnContext {
        system_prompt: &system_prompt,
        model_id,
        thinking: thinking.as_ref(),
        permission_mode: prompt_config.permission_mode,
        run_mode: prompt_config.run_mode,
    };

    let res = session_ctx.agent.run(&mut session, turn_ctx, &runtime).await;
    session.persist = None;
    res.map(|tr| tr.text)
}

/// Execute a heartbeat turn as an independent spawned task.
async fn run_heartbeat_task(
    orch: Arc<Orchestrator>,
    target_channel: Option<String>,
    target_account: Option<String>,
    prompt: String,
    due: Vec<super::heartbeat_tasks::HeartbeatTask>,
    mut state: super::heartbeat_tasks::HeartbeatState,
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
            send_to_target_internal(orch.channels.clone(), orch.last_channel.clone(), orch.last_recipient.clone(), target_channel, target_account, &response).await;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(err = %e, "heartbeat run failed");
        }
    }
}

/// Execute a cron job turn as an independent spawned task.
#[allow(clippy::too_many_arguments)]
async fn run_cron_task(
    orch: Arc<Orchestrator>,
    session_key: String,
    prompt: String,
    target_channel: Option<String>,
    target_account: Option<String>,
    job_id: String,
    _delivery: Option<crate::agents::scheduling::cron_types::DeliveryConfig>,
    _enabled_tools: Option<Vec<String>>,
    _disabled_tools: Option<Vec<String>>,
    model: Option<String>,
    _provider: Option<String>,
) {
    let start = std::time::Instant::now();
    let result = run_scheduled_turn(&orch, &session_key, &prompt, model.clone()).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    // Build run record and mark result in scheduler.
    let record = match &result {
        Ok(response) => {
            crate::agents::scheduling::cron_types::RunRecord::now(
                crate::agents::scheduling::cron_types::RunStatus::Ok,
            ).with_duration(duration_ms).with_output_preview(response)
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::warn!(session_key = %session_key, err = %err_str, "cron job failed");
            crate::agents::scheduling::cron_types::RunRecord::now(
                crate::agents::scheduling::cron_types::RunStatus::Error,
            ).with_duration(duration_ms).with_error(err_str)
        }
    };

    // Record run result in scheduler (returns failure alert message if needed).
    let failure_alert = if let Some(ref scheduler) = orch.scheduler {
        scheduler.mark_run_result(&job_id, record)
    } else {
        None
    };

    // Send output to target channel (on success with non-empty output).
    if let Ok(ref response) = result {
        if !response.trim().is_empty() {
            send_to_target_internal(
                orch.channels.clone(), orch.last_channel.clone(), orch.last_recipient.clone(),
                target_channel.clone(), target_account.clone(), response,
            ).await;
        }
    }

    // Send failure alert to channel if generated.
    if let Some(alert_msg) = failure_alert {
        tracing::warn!(job_id = %job_id, alert = %alert_msg, "sending failure alert");
        send_to_target_internal(
            orch.channels.clone(), orch.last_channel.clone(), orch.last_recipient.clone(),
            target_channel, target_account, &alert_msg,
        ).await;
    }
}

/// Send a response to the configured target channel (used by heartbeat).
async fn send_to_target_internal(
    channels: Arc<DashMap<(String, String), Arc<dyn Channel>>>,
    last_channel: Arc<tokio::sync::Mutex<Option<String>>>,
    last_recipient: Arc<tokio::sync::Mutex<Option<String>>>,
    target_channel: Option<String>,
    target_account: Option<String>,
    content: &str,
) {
    let (ch_type, acc_id) = match (target_channel, target_account) {
        (Some(ch), Some(acc)) => (ch, acc),
        (Some(ch), None) => (ch, "default".to_string()),
        (None, _) => {
            // Resolve from last_channel (format: "channel_type:account_id")
            let last = last_channel.lock().await.clone();
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

    let channel = match channels.get(&(ch_type.clone(), acc_id.clone())) {
        Some(ch) => ch.clone(),
        None => {
            tracing::warn!(channel = %ch_type, account = %acc_id, "target channel not found");
            return;
        }
    };

    // Resolve recipient from last_recipient.
    let recipient = last_recipient.lock().await.clone().unwrap_or_default();

    let msg = SendMessage {
        content: content.to_string(),
        recipient,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_key() {
        assert_eq!(
            Orchestrator::session_key("wechat", "default", "o9cq80zXpSX1Hz0ph_QNs591k4PA"),
            "wechat:default:o9cq80zXpSX1Hz0ph_QNs591k4PA"
        );
    }
}
