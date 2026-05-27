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
use parking_lot::{Mutex as SyncMutex, RwLock};
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
    #[allow(dead_code)]
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
    /// Pre-bound listener passed from the old process during hot switch.
    /// When set, start() reuses it instead of calling bind().
    pre_bound: SyncMutex<Option<std::net::TcpListener>>,
    /// Per-session streaming context.
    stream_contexts: Arc<RwLock<HashMap<String, StreamContext>>>,
    /// Active connections: connection_id → ClientConnection.
    connections: Arc<RwLock<HashMap<String, ClientConnection>>>,
    /// Reverse map: session_key → connection_id.
    session_owners: Arc<RwLock<HashMap<String, String>>>,
    /// Session manager for management API (set after construction).
    session_manager: Arc<RwLock<Option<Arc<crate::agents::SessionManager>>>>,
    /// Tool specs for management API (set after construction).
    tool_specs: Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
    /// Workspace directory for memory API (set after construction).
    workspace_dir: Arc<RwLock<Option<std::path::PathBuf>>>,
    /// Config file path for config read/write API (set after construction).
    config_path: Arc<RwLock<Option<std::path::PathBuf>>>,
    /// Skill manager for skills API (set after construction).
    skill_manager: Arc<RwLock<Option<Arc<RwLock<crate::agents::SkillManager>>>>>,
    /// Service registry for models API (set after construction).
    provider_registry: Arc<RwLock<Option<Arc<dyn crate::providers::ProviderRegistry>>>>,
}

impl ClientChannel {
    pub fn new(config: ClientConfig) -> Self {
        let (message_tx, message_rx) = mpsc::channel(100);
        Self {
            config,
            message_tx,
            message_rx: Mutex::new(Some(message_rx)),
            pre_bound: SyncMutex::new(None),
            stream_contexts: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(HashMap::new())),
            session_owners: Arc::new(RwLock::new(HashMap::new())),
            session_manager: Arc::new(RwLock::new(None)),
            tool_specs: Arc::new(RwLock::new(Vec::new())),
            workspace_dir: Arc::new(RwLock::new(None)),
            config_path: Arc::new(RwLock::new(None)),
            skill_manager: Arc::new(RwLock::new(None)),
            provider_registry: Arc::new(RwLock::new(None)),
        }
    }

    /// Supply a pre-bound std TcpListener (SO_REUSEPORT, from hot switch or
    /// early daemon startup).  Must be called before listen().
    pub fn set_pre_bound(&self, listener: std::net::TcpListener) {
        *self.pre_bound.lock() = Some(listener);
    }

    /// Set the session manager (called from daemon.rs after construction).
    pub fn set_session_manager(&self, sm: Arc<crate::agents::SessionManager>) {
        *self.session_manager.write() = Some(sm);
    }

    /// Set the tool specs list (called from daemon.rs after construction).
    pub fn set_tool_specs(&self, specs: Vec<crate::providers::capability_tool::ToolSpec>) {
        *self.tool_specs.write() = specs;
    }

    /// Set the workspace directory (called from daemon.rs after construction).
    pub fn set_workspace_dir(&self, dir: std::path::PathBuf) {
        *self.workspace_dir.write() = Some(dir);
    }

    /// Set the config file path (called from daemon.rs after construction).
    pub fn set_config_path(&self, path: std::path::PathBuf) {
        *self.config_path.write() = Some(path);
    }

    /// Set the skill manager (called from daemon.rs after construction).
    pub fn set_skill_manager(&self, sm: Arc<RwLock<crate::agents::SkillManager>>) {
        *self.skill_manager.write() = Some(sm);
    }

    /// Set the service registry (called from daemon.rs after construction).
    pub fn set_provider_registry(&self, sr: Arc<dyn crate::providers::ProviderRegistry>) {
        *self.provider_registry.write() = Some(sr);
    }

    /// Start the WebSocket server (spawns a background task).
    /// Called lazily from listen() — the first time the Orchestrator starts consuming.
    async fn start(&self) -> anyhow::Result<()> {
        // Prefer a pre-bound listener (hot switch / SO_REUSEPORT inheritance).
        // Extract from the sync lock before any await so MutexGuard is not held
        // across an await point (parking_lot::MutexGuard is not Send).
        let pre_bound = self.pre_bound.lock().take();
        let listener = if let Some(std_listener) = pre_bound {
            std_listener.set_nonblocking(true)
                .map_err(|e| anyhow::anyhow!("failed to set nonblocking on inherited client socket: {}", e))?;
            let l = TcpListener::from_std(std_listener)
                .map_err(|e| anyhow::anyhow!("failed to convert inherited client socket: {}", e))?;
            tracing::info!(
                addr = %l.local_addr().unwrap_or_else(|_| self.config.bind.parse().unwrap()),
                "WebSocket server reusing inherited socket (hot switch)"
            );
            l
        } else {
            let addr: SocketAddr = self.config.bind.parse()
                .map_err(|e| anyhow::anyhow!("invalid client bind address '{}': {}", self.config.bind, e))?;
            TcpListener::bind(addr).await
                .map_err(|e| anyhow::anyhow!("failed to bind WebSocket server to {}: {}", addr, e))?
        };

        let max_connections = self.config.max_connections;
        let auth_token = self.config.auth_token.clone();
        let message_tx = self.message_tx.clone();
        let stream_contexts = self.stream_contexts.clone();
        let connections = self.connections.clone();
        let session_owners = self.session_owners.clone();
        let session_manager = self.session_manager.clone();
        let tool_specs = self.tool_specs.clone();
        let workspace_dir = self.workspace_dir.clone();
        let config_path = self.config_path.clone();
        let skill_manager = self.skill_manager.clone();
        let provider_registry = self.provider_registry.clone();

        let local_addr = listener.local_addr()?;
        tracing::info!("WebSocket server listening on ws://{}/myclaw", local_addr);

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
                                tracing::warn!(peer = %peer_addr, err = %e, "WebSocket handshake failed");
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
                        let session_manager_clone = session_manager.clone();
                        let tool_specs_clone = tool_specs.clone();
                        let workspace_dir_clone = workspace_dir.clone();
                        let config_path_clone = config_path.clone();
                        let skill_manager_clone = skill_manager.clone();
                        let provider_registry_clone = provider_registry.clone();
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
                                // If auth_token is configured, connections must authenticate
                                // before sending any other message type.
                                // If auth_token is None, all connections are pre-authenticated.
                                let mut is_authenticated = auth_token_clone.is_none();
                                // Stable identity for session scoping. Defaults to the
                                // ephemeral conn_id (TUI); a WebUI client supplies a
                                // persistent client_id in its auth message so its
                                // sessions survive reconnects.
                                let mut client_id = conn_id_clone.clone();

                                while let Some(msg_result) = futures_util::StreamExt::next(&mut ws_stream).await {
                                    let msg = match msg_result {
                                        Ok(Message::Text(text)) => text.to_string(),
                                        Ok(Message::Close(_)) => break,
                                        Ok(_) => continue, // Ignore binary, ping, pong
                                        Err(e) => {
                                            tracing::warn!(conn_id = %conn_id_clone, err = %e, "WebSocket read error");
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

                                    // ── Auth gate ──────────────────────────────────────
                                    // Handle the "auth" message type first (before the gate).
                                    if msg_type == "auth" {
                                        let provided = parsed["token"].as_str().unwrap_or("");
                                        let ok = match &auth_token_clone {
                                            None => true, // no auth required — accept any token
                                            Some(required) => provided == required.as_str(),
                                        };
                                        if ok {
                                            is_authenticated = true;
                                            if let Some(cid) = parsed["client_id"].as_str() {
                                                let cid = cid.trim();
                                                if !cid.is_empty() {
                                                    client_id = format!("web:{}", cid);
                                                }
                                            }
                                            let _ = ws_sender.send(r#"{"type":"auth_ok"}"#.to_string()).await;
                                            tracing::debug!(conn_id = %conn_id_clone, "WebSocket client authenticated");
                                        } else {
                                            let err = serde_json::json!({
                                                "type": "error",
                                                "message": "Unauthorized: invalid token"
                                            });
                                            let _ = ws_sender.send(err.to_string()).await;
                                            tracing::warn!(conn_id = %conn_id_clone, "WebSocket auth failed, closing connection");
                                            break; // Close connection on invalid token
                                        }
                                        continue;
                                    }

                                    // Reject all other messages until authenticated.
                                    if !is_authenticated {
                                        let err = serde_json::json!({
                                            "type": "error",
                                            "message": "Authentication required: send {\"type\":\"auth\",\"token\":\"...\"} first"
                                        });
                                        let _ = ws_sender.send(err.to_string()).await;
                                        tracing::warn!(conn_id = %conn_id_clone, msg_type, "rejected unauthenticated message");
                                        break;
                                    }

                                    match msg_type {
                                        "message" => {
                                            let mut content = parsed["content"].as_str().unwrap_or("").to_string();

                                            // Inline text-file attachments into the prompt
                                            // ({name, content} pairs sent by the WebUI).
                                            if let Some(arr) = parsed["attachments"].as_array() {
                                                for a in arr {
                                                    let nm = a["name"].as_str().unwrap_or("file");
                                                    let body = a["content"].as_str().unwrap_or("");
                                                    content.push_str(&format!(
                                                        "\n\n--- attached file: {} ---\n```\n{}\n```",
                                                        nm, body
                                                    ));
                                                }
                                            }

                                            // Decode base64 images; strip any data: URL prefix.
                                            let images: Vec<String> = parsed["image_base64"]
                                                .as_array()
                                                .map(|arr| {
                                                    arr.iter()
                                                        .filter_map(|v| v.as_str())
                                                        .map(|s| match s.split_once("base64,") {
                                                            Some((_, b)) => b.to_string(),
                                                            None => s.to_string(),
                                                        })
                                                        .collect()
                                                })
                                                .unwrap_or_default();
                                            let has_images = !images.is_empty();

                                            if content.trim().is_empty() && !has_images {
                                                let err = serde_json::json!({"type":"error","message":"empty content"});
                                                let _ = ws_sender.send(err.to_string()).await;
                                                continue;
                                            }

                                            // E32: stream context indexed by reply_target.
                                            // For ClientChannel today reply_target ==
                                            // session_key, but using reply_target
                                            // semantically lets sub-agents push events
                                            // to the parent's UI when their session
                                            // inherits the parent's reply_target.
                                            let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(64);
                                            let cancel = CancellationToken::new();
                                            let reply_target_key = session_key_clone.clone();
                                            {
                                                let mut contexts = stream_contexts_clone.write();
                                                contexts.insert(reply_target_key.clone(), StreamContext {
                                                    event_tx: event_tx.clone(),
                                                    cancel: cancel.clone(),
                                                });
                                            }

                                            // Spawn event forwarder: event_rx → ws_sender.
                                            // NOTE: do NOT remove the stream_context entry here.
                                            // The orchestrator calls take_stream_context() which
                                            // removes the entry atomically before processing
                                            // begins.  Removing it here would race with the next
                                            // message's context insertion and silently drop it.
                                            let fwd_sender = ws_sender.clone();
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
                                            });

                                            // Create ChannelMessage for Orchestrator.
                                            let channel_msg = ChannelMessage {
                                                id: format!("{}-{}", conn_id_clone, chrono::Utc::now().timestamp_millis()),
                                                sender: client_id.clone(),
                                                reply_target: session_key_clone.clone(),
                                                content,
                                                timestamp: chrono::Utc::now().timestamp() as u64,
                                                thread_ts: None,
                                                interruption_scope_id: None,
                                                attachments: vec![],
                                                image_urls: None,
                                                image_base64: if has_images { Some(images) } else { None },
                                            };

                                            if message_tx_clone.send(channel_msg).await.is_err() {
                                                tracing::warn!("orchestrator message channel closed");
                                                break;
                                            }
                                        }

                                        "cancel" => {
                                            // Cancel current turn.
                                            let contexts = stream_contexts_clone.read();
                                            if let Some(ctx) = contexts.get(&session_key_clone) {
                                                ctx.cancel.cancel();
                                                tracing::debug!(session = %session_key_clone, "turn cancelled by client");
                                            }
                                        }

                                        "api" => {
                                            // Management API.
                                            let id = parsed["id"].as_str().unwrap_or("").to_string();
                                            let method = parsed["method"].as_str().unwrap_or("").to_string();
                                            let params = parsed.get("params").cloned().unwrap_or(serde_json::Value::Null);

                                            // Align the management-API session scope
                                            // with the orchestrator's session key
                                            // (channel:account:sender) so the WebUI
                                            // sees the same sessions chat actually uses.
                                            let api_user_id = format!("client:default:{}", client_id);
                                            let resp = handle_api_request(
                                                &id, &method, &params,
                                                &ApiContext {
                                                    user_id: &api_user_id,
                                                    session_manager: &session_manager_clone,
                                                    tool_specs: &tool_specs_clone,
                                                    workspace_dir: &workspace_dir_clone,
                                                    config_path: &config_path_clone,
                                                    skill_manager: &skill_manager_clone,
                                                    provider_registry: &provider_registry_clone,
                                                },
                                            );
                                            let _ = ws_sender.send(resp).await;
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

                            tracing::debug!(conn_id = %conn_id_clone, "WebSocket client disconnected");
                        });
                    }
                    Err(e) => {
                        tracing::error!(err = %e, "failed to accept WebSocket connection");
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
            conns.get(&conn_id).map(|conn| conn.ws_sender.clone())
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

    fn supports_streaming(&self) -> bool {
        true
    }

    /// E32: push a per-turn event (Chunk, Thinking, ToolCall, …) at
    /// the stream context indexed by `reply_target`. Agent::run calls
    /// this via `session.channel.push_event(reply_target, event)`.
    /// No-op if no stream context is registered for this target —
    /// non-streaming connections drop events silently.
    async fn push_event(&self, reply_target: &str, event: TurnEvent) {
        let sender = {
            let contexts = self.stream_contexts.read();
            contexts.get(reply_target).map(|ctx| ctx.event_tx.clone())
        };
        if let Some(tx) = sender {
            if tx.send(event).await.is_err() {
                tracing::debug!(reply_target = %reply_target, "push_event: client disconnected");
            }
        }
    }

    /// E32: return the cancellation token registered for this
    /// `reply_target` so `Agent::run` can poll for user-initiated cancels.
    fn cancel_signal(&self, reply_target: &str) -> Option<CancellationToken> {
        let contexts = self.stream_contexts.read();
        contexts.get(reply_target).map(|ctx| ctx.cancel.clone())
    }
}

// ── Management API Router ───────────────────────────────────────────────────

/// Shared handles passed to every API request handler.
struct ApiContext<'a> {
    /// Session-manager scope key (channel:account:sender), stable across reconnects.
    user_id: &'a str,
    session_manager: &'a Arc<RwLock<Option<Arc<crate::agents::SessionManager>>>>,
    tool_specs: &'a Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
    workspace_dir: &'a Arc<RwLock<Option<std::path::PathBuf>>>,
    config_path: &'a Arc<RwLock<Option<std::path::PathBuf>>>,
    skill_manager: &'a Arc<RwLock<Option<Arc<RwLock<crate::agents::SkillManager>>>>>,
    provider_registry: &'a Arc<RwLock<Option<Arc<dyn crate::providers::ProviderRegistry>>>>,
}

/// Route a management API request and return a JSON response string.
fn handle_api_request(
    id: &str,
    method: &str,
    params: &serde_json::Value,
    ctx: &ApiContext<'_>,
) -> String {
    let guard = ctx.session_manager.read();
    let sm = match guard.as_ref() {
        Some(sm) => sm,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "session manager not available"
            }).to_string();
        }
    };

    let user_id = ctx.user_id;

    match method {
        "sessions.list" => {
            let sessions = sm.list_sessions(user_id);
            let active = sm.active_session_id(user_id);
            let result: Vec<serde_json::Value> = sessions.iter().map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "name": s.display_name,
                    "created_at": s.created_at.to_rfc3339(),
                    "is_active": active.as_ref() == Some(&s.id),
                })
            }).collect();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": result,
            }).to_string()
        }

        "sessions.create" => {
            let name = params["name"].as_str();
            match sm.new_session(user_id, name) {
                Ok(info) => {
                    // H57: SessionContext eviction handled by slash command paths;
                    // ClientChannel no longer caches AgentLoop instances.
                    serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": {
                            "id": info.id,
                            "name": info.display_name,
                            "created_at": info.created_at.to_rfc3339(),
                        }
                    }).to_string()
                }
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to create session: {}", e)
                }).to_string(),
            }
        }

        "sessions.switch" => {
            let session_id = match params["id"].as_str() {
                Some(s) => s,
                None => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": "missing id parameter"
                    }).to_string();
                }
            };
            match sm.switch_session(user_id, session_id) {
                Ok(info) => {
                    // H57: SessionContext eviction handled by slash command paths;
                    // ClientChannel no longer caches AgentLoop instances.
                    serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": {
                            "id": info.id,
                            "name": info.display_name,
                        }
                    }).to_string()
                }
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to switch session: {}", e)
                }).to_string(),
            }
        }

        "sessions.delete" => {
            let session_id = match params["id"].as_str() {
                Some(s) => s,
                None => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": "missing id parameter"
                    }).to_string();
                }
            };
            match sm.delete_session(user_id, session_id) {
                Ok(()) => {
                    // H57: SessionContext eviction handled by slash command paths;
                    // ClientChannel no longer caches AgentLoop instances.
                    serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": null
                    }).to_string()
                }
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to delete session: {}", e)
                }).to_string(),
            }
        }

        "sessions.rename" => {
            let session_id = match params["id"].as_str() {
                Some(s) => s,
                None => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": "missing id parameter"
                    }).to_string();
                }
            };
            let name = match params["name"].as_str() {
                Some(s) if !s.trim().is_empty() => s.trim(),
                _ => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": "missing or empty name parameter"
                    }).to_string();
                }
            };
            match sm.rename_session(session_id, name) {
                Ok(()) => serde_json::json!({
                    "type": "api_response",
                    "id": id,
                    "result": null
                }).to_string(),
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to rename session: {}", e)
                }).to_string(),
            }
        }

        "tools.list" => {
            let specs = ctx.tool_specs.read();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": &*specs,
            }).to_string()
        }

        "memory.list" => {
            let dir_guard = ctx.workspace_dir.read();
            let result = match dir_guard.as_ref() {
                Some(dir) => {
                    let memory_dir = dir.join("memory");
                    let files = crate::memory::scan_memory_files(&memory_dir);
                    let result: Vec<serde_json::Value> = files.iter().map(|f| {
                        serde_json::json!({
                            "name": f.path.file_name().and_then(|n| n.to_str()).unwrap_or(&f.name).to_string(),
                            "size": std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0),
                            "mem_name": f.name,
                            "summary": f.summary,
                            "tags": f.tags,
                            "mem_type": f.mem_type.as_str(),
                            "created_at": f.created_at,
                        })
                    }).collect();
                    serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": result,
                    }).to_string()
                }
                None => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": "workspace directory not configured"
                }).to_string(),
            };
            drop(dir_guard);
            result
        }

        "memory.write" => {
            let filename = match params["name"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
            };
            if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid filename" }).to_string();
            }
            let content = params["content"].as_str().unwrap_or("");
            let dir_guard = ctx.workspace_dir.read();
            let result = match dir_guard.as_ref() {
                Some(dir) => {
                    let memory_dir = dir.join("memory");
                    let _ = std::fs::create_dir_all(&memory_dir);
                    let path = memory_dir.join(filename);
                    match std::fs::write(&path, content) {
                        Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
                        Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to write file: {}", e) }).to_string(),
                    }
                }
                None => serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string(),
            };
            drop(dir_guard);
            result
        }

        "memory.delete" => {
            let filename = match params["name"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
            };
            if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid filename" }).to_string();
            }
            let dir_guard = ctx.workspace_dir.read();
            let result = match dir_guard.as_ref() {
                Some(dir) => {
                    let path = dir.join("memory").join(filename);
                    match std::fs::remove_file(&path) {
                        Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
                        Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to delete file: {}", e) }).to_string(),
                    }
                }
                None => serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string(),
            };
            drop(dir_guard);
            result
        }

        "memory.read" => {
            let filename = match params["name"].as_str() {
                Some(s) => s,
                None => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": "missing name parameter"
                    }).to_string();
                }
            };
            // Reject path traversal attempts.
            if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
                return serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": "invalid filename"
                }).to_string();
            }
            let dir_guard = ctx.workspace_dir.read();
            let result = match dir_guard.as_ref() {
                Some(dir) => {
                    let path = dir.join("memory").join(filename);
                    match std::fs::read_to_string(&path) {
                        Ok(content) => serde_json::json!({
                            "type": "api_response",
                            "id": id,
                            "result": { "name": filename, "content": content }
                        }).to_string(),
                        Err(e) => serde_json::json!({
                            "type": "api_error",
                            "id": id,
                            "error": format!("failed to read file: {}", e)
                        }).to_string(),
                    }
                }
                None => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": "workspace directory not configured"
                }).to_string(),
            };
            drop(dir_guard);
            result
        }

        "config.get" => {
            let specs = ctx.tool_specs.read();
            let ws_dir = ctx.workspace_dir.read();
            let cfg_path = ctx.config_path.read();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": {
                    "tool_count": specs.len(),
                    "workspace_dir": ws_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                    "config_path": cfg_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                }
            }).to_string()
        }

        "config.get_raw" => {
            let cfg_guard = ctx.config_path.read();
            match cfg_guard.as_ref() {
                Some(path) => match std::fs::read_to_string(path) {
                    Ok(content) => serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": {
                            "content": content,
                            "path": path.to_string_lossy().to_string(),
                        }
                    }).to_string(),
                    Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to read config: {}", e) }).to_string(),
                },
                None => serde_json::json!({ "type": "api_error", "id": id, "error": "config path not set" }).to_string(),
            }
        }

        "config.save" => {
            let content = match params["content"].as_str() {
                Some(s) => s,
                None => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing content parameter" }).to_string(),
            };
            let cfg_guard = ctx.config_path.read();
            match cfg_guard.as_ref() {
                Some(path) => match std::fs::write(path, content) {
                    Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
                    Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to save config: {}", e) }).to_string(),
                },
                None => serde_json::json!({ "type": "api_error", "id": id, "error": "config path not set" }).to_string(),
            }
        }

        "daemon.restart" => {
            // Respond first, then send SIGUSR1 to trigger a hot-switch restart.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(300));
                unsafe { libc::kill(std::process::id() as libc::pid_t, libc::SIGUSR1); }
            });
            serde_json::json!({ "type": "api_response", "id": id, "result": { "message": "Restarting…" } }).to_string()
        }

        "sessions.history" => {
            let session = sm.get_or_create(user_id);
            let msgs = reconstruct_history(&session.history);
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": msgs,
            }).to_string()
        }

        "skills.list" => {
            let guard = ctx.skill_manager.read();
            let result: Vec<serde_json::Value> = match guard.as_ref() {
                Some(mgr_arc) => {
                    let mgr = mgr_arc.read();
                    mgr.skills_iter()
                        .map(|(name, s)| serde_json::json!({
                            "name": name,
                            "description": s.description,
                            "keywords": s.keywords,
                        }))
                        .collect()
                }
                None => Vec::new(),
            };
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": result,
            }).to_string()
        }

        "commands.list" => {
            let result: Vec<serde_json::Value> = crate::agents::commands::command_catalog()
                .into_iter()
                .map(|(name, desc)| serde_json::json!({ "name": name, "description": desc }))
                .collect();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": result,
            }).to_string()
        }

        "models.list" => {
            let guard = ctx.provider_registry.read();
            match guard.as_ref() {
                Some(reg) => {
                    let model_ids = reg.get_chat_routing_models();
                    let active = sm.get_session_override(user_id).model
                        .or_else(|| model_ids.first().cloned());
                    let models: Vec<serde_json::Value> = model_ids.iter().map(|mid| {
                        let supports_image = reg.get_chat_model_config(mid)
                            .map(|c| c.supports_image_input())
                            .unwrap_or(false);
                        serde_json::json!({
                            "id": mid,
                            "active": active.as_deref() == Some(mid.as_str()),
                            "supports_image": supports_image,
                        })
                    }).collect();
                    serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": { "models": models, "active": active },
                    }).to_string()
                }
                None => serde_json::json!({
                    "type": "api_response",
                    "id": id,
                    "result": { "models": [], "active": null },
                }).to_string(),
            }
        }

        "models.set" => {
            let model = params["model"].as_str()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let mut ov = sm.get_session_override(user_id);
            ov.model = model.map(|s| s.to_string());
            sm.save_session_override(user_id, ov);
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": { "model": model },
            }).to_string()
        }

        _ => {
            serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": format!("unknown method: {}", method)
            }).to_string()
        }
    }
}

/// Reconstruct a session's stored history into WebUI chat-message shape.
fn reconstruct_history(
    history: &[crate::providers::capability_chat::ChatMessage],
) -> Vec<serde_json::Value> {
    use crate::providers::capability_chat::ContentPart;
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut counter = 0u64;
    for m in history {
        let mut text = String::new();
        let mut has_image = false;
        for p in &m.parts {
            match p {
                ContentPart::Text { text: t } => text.push_str(t),
                ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. } => has_image = true,
                _ => {}
            }
        }
        match m.role.as_str() {
            "user" => {
                let content = if !text.is_empty() {
                    text
                } else if has_image {
                    "🖼️ (image)".to_string()
                } else {
                    continue;
                };
                counter += 1;
                out.push(serde_json::json!({
                    "role": "user",
                    "content": content,
                    "id": format!("h-{}", counter),
                }));
            }
            "assistant" => {
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                for p in &m.parts {
                    match p {
                        ContentPart::Text { text: t } if !t.is_empty() => {
                            blocks.push(serde_json::json!({ "type": "content", "text": t }));
                        }
                        ContentPart::Thinking { thinking: t, .. } if !t.is_empty() => {
                            blocks.push(serde_json::json!({ "type": "thinking", "text": t }));
                        }
                        _ => {}
                    }
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        let args = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        blocks.push(serde_json::json!({
                            "type": "tool_call",
                            "id": tc.id,
                            "name": tc.name,
                            "args": args,
                        }));
                    }
                }

                // Try to find the closest assistant message in out to merge blocks with,
                // stopping if we encounter a user message. This robustly merges fragmented turns
                // even if intermediate virtual turns are present.
                let mut merged = false;
                for msg in out.iter_mut().rev() {
                    if msg["role"] == "user" {
                        break;
                    }
                    if msg["role"] == "assistant" {
                        if let Some(arr) = msg.get_mut("blocks").and_then(|v| v.as_array_mut()) {
                            arr.extend(blocks.clone());
                            merged = true;
                        }
                        break;
                    }
                }
                if merged {
                    continue;
                }

                if blocks.is_empty() {
                    continue;
                }
                counter += 1;
                out.push(serde_json::json!({
                    "role": "assistant",
                    "blocks": blocks,
                    "id": format!("h-{}", counter),
                    "done": true,
                }));
            }
            "tool" => {
                if let Some(tcid) = &m.tool_call_id {
                    let tcid_val = serde_json::Value::String(tcid.clone());
                    'outer: for msg in out.iter_mut().rev() {
                        if msg["role"] != "assistant" {
                            continue;
                        }
                        if let Some(arr) = msg.get_mut("blocks").and_then(|v| v.as_array_mut()) {
                            for block in arr.iter_mut() {
                                if block["type"] == "tool_call" && block["id"] == tcid_val {
                                    block["output"] = serde_json::json!(text);
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}
