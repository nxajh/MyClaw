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
use crate::channels::{Channel, ChannelMessage, SendMessage};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as TokioMutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agents::agent_impl::{Agent, AgentLoop, AskUserHandler}; use crate::agents::session_manager::SessionManager;

const CHANNEL_QUEUE_SIZE: usize = 100;
/// Timeout for ask_user waiting for user reply (5 minutes).
const ASK_USER_TIMEOUT: Duration = Duration::from_secs(300);

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
    session_manager: SessionManager,
    /// The message receiver, owned and consumed by run().
    #[allow(clippy::type_complexity)]
    msg_rx: Arc<TokioMutex<Option<mpsc::Receiver<(String, ChannelMessage)>>>>,
    /// Listener task handles — taken and awaited on shutdown.
    listener_handles: Vec<JoinHandle<()>>,
    /// Pending ask_user replies: session_key → (oneshot sender, reply_target).
    pending_asks: Arc<DashMap<String, (oneshot::Sender<String>, String)>>,
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
    pub session_manager: SessionManager,
    /// Pre-built channels from Interface layer (Feature-gated at compile time).
    pub channels: Vec<(&'static str, Arc<dyn Channel>)>,
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
        };

        info!(channels = orchestrator.channels.len(), "orchestrator initialized");
        (orchestrator, (*msg_tx).clone())
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

    fn get_or_create_loop(
        sessions: &DashMap<String, Arc<TokioMutex<AgentLoop>>>,
        agent: &Agent,
        session_manager: &SessionManager,
        sk: &str,
        channels: &DashMap<String, Arc<dyn Channel>>,
        pending_asks: &Arc<DashMap<String, (oneshot::Sender<String>, String)>>,
        reply_target: &str,
    ) -> Arc<TokioMutex<AgentLoop>> {
        if let Some(existing) = sessions.get(sk) {
            return existing.clone();
        }
        let session = session_manager.get_or_create(sk);
        let loop_ = agent.loop_for(session);

        // Wire up the ask_user handler.
        let channels = channels.clone();
        let pending_asks = pending_asks.clone();
        let reply_target_owned = reply_target.to_string();
        let handler: AskUserHandler = Arc::new(move |session_key: String, question: String| {
            let channels = channels.clone();
            let pending_asks = pending_asks.clone();
            let reply_target = reply_target_owned.clone();
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
                let (tx, rx) = oneshot::channel();
                pending_asks.insert(session_key.clone(), (tx, reply_target.clone()));

                // 3. Wait for the user's reply (delivered by the run loop) with timeout.
                let answer = tokio::time::timeout(ASK_USER_TIMEOUT, rx)
                    .await
                    .map_err(|_| anyhow::anyhow!("ask_user timed out waiting for user reply"))?
                    .map_err(|_| anyhow::anyhow!("ask_user cancelled (dropped)"))?;

                Ok(answer)
            })
        });

        let loop_ = loop_.with_ask_user_handler(handler);
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

        let sessions = self.sessions.clone();
        let agent = self.agent.clone();
        let channels = self.channels.clone();

        let mut rx = rx;

        loop {
            if *shutdown_rx.borrow() {
                tracing::info!("shutdown requested, exiting message loop");
                break;
            }

            let msg = tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => m,
                    None => break,
                },
                _ = shutdown_rx.changed() => {
                    tracing::info!("shutdown signal received");
                    break;
                }
            };

            let (channel_name, msg) = msg;
            let sk = Self::session_key(&channel_name, &msg.sender);

            // Check if this is a reply to a pending ask_user.
            if let Some((_, ((tx, _)))) = self.pending_asks.remove(&sk) {
                // Deliver the user's answer to the waiting ask_user handler.
                if tx.send(msg.content.clone()).is_err() {
                    warn!(session = %sk, "ask_user oneshot already closed");
                }
                // Do NOT spawn a new agent loop — the existing one is waiting.
                continue;
            }

            let content = msg.content.clone();
            let reply_target = msg.reply_target.clone();
            let channel_name_clone = channel_name.clone();
            let loop_ = Self::get_or_create_loop(
                &sessions, &agent, &self.session_manager, &sk, &channels, &self.pending_asks, &reply_target,
            );

            let ch = channels.clone();
            tokio::spawn(async move {
                let response = {
                    let mut guard = loop_.lock().await;
                    guard.run(&content).await
                };

                let channel: Option<Arc<dyn Channel>> = {
                    ch.get(&channel_name_clone).map(|r| r.clone())
                };

                let channel = match channel {
                    Some(c) => c,
                    None => return,
                };

                match response {
                    Ok(text) if !text.is_empty() => {
                        tracing::info!(session = %sk, text_len = text.len(), "sending response");
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
                            error!(session = %sk, err = %e, "send failed");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!(session = %sk, err = %e, "loop failed");
                    }
                }
            });
        }

        info!("all listeners stopped, exiting");
        Ok(())
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
