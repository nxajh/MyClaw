//! ClientChannel — WebSocket-based channel for TUI and Web UI clients.
//!
//! Unlike other channels (Telegram, QQBot) where MyClaw is a *client* connecting
//! to an external platform, ClientChannel runs a WebSocket *server* that TUI and
//! Web UI clients connect to.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::agents::TurnEvent;
use crate::channels::message::{Channel, ChannelMessage, SendMessage};
use crate::config::channel::ClientConfig;

// ── Stream Context ──────────────────────────────────────────────────────────

/// Per-session streaming state, stored in ClientChannel.
struct StreamContext {
    event_tx: mpsc::Sender<TurnEvent>,
    cancel: CancellationToken,
}

// ── Client Connection ───────────────────────────────────────────────────────

/// A single connected client.
struct ClientConnection {
    /// WebSocket sender (clone of the split sink, wrapped as mpsc for simplicity).
    ws_sender: mpsc::Sender<String>,
    /// Current active session key for this connection.
    active_session: String,
    /// Set of session keys owned by this connection.
    sessions: std::collections::HashSet<String>,
}

// ── ClientChannel ───────────────────────────────────────────────────────────

pub struct ClientChannel {
    config: ClientConfig,
    /// Outgoing messages for Orchestrator (filled by WS handlers).
    message_tx: mpsc::Sender<ChannelMessage>,
    /// One-time take for listen().
    message_rx: Mutex<Option<mpsc::Receiver<ChannelMessage>>>,
    /// Per-session streaming context.
    stream_contexts: Arc<RwLock<HashMap<String, StreamContext>>>,
    /// Active connections: connection_id → ClientConnection.
    connections: Arc<RwLock<HashMap<String, ClientConnection>>>,
    /// Reverse map: session_key → connection_id.
    session_owners: Arc<RwLock<HashMap<String, String>>>,
}

impl ClientChannel {
    pub fn new(config: ClientConfig) -> Self {
        let (message_tx, message_rx) = mpsc::channel(100);
        Self {
            config,
            message_tx,
            message_rx: Mutex::new(Some(message_rx)),
            stream_contexts: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(HashMap::new())),
            session_owners: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start the WebSocket server (spawns a background task).
    /// Called lazily from listen() — the first time the Orchestrator starts consuming.
    async fn start(&self) -> anyhow::Result<()> {
        let addr: SocketAddr = self.config.bind.parse()
            .map_err(|e| anyhow::anyhow!("invalid client bind address '{}': {}", self.config.bind, e))?;
        let listener = TcpListener::bind(addr).await
            .map_err(|e| anyhow::anyhow!("failed to bind WebSocket server to {}: {}", addr, e))?;

        let max_connections = self.config.max_connections;
        let auth_token = self.config.auth_token.clone();
        let message_tx = self.message_tx.clone();
        let stream_contexts = self.stream_contexts.clone();
        let connections = self.connections.clone();
        let session_owners = self.session_owners.clone();

        tracing::info!("WebSocket server listening on ws://{}/myclaw", addr);

        tokio::spawn(async move {
            let mut connection_count = 0u32;
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        // Check connection limit
                        {
                            let conns = connections.read();
                            if conns.len() >= max_connections as usize {
                                tracing::warn!(
                                    peer = %peer_addr,
                                    connections = conns.len(),
                                    max = max_connections,
                                    "rejecting WebSocket connection: limit reached"
                                );
                                continue;
                            }
                        }

                        // Validate auth token via HTTP header (peek at the upgrade request).
                        // tokio-tungstenite's accept_async doesn't expose headers,
                        // so for Phase 1 we skip header-based auth and rely on
                        // the first message being an auth message, or accept
                        // connections from localhost only (bind defaults to 127.0.0.1).
                        let ws_result = tokio_tungstenite::accept_async(stream).await;
                        let ws_stream = match ws_result {
                            Ok(ws) => ws,
                            Err(e) => {
                                tracing::warn!(peer = %peer_addr, error = %e, "WebSocket handshake failed");
                                continue;
                            }
                        };

                        connection_count += 1;
                        let conn_id = format!("ws-{}", connection_count);
                        let session_key = format!("client:{}", conn_id);

                        let (mut ws_sink, mut ws_stream) = ws_stream.split();

                        // Create mpsc channel for outgoing messages to this client.
                        let (ws_sender, mut ws_receiver) = mpsc::channel::<String>(64);

                        // Register connection.
                        {
                            let mut conns = connections.write();
                            conns.insert(conn_id.clone(), ClientConnection {
                                ws_sender: ws_sender.clone(),
                                active_session: session_key.clone(),
                                sessions: {
                                    let mut set = std::collections::HashSet::new();
                                    set.insert(session_key.clone());
                                    set
                                },
                            });
                            let mut owners = session_owners.write();
                            owners.insert(session_key.clone(), conn_id.clone());
                        }

                        let conn_id_clone = conn_id.clone();
                        let session_key_clone = session_key.clone();
                        let message_tx_clone = message_tx.clone();
                        let stream_contexts_clone = stream_contexts.clone();
                        let connections_clone = connections.clone();
                        let session_owners_clone = session_owners.clone();
                        let auth_token_clone = auth_token.clone();

                        tracing::info!(
                            conn_id = %conn_id,
                            peer = %peer_addr,
                            session = %session_key,
                            "WebSocket client connected"
                        );

                        // Spawn per-connection handler.
                        tokio::spawn(async move {
                            // Outgoing message forwarder: ws_receiver → WebSocket sink.
                            let outgoing = async {
                                while let Some(text) = ws_receiver.recv().await {
                                    if ws_sink.send(Message::Text(text.into())).await.is_err() {
                                        break;
                                    }
                                }
                                let _ = ws_sink.close().await;
                            };

                            // Incoming message handler: WebSocket stream → message_tx.
                            let incoming = async {
                                while let Some(msg_result) = futures_util::StreamExt::next(&mut ws_stream).await {
                                    let msg = match msg_result {
                                        Ok(Message::Text(text)) => text.to_string(),
                                        Ok(Message::Close(_)) => break,
                                        Ok(_) => continue, // Ignore binary, ping, pong
                                        Err(e) => {
                                            tracing::warn!(conn_id = %conn_id_clone, error = %e, "WebSocket read error");
                                            break;
                                        }
                                    };

                                    // Parse the incoming JSON message.
                                    let parsed: serde_json::Value = match serde_json::from_str(&msg) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let err = serde_json::json!({"type":"error","message":format!("invalid JSON: {}", e)});
                                            let _ = ws_sender.send(err.to_string()).await;
                                            continue;
                                        }
                                    };

                                    let msg_type = parsed["type"].as_str().unwrap_or("");

                                    match msg_type {
                                        "message" => {
                                            let content = parsed["content"].as_str().unwrap_or("").to_string();
                                            if content.is_empty() {
                                                let err = serde_json::json!({"type":"error","message":"empty content"});
                                                let _ = ws_sender.send(err.to_string()).await;
                                                continue;
                                            }

                                            // Create streaming context.
                                            let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(64);
                                            let cancel = CancellationToken::new();

                                            // Store in stream_contexts.
                                            {
                                                let mut contexts = stream_contexts_clone.write();
                                                contexts.insert(session_key_clone.clone(), StreamContext {
                                                    event_tx: event_tx.clone(),
                                                    cancel: cancel.clone(),
                                                });
                                            }

                                            // Spawn event forwarder: event_rx → ws_sender.
                                            let fwd_sender = ws_sender.clone();
                                            let fwd_session = session_key_clone.clone();
                                            let fwd_contexts = stream_contexts_clone.clone();
                                            tokio::spawn(async move {
                                                while let Some(event) = event_rx.recv().await {
                                                    let json = match serde_json::to_string(&event) {
                                                        Ok(j) => j,
                                                        Err(e) => {
                                                            tracing::warn!("failed to serialize TurnEvent: {}", e);
                                                            continue;
                                                        }
                                                    };
                                                    if fwd_sender.send(json).await.is_err() {
                                                        break; // Client gone
                                                    }
                                                }
                                                // Clean up stream context when forwarding ends.
                                                {
                                                    let mut contexts = fwd_contexts.write();
                                                    contexts.remove(&fwd_session);
                                                }
                                            });

                                            // Create ChannelMessage for Orchestrator.
                                            let channel_msg = ChannelMessage {
                                                id: format!("{}-{}", conn_id_clone, chrono::Utc::now().timestamp_millis()),
                                                sender: conn_id_clone.clone(),
                                                reply_target: session_key_clone.clone(),
                                                content,
                                                channel: "client".to_string(),
                                                timestamp: chrono::Utc::now().timestamp() as u64,
                                                thread_ts: None,
                                                interruption_scope_id: None,
                                                attachments: vec![],
                                                image_urls: None,
                                                image_base64: None,
                                            };

                                            if message_tx_clone.send(channel_msg).await.is_err() {
                                                tracing::warn!("Orchestrator message channel closed");
                                                break;
                                            }
                                        }

                                        "cancel" => {
                                            // Cancel current turn.
                                            let contexts = stream_contexts_clone.read();
                                            if let Some(ctx) = contexts.get(&session_key_clone) {
                                                ctx.cancel.cancel();
                                                tracing::info!(session = %session_key_clone, "turn cancelled by client");
                                            }
                                        }

                                        "api" => {
                                            // Management API — Phase 1: basic framework.
                                            let id = parsed["id"].as_str().unwrap_or("").to_string();
                                            let method = parsed["method"].as_str().unwrap_or("").to_string();

                                            // TODO: implement api_router with sessions.*
                                            let resp = serde_json::json!({
                                                "type": "api_error",
                                                "id": id,
                                                "error": format!("method not implemented: {}", method)
                                            });
                                            let _ = ws_sender.send(resp.to_string()).await;
                                        }

                                        "ping" => {
                                            let _ = ws_sender.send(r#"{"type":"pong"}"#.to_string()).await;
                                        }

                                        _ => {
                                            let err = serde_json::json!({
                                                "type": "error",
                                                "message": format!("unknown message type: {}", msg_type)
                                            });
                                            let _ = ws_sender.send(err.to_string()).await;
                                        }
                                    }
                                }
                            };

                            // Run both directions concurrently.
                            tokio::select! {
                                _ = outgoing => {},
                                _ = incoming => {},
                            }

                            // Clean up on disconnect.
                            {
                                let conns = connections_clone.read();
                                if let Some(conn) = conns.get(&conn_id_clone) {
                                    let mut owners = session_owners_clone.write();
                                    for sk in &conn.sessions {
                                        owners.remove(sk);
                                    }
                                }
                                drop(conns);
                                let mut conns = connections_clone.write();
                                conns.remove(&conn_id_clone);
                            }

                            // Cancel any pending turn.
                            {
                                let contexts = stream_contexts_clone.read();
                                if let Some(ctx) = contexts.get(&session_key_clone) {
                                    ctx.cancel.cancel();
                                }
                            }

                            tracing::info!(conn_id = %conn_id_clone, "WebSocket client disconnected");
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to accept WebSocket connection");
                    }
                }
            }
        });

        Ok(())
    }
}

#[async_trait]
impl Channel for ClientChannel {
    fn name(&self) -> &str {
        "client"
    }

    async fn send(&self, msg: &SendMessage) -> anyhow::Result<()> {
        // msg.recipient is the session_key (e.g. "client:ws-1")
        // Find the connection that owns this session.
        let ws_sender = {
            let owners = self.session_owners.read();
            let conn_id = match owners.get(&msg.recipient) {
                Some(id) => id.clone(),
                None => {
                    tracing::warn!(recipient = %msg.recipient, "no connection found for session");
                    return Ok(());
                }
            };
            drop(owners); // Release lock before await.

            let conns = self.connections.read();
            match conns.get(&conn_id) {
                Some(conn) => Some(conn.ws_sender.clone()),
                None => None,
            }
        }; // Lock released here.

        if let Some(sender) = ws_sender {
            let outgoing = serde_json::json!({
                "type": "message",
                "session": msg.recipient,
                "content": msg.content,
            });
            let _ = sender.send(outgoing.to_string()).await;
        }
        Ok(())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>> {
        // Lazily start the WebSocket server on first listen() call.
        self.start().await?;
        let rx = self.message_rx.lock().await
            .take()
            .ok_or_else(|| anyhow::anyhow!("listen() called more than once on ClientChannel"))?;
        Ok(rx)
    }

    async fn health_check(&self) -> bool {
        true // Local WebSocket server is always healthy.
    }

    fn prepare_stream(
        &self,
        _session_key: &str,
        _ws_sender: mpsc::Sender<String>,
    ) -> Option<(mpsc::Sender<TurnEvent>, CancellationToken)> {
        // Not used — context is prepared by WS handler.
        None
    }

    fn take_stream_context(
        &self,
        session_key: &str,
    ) -> Option<(mpsc::Sender<TurnEvent>, CancellationToken)> {
        let contexts = self.stream_contexts.read();
        contexts.get(session_key).map(|ctx| (ctx.event_tx.clone(), ctx.cancel.clone()))
    }

    fn cancel_turn(&self, session_key: &str) {
        let contexts = self.stream_contexts.read();
        if let Some(ctx) = contexts.get(session_key) {
            ctx.cancel.cancel();
        }
    }
}
