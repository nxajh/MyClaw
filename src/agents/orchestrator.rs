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
use crate::agents::sub_agent::SubAgentDelegator;
use crate::channels::{Channel, ChannelMessage, SendMessage, ProcessingStatus};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as TokioMutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agents::agent_impl::{Agent, AgentLoop, AskUserHandler, DelegateHandler};
use crate::agents::session_manager::{SessionManager, PersistHook, BackendPersistHook};

const CHANNEL_QUEUE_SIZE: usize = 100;
/// Timeout for ask_user waiting for user reply (5 minutes).
const ASK_USER_TIMEOUT: Duration = Duration::from_secs(300);

/// Internal enum for the run loop's select.
enum ChannelEvent {
    UserMessage((String, ChannelMessage)),
    Delegation(DelegationEvent),
}

/// Orchestrator — Application Service for message routing and session lifecycle.
///
/// Coordinates the flow: Channel → Session → AgentLoop → Channel.
/// Does NOT depend on any Infrastructure concrete types.
pub struct Orchestrator {
    /// Channels, keyed by name (e.g. "telegram", "wechat").
    channels: Arc<DashMap<String, Arc<dyn Channel>>>,
    /// Per-session agent loops: "channel:sender" → Arc<Mutex<AgentLoop>>.
    sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>>,
    agent: Agent,
    session_manager: Arc<SessionManager>,
    /// The message receiver, owned and consumed by run().
    #[allow(clippy::type_complexity)]
    msg_rx: Arc<TokioMutex<Option<mpsc::Receiver<(String, ChannelMessage)>>>>,
    /// Listener task handles — taken and awaited on shutdown.
    listener_handles: Vec<JoinHandle<()>>,
    /// Pending ask_user replies: session_key → (oneshot sender, reply_target).
    pending_asks: Arc<DashMap<String, (oneshot::Sender<String>, String)>>,
    /// Sub-agent delegator (for async delegation).
    sub_delegator: Option<Arc<SubAgentDelegator>>,
    /// Delegation manager (shared with DelegateTaskTool via handler).
    delegation_manager: Option<Arc<DelegationManager>>,
    /// Delegation event receiver.
    delegation_rx: Arc<TokioMutex<Option<mpsc::Receiver<DelegationEvent>>>>,
    /// Backend for session persistence (shared with persist hooks).
    persist_backend: Arc<dyn crate::storage::SessionBackend>,
    /// MCP manager (for /mcp command).
    mcp_manager: Option<Arc<crate::agents::McpManager>>,
    /// File change receiver for hot-reload.
    change_rx: Option<tokio::sync::watch::Receiver<crate::agents::ChangeSet>>,
    /// Last channel that received a user message (shared with schedulers).
    pub last_channel: Arc<tokio::sync::Mutex<Option<String>>>,
}

/// Resources shared between Orchestrator and scheduler tasks.
pub struct SharedSessions {
    pub sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>>,
    pub channels: Arc<DashMap<String, Arc<dyn Channel>>>,
    pub last_channel: Arc<tokio::sync::Mutex<Option<String>>>,
}

/// Parse a session key like "telegram:12345" into (channel_name, sender).
fn parse_session_key(sk: &str) -> Option<(&str, &str)> {
    let (ch, sender) = sk.split_once(':')?;
    if ch.is_empty() || sender.is_empty() {
        return None;
    }
    Some((ch, sender))
}

/// Fully-assembled components ready for the Orchestrator to use.
///
/// Built by the Composition Root (daemon.rs).  This struct is the seam that
/// decouples the Application layer from Infrastructure assembly logic.
pub struct OrchestratorParts {
    pub agent: Agent,
    pub session_manager: Arc<SessionManager>,
    /// Pre-built channels from Interface layer (Feature-gated at compile time).
    pub channels: Vec<(&'static str, Arc<dyn Channel>)>,
    /// Sub-agent delegator (conditional — only when sub-agents are configured).
    pub sub_delegator: Option<Arc<SubAgentDelegator>>,
    /// Delegation manager (conditional — only when sub-agents are configured).
    pub delegation_manager: Option<Arc<DelegationManager>>,
    /// Delegation event receiver (conditional).
    pub delegation_rx: Option<mpsc::Receiver<DelegationEvent>>,
    /// Backend for session persistence (shared with persist hooks).
    pub persist_backend: Arc<dyn crate::storage::SessionBackend>,
    /// MCP manager (conditional — only when MCP servers are configured).
    pub mcp_manager: Option<Arc<crate::agents::McpManager>>,
    /// File change receiver for hot-reload (shared across all AgentLoops).
    pub change_rx: Option<tokio::sync::watch::Receiver<crate::agents::ChangeSet>>,
}

impl Orchestrator {
    /// Create a new Orchestrator from pre-assembled parts.
    ///
    /// The Composition Root is responsible for building `OrchestratorParts`
    /// (creating Registry, registering Providers/Tools, opening Storage, etc.).
    pub fn new(parts: OrchestratorParts) -> (Self, mpsc::Sender<(String, ChannelMessage)>) {
        let (msg_tx, msg_rx) = mpsc::channel(CHANNEL_QUEUE_SIZE);
        let msg_tx = Arc::new(msg_tx);

        let channels_map: Arc<DashMap<String, Arc<dyn Channel>>> = Arc::new(DashMap::new());
        let mut listener_handles = Vec::new();

        for (name, channel) in &parts.channels {
            let name_static: &'static str = name;
            let handle = Self::spawn_listener(name_static, Arc::clone(channel), Arc::clone(&msg_tx));
            channels_map.insert((*name).to_string(), Arc::clone(channel));
            listener_handles.push(handle);
            info!(channel = %name, "listener started");
        }

        if channels_map.is_empty() {
            warn!("no channels enabled");
        }

        let orchestrator = Orchestrator {
            channels: channels_map,
            sessions: Arc::new(DashMap::new()),
            agent: parts.agent,
            session_manager: parts.session_manager,
            msg_rx: Arc::new(TokioMutex::new(Some(msg_rx))),
            listener_handles,
            pending_asks: Arc::new(DashMap::new()),
            sub_delegator: parts.sub_delegator,
            delegation_manager: parts.delegation_manager,
            delegation_rx: Arc::new(TokioMutex::new(parts.delegation_rx)),
            persist_backend: parts.persist_backend,
            mcp_manager: parts.mcp_manager,
            change_rx: parts.change_rx,
            last_channel: Arc::new(tokio::sync::Mutex::new(None)),
        };

        info!(channels = orchestrator.channels.len(), "orchestrator initialized");
        (orchestrator, (*msg_tx).clone())
    }

    /// Get shared resources for scheduler tasks.
    pub fn shared(&self) -> SharedSessions {
        SharedSessions {
            sessions: self.sessions.clone(),
            channels: self.channels.clone(),
            last_channel: self.last_channel.clone(),
        }
    }

    fn spawn_listener(
        channel_name: &'static str,
        channel: Arc<dyn Channel>,
        msg_tx: Arc<mpsc::Sender<(String, ChannelMessage)>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let rx = match channel.listen().await {
                Ok(r) => r,
                Err(e) => {
                    error!(channel = %channel_name, err = %e, "listen failed");
                    return;
                }
            };
            let mut rx = rx;
            while let Some(msg) = rx.recv().await {
                if msg_tx.send((channel_name.to_string(), msg)).await.is_err() {
                    break;
                }
            }
            info!(channel = %channel_name, "listener ended");
        })
    }

    fn session_key(channel: &str, sender: &str) -> String {
        format!("{channel}:{sender}")
    }

    #[allow(clippy::too_many_arguments)]
    fn get_or_create_loop(
        sessions: &DashMap<String, Arc<TokioMutex<AgentLoop>>>,
        agent: &Agent,
        session_manager: &SessionManager,
        sk: &str,
        channels: &DashMap<String, Arc<dyn Channel>>,
        pending_asks: &Arc<DashMap<String, (oneshot::Sender<String>, String)>>,
        reply_target: &str,
        sub_delegator: &Option<Arc<SubAgentDelegator>>,
        delegation_manager: &Option<Arc<DelegationManager>>,
        persist_backend: &Arc<dyn crate::storage::SessionBackend>,
        change_rx: &Option<tokio::sync::watch::Receiver<crate::agents::ChangeSet>>,
    ) -> Arc<TokioMutex<AgentLoop>> {
        if let Some(existing) = sessions.get(sk) {
            return existing.clone();
        }
        let session = session_manager.get_or_create(sk);

        // Create persist hook from the shared backend.
        let persist_hook: Arc<dyn PersistHook> = Arc::new(
            BackendPersistHook::new(Arc::clone(persist_backend))
        );
        let loop_ = agent.loop_for_with_persist(session, Some(persist_hook));

        // Wire up the ask_user handler.
        let channels = channels.clone();
        let pending_asks = pending_asks.clone();
        let reply_target_owned = reply_target.to_string();
        let user_facing_key = sk.to_string();
        let handler: AskUserHandler = Arc::new(move |session_key: String, question: String| {
            let channels = channels.clone();
            let pending_asks = pending_asks.clone();
            let reply_target = reply_target_owned.clone();
            let user_facing_key = user_facing_key.clone();
            Box::pin(async move {
                // 1. Send the question through the channel.
                let (ch_name, _) = parse_session_key(&session_key)
                    .ok_or_else(|| anyhow::anyhow!("invalid session key: {}", session_key))?;

                let channel: Arc<dyn Channel> = channels
                    .get(ch_name)
                    .map(|r| r.clone())
                    .ok_or_else(|| anyhow::anyhow!("channel '{}' not found", ch_name))?;

                let send_msg = SendMessage::new(&question, &reply_target);
                channel.send(&send_msg).await?;

                // 2. Create a oneshot channel and register as pending.
                //    Use the user-facing key so the run loop can find it.
                let (tx, rx) = oneshot::channel();
                pending_asks.insert(user_facing_key, (tx, reply_target.clone()));

                // 3. Wait for the user's reply (delivered by the run loop) with timeout.
                let answer = tokio::time::timeout(ASK_USER_TIMEOUT, rx)
                    .await
                    .map_err(|_| anyhow::anyhow!("ask_user timed out waiting for user reply"))?
                    .map_err(|_| anyhow::anyhow!("ask_user cancelled (dropped)"))?;

                Ok(answer)
            })
        });

        let mut loop_ = loop_.with_ask_user_handler(handler);

        // Wire up the delegate handler (async delegation).
        if let (Some(delegator), Some(manager)) = (sub_delegator.clone(), delegation_manager.clone()) {
            // Use the session *key* (e.g. "telegram:12345") not session.id — the
            // sessions DashMap is keyed by session_key, and handle_delegation_event
            // looks up the entry with exactly this value.
            let session_key_for_delegate = sk.to_string();
            let reply_target_for_delegate = reply_target.to_string();
            let delegate_handler: DelegateHandler = Arc::new(
                move |agent_name: String, task: String| {
                    delegator.delegate_async(
                        &agent_name,
                        &task,
                        &session_key_for_delegate,
                        &reply_target_for_delegate,
                        &manager,
                    )
                }
            );
            loop_ = loop_.with_delegate_handler(delegate_handler);
        }

        // Wire up the sub-agent delegator for compaction summarization.
        if let Some(delegator) = sub_delegator.clone() {
            loop_ = loop_.with_sub_delegator(delegator);
        }

        // Wire up the file change receiver for hot-reload.
        if let Some(rx) = change_rx.clone() {
            loop_ = loop_.with_change_rx(rx);
        }

        let entry: Arc<TokioMutex<AgentLoop>> = Arc::new(TokioMutex::new(loop_));
        sessions.insert(sk.into(), entry.clone());
        entry
    }

    /// Main message loop. Consumes self.msg_rx.
    /// Call from the Composition Root; blocks until shutdown.
    pub async fn run(&self, mut shutdown_rx: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()> {
        let rx = {
            let mut guard = self.msg_rx.lock().await;
            guard.take().context("run() already called or msg_rx was None")?
        };

        // Take the delegation event receiver if available.
        let mut delegation_rx = {
            let mut guard = self.delegation_rx.lock().await;
            guard.take()
        };

        let sessions = self.sessions.clone();
        let agent = self.agent.clone();
        let channels = self.channels.clone();
        let sub_delegator = self.sub_delegator.clone();
        let delegation_manager = self.delegation_manager.clone();

        let mut rx = rx;

        loop {
            if *shutdown_rx.borrow() {
                tracing::info!("shutdown requested, exiting message loop");
                break;
            }

            let event = if delegation_rx.is_some() {
                // select over user messages + delegation events + shutdown
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(m) => ChannelEvent::UserMessage(m),
                        None => break,
                    },
                    event = delegation_rx.as_mut().unwrap().recv() => {
                        match event {
                            Some(e) => ChannelEvent::Delegation(e),
                            None => {
                                // Delegation channel closed, stop listening for it.
                                delegation_rx = None;
                                continue;
                            }
                        }
                    },
                    _ = shutdown_rx.changed() => {
                        tracing::info!("shutdown signal received");
                        break;
                    }
                }
            } else {
                // No delegation events — only user messages + shutdown
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(m) => ChannelEvent::UserMessage(m),
                        None => break,
                    },
                    _ = shutdown_rx.changed() => {
                        tracing::info!("shutdown signal received");
                        break;
                    }
                }
            };

            match event {
                ChannelEvent::UserMessage((channel_name, msg)) => {
                    // Track last channel for scheduler target resolution.
                    {
                        let mut lc = self.last_channel.lock().await;
                        *lc = Some(channel_name.clone());
                    }

                    let sk = Self::session_key(&channel_name, &msg.sender);

                    // Check if this is a reply to a pending ask_user.
                    if let Some((_, (tx, _))) = self.pending_asks.remove(&sk) {
                        // Deliver the user's answer to the waiting ask_user handler.
                        if tx.send(msg.content.clone()).is_err() {
                            warn!(session = %sk, "ask_user oneshot already closed");
                        }
                        // Do NOT spawn a new agent loop — the existing one is waiting.
                        continue;
                    }

                    let content = msg.content.clone();
                    let image_urls = msg.image_urls.clone();
                    let image_base64 = msg.image_base64.clone();
                    let reply_target = msg.reply_target.clone();
                    let reply_to_id = Some(msg.id.clone());
                    let channel_name_clone = channel_name.clone();

                    // Intercept slash commands before reaching agent loop.
                    if let Some((cmd, cmd_args)) = super::slash_command::parse_command(&content) {
                        let session_loop = sessions.get(&sk).map(|r| r.clone());
                        let cmd_ctx = super::slash_command::CommandContext {
                            user_id: &sk,
                            registry: agent.registry(),
                            session_manager: self.session_manager.as_ref(),
                            agent: &agent,
                            agent_loop: session_loop.as_ref(),
                            mcp_manager: self.mcp_manager.as_ref(),
                            sessions: &self.sessions,
                        };
                        if let Some(response) = super::slash_command::dispatch(cmd, cmd_args, cmd_ctx).await {
                            // Send command response directly, skip agent loop.
                            let ch = channels.clone();
                            tokio::spawn(async move {
                                let channel: Option<Arc<dyn Channel>> = {
                                    ch.get(&channel_name_clone).map(|r| r.clone())
                                };
                                if let Some(channel) = channel {
                                    let send_msg = SendMessage {
                                        recipient: reply_target,
                                        content: response,
                                        subject: None,
                                        thread_ts: reply_to_id.clone(),
                                        cancellation_token: None,
                                        attachments: vec![],
                                        image_urls: None,
                                    };
                                    if let Err(e) = channel.send(&send_msg).await {
                                        error!(session = %sk, err = %e, "command response send failed");
                                    }
                                }
                            });
                            continue;
                        }
                    }

                    let loop_ = Self::get_or_create_loop(
                        &sessions, &agent, self.session_manager.as_ref(), &sk,
                        &channels, &self.pending_asks, &reply_target,
                        &sub_delegator, &delegation_manager,
                        &self.persist_backend,
                        &self.change_rx,
                    );

                    let ch = channels.clone();
                    tokio::spawn(async move {
                        // Resolve channel early so we can send the response.
                        let channel: Option<Arc<dyn Channel>> = {
                            ch.get(&channel_name_clone).map(|r| r.clone())
                        };
                        let channel = match channel {
                            Some(c) => c,
                            None => return,
                        };

                        // Notify channel that processing has started.
                        channel.on_status(&reply_target, ProcessingStatus::Thinking).await;

                        // ClientChannel uses streaming path: run_streamed() + TurnEvent forwarding.
                        // Other channels use the existing run() + channel.send() path.
                        let is_client = channel_name_clone == "client";

                        if is_client {
                            // Streaming path for ClientChannel.
                            let stream_ctx = channel.take_stream_context(&reply_target);
                            let (event_tx, cancel) = match stream_ctx {
                                Some(ctx) => ctx,
                                None => {
                                    tracing::warn!(session = %sk, "no stream context for client session, falling back to run()");
                                    let mut guard = loop_.lock().await;
                                    let _ = guard.run(&content, image_urls, image_base64).await;
                                    return;
                                }
                            };

                            let response = {
                                let mut guard = loop_.lock().await;
                                guard.run_streamed(&content, image_urls, image_base64, event_tx, cancel).await
                            };

                            match response {
                                Ok(_text) => {
                                    // Text already sent via TurnEvent::Done.
                                    // Send TurnEvent::Error if needed (handled by run_streamed internally).
                                    channel.on_status(&reply_target, ProcessingStatus::Done).await;
                                }
                                Err(e) => {
                                    channel.on_status(&reply_target, ProcessingStatus::Error).await;
                                    error!(session = %sk, err = %e, "streamed turn failed");
                                    // Error already sent via channel.send() is not needed
                                    // because the WS handler's forwarding task will have ended
                                    // and the client can detect the stream ended without Done.
                                }
                            }
                        } else {
                            // Existing non-streaming path.
                            let response = {
                                let mut guard = loop_.lock().await;
                                guard.run(&content, image_urls, image_base64).await
                            };

                            match response {
                                Ok(text) if !text.is_empty() => {
                                    tracing::info!(session = %sk, text_len = text.len(), "sending response");
                                    let send_msg = SendMessage {
                                        recipient: reply_target.clone(),
                                        content: text,
                                        subject: None,
                                        thread_ts: reply_to_id.clone(),
                                        cancellation_token: None,
                                        attachments: vec![],
                                        image_urls: None,
                                    };
                                    if let Err(e) = channel.send(&send_msg).await {
                                        error!(session = %sk, err = %e, "send failed");
                                    }
                                    channel.on_status(&reply_target, ProcessingStatus::Done).await;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    // Notify channel about the error.
                                    channel.on_status(&reply_target, ProcessingStatus::Error).await;
                                    // Send error message to user so they know what happened.
                                    let error_text = format!(
                                        "⚠️ 处理消息时发生错误：\n\n`{}`\n\n请稍后重试，或联系管理员。",
                                        e
                                    );
                                    error!(session = %sk, err = %e, "loop failed, notifying user");
                                    let send_msg = SendMessage {
                                        recipient: reply_target,
                                        content: error_text,
                                        subject: None,
                                        thread_ts: reply_to_id.clone(),
                                        cancellation_token: None,
                                        attachments: vec![],
                                        image_urls: None,
                                    };
                                    if let Err(send_err) = channel.send(&send_msg).await {
                                        error!(session = %sk, err = %send_err, "failed to send error notification to user");
                                    }
                                }
                            }
                        }
                    });
                }
                ChannelEvent::Delegation(event) => {
                    self.handle_delegation_event(event).await;
                }
            }
        }

        info!("all listeners stopped, exiting");
        Ok(())
    }

    /// Handle a delegation event from a background sub-agent.
    async fn handle_delegation_event(&self, event: DelegationEvent) {
        match event {
            DelegationEvent::Completed { task_id, session_key, reply_target, summary } => {
                tracing::info!(task_id = %task_id, "delegation completed, waking main agent");

                let loop_ = match self.sessions.get(&session_key) {
                    Some(l) => l.clone(),
                    None => {
                        tracing::warn!(session = %session_key, "session not found for delegation event");
                        return;
                    }
                };

                // Construct synthetic message to wake the main agent.
                let synthetic_msg = format!(
                    "[系统通知] 子代理已完成后台任务 (task_id: {})，结果如下：\n{}",
                    task_id, summary
                );

                // Run the main agent with the synthetic message.
                let response = {
                    let mut guard = loop_.lock().await;
                    guard.run(&synthetic_msg, None, None).await
                };

                // Send the main agent's response to the user.
                match response {
                    Ok(text) if !text.is_empty() => {
                        let (ch_name, _) = match parse_session_key(&session_key) {
                            Some(pair) => pair,
                            None => {
                                tracing::warn!(session = %session_key, "invalid session key in delegation event");
                                return;
                            }
                        };
                        if let Some(channel) = self.channels.get(ch_name) {
                            let send_msg = SendMessage {
                                recipient: reply_target,
                                content: text,
                                subject: None,
                                thread_ts: None,
                                cancellation_token: None,
                                attachments: vec![],
                                image_urls: None,
                            };
                            if let Err(e) = channel.send(&send_msg).await {
                                tracing::error!(session = %session_key, err = %e, "send delegation result failed");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(session = %session_key, err = %e, "main agent failed to process delegation result");
                    }
                }
            }
            DelegationEvent::Failed { task_id, session_key, reply_target, error } => {
                tracing::warn!(task_id = %task_id, "delegation failed, waking main agent");

                let loop_ = match self.sessions.get(&session_key) {
                    Some(l) => l.clone(),
                    None => {
                        tracing::warn!(session = %session_key, "session not found for delegation event");
                        return;
                    }
                };

                let synthetic_msg = format!(
                    "[系统通知] 子代理后台任务失败 (task_id: {})，错误：\n{}",
                    task_id, error
                );

                let response = {
                    let mut guard = loop_.lock().await;
                    guard.run(&synthetic_msg, None, None).await
                };

                match response {
                    Ok(text) if !text.is_empty() => {
                        let (ch_name, _) = match parse_session_key(&session_key) {
                            Some(pair) => pair,
                            None => {
                                tracing::warn!(session = %session_key, "invalid session key in delegation event");
                                return;
                            }
                        };
                        if let Some(channel) = self.channels.get(ch_name) {
                            let send_msg = SendMessage {
                                recipient: reply_target,
                                content: text,
                                subject: None,
                                thread_ts: None,
                                cancellation_token: None,
                                attachments: vec![],
                                image_urls: None,
                            };
                            if let Err(e) = channel.send(&send_msg).await {
                                tracing::error!(session = %session_key, err = %e, "send delegation result failed");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(session = %session_key, err = %e, "main agent failed to process delegation failure");
                    }
                }
            }
        }
    }

    /// Abort all listener handles (call after run() returns).
    pub async fn shutdown_listeners(&mut self) {
        let handles = std::mem::take(&mut self.listener_handles);
        for h in handles {
            h.abort();
        }
        tracing::info!("all listener tasks aborted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_key() {
        assert_eq!(
            Orchestrator::session_key("wechat", "o9cq80zXpSX1Hz0ph_QNs591k4PA"),
            "wechat:o9cq80zXpSX1Hz0ph_QNs591k4PA"
        );
    }
}
