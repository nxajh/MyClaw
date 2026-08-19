//! ClientChannel — WebSocket-based channel for TUI and Web UI clients.
//!
//! Unlike other channels (Telegram, QQBot) where MyClaw is a *client* connecting
//! to an external platform, ClientChannel runs a WebSocket *server* that TUI and
//! Web UI clients connect to.
//!
//! ## Shared state & lock ordering
//!
//! Deferred-init handles (`session_manager`, `workspace_dir`, `config_path`,
//! `skill_manager`, `provider_registry`) are wired by the daemon after
//! construction and never change afterwards, so they use `OnceLock` —
//! lock-free reads, no `RwLock<Option<_>>` nesting. The remaining mutable
//! maps use `parking_lot::RwLock`.
//!
//! Lock-ordering rule (to avoid deadlocks): acquire `connections` before
//! `session_owners`; never hold either across an `.await`. The `skill_manager`
//! inner `RwLock` is a leaf — take it last and release it before touching the
//! connection maps.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::{Mutex as SyncMutex, RwLock};
use std::sync::OnceLock;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::agents::{Skill, TurnEvent};
use crate::agents::workspace::skill_loader;
use crate::channels::message::{
    Channel, ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent,
    ChannelOutboundMessage, LocalFileBody, MessageReceiver, MessageSender, OutboundSendResult,
};
use crate::config::channel::ClientConfig;

// ── Session Output Bus ──────────────────────────────────────────────────────

struct Subscriber {
    conn_id: String,
    sender: mpsc::Sender<String>,
}

/// Per-session output bus. Decouples turn execution from WebSocket connection
/// lifetime. Survives disconnects; buffers events and messages for replay on
/// reconnect. This is the single source of truth for output delivery — the WS
/// connection is just one (replaceable) subscriber.
struct SessionOutputBus {
    /// TurnEvent ring buffer — only accumulates while no subscriber is attached.
    /// Drained on subscribe (replay). Capped at `event_buffer_capacity`.
    event_buffer: std::collections::VecDeque<TurnEvent>,
    event_buffer_capacity: usize,
    /// Non-streaming messages (send_message output) queued while no subscriber.
    message_queue: std::collections::VecDeque<String>,
    /// Active WS subscribers: raw text mpsc → outgoing forwarder → ws_sink.
    /// Multiple connections of the same identity may subscribe simultaneously.
    subscribers: Vec<Subscriber>,
    /// Active session_id for event JSON injection (frontend filtering).
    session_id: String,
    /// Current turn's cancel token. Recreated each turn by create_stream.
    cancel: CancellationToken,
    /// Whether a turn is in progress.
    turn_active: bool,
}

impl SessionOutputBus {
    fn new() -> Self {
        Self {
            event_buffer: std::collections::VecDeque::new(),
            event_buffer_capacity: 256,
            message_queue: std::collections::VecDeque::new(),
            subscribers: Vec::new(),
            session_id: String::new(),
            cancel: CancellationToken::new(),
            turn_active: false,
        }
    }

    /// Install a WS subscriber. Idempotent per conn_id (replaces the sender).
    /// Returns true when the bus transitioned 0 -> 1 subscribers — the caller
    /// should then replay buffered content (drain_messages / drain_events).
    fn subscribe(
        &mut self,
        conn_id: String,
        ws_sender: mpsc::Sender<String>,
        session_id: String,
    ) -> bool {
        let was_empty = self.subscribers.is_empty();
        if let Some(sub) = self.subscribers.iter_mut().find(|s| s.conn_id == conn_id) {
            sub.sender = ws_sender;
        } else {
            self.subscribers.push(Subscriber {
                conn_id,
                sender: ws_sender,
            });
        }
        self.session_id = session_id;
        was_empty
    }

    /// Drain queued non-streaming messages for replay (clears the queue).
    fn drain_messages(&mut self) -> Vec<String> {
        self.message_queue.drain(..).collect()
    }

    /// Drain buffered TurnEvents for replay (clears the buffer).
    fn drain_events(&mut self) -> Vec<TurnEvent> {
        self.event_buffer.drain(..).collect()
    }

    /// Detach one subscriber (on WS disconnect). Bus survives for replay.
    fn detach(&mut self, conn_id: &str) {
        self.subscribers.retain(|s| s.conn_id != conn_id);
    }

    /// Push a TurnEvent. If subscribers are online, forwards directly via
    /// try_send (non-blocking); failed subscribers are dropped. If offline,
    /// buffers for replay. **Never fails** — this is the core decoupling
    /// invariant.
    fn push_event(&mut self, event: TurnEvent) {
        if !self.subscribers.is_empty() {
            let versioned = event.versioned();
            let mut json_val = match serde_json::to_value(&versioned) {
                Ok(v) => v,
                Err(_) => return,
            };
            if let serde_json::Value::Object(ref mut map) = json_val {
                if !self.session_id.is_empty() {
                    map.insert(
                        "session_id".to_string(),
                        serde_json::Value::String(self.session_id.clone()),
                    );
                }
            }
            let json = json_val.to_string();
            self.subscribers
                .retain(|sub| sub.sender.try_send(json.clone()).is_ok());
        } else {
            if self.event_buffer.len() >= self.event_buffer_capacity {
                self.event_buffer.pop_front();
            }
            self.event_buffer.push_back(event);
        }
    }

    /// Push a non-streaming message (send_message output).
    /// If subscribers are online, forwards immediately; else queues for replay.
    fn push_message(&mut self, json: String) {
        if !self.subscribers.is_empty() {
            let mut delivered = false;
            self.subscribers.retain(|sub| match sub.sender.try_send(json.clone()) {
                Ok(()) => {
                    delivered = true;
                    true
                }
                Err(mpsc::error::TrySendError::Full(_)) => true,
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
            if delivered {
                return;
            }
            // All subscribers full or closed — fall through to queue
        }
        self.message_queue.push_back(json);
    }
}

// ── Client Connection ───────────────────────────────────────────────────────

/// A single connected client.
struct ClientConnection {
    /// WebSocket sender — kept for the outgoing forwarder task but no longer
    /// read by send_message (which routes through session_buses instead).
    #[allow(dead_code)]
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
    message_tx: mpsc::Sender<ChannelInboundMessage>,
    /// One-time take for listen().
    message_rx: Mutex<Option<mpsc::Receiver<ChannelInboundMessage>>>,
    /// Pre-bound listener passed from the old process during hot switch.
    /// When set, start() reuses it instead of calling bind().
    pre_bound: SyncMutex<Option<std::net::TcpListener>>,
    /// Per-session output buses (survive WS disconnects).
    session_buses: Arc<RwLock<HashMap<String, Arc<SyncMutex<SessionOutputBus>>>>>,
    /// Active connections: connection_id → ClientConnection.
    connections: Arc<RwLock<HashMap<String, ClientConnection>>>,
    /// Reverse map: session_key → connection_id.
    session_owners: Arc<RwLock<HashMap<String, String>>>,
    /// Session manager for management API (set once after construction).
    session_manager: Arc<OnceLock<Arc<crate::agents::SessionManager>>>,
    /// Tool specs for management API (set after construction).
    tool_specs: Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
    /// Workspace directory for memory API (set once after construction).
    workspace_dir: Arc<OnceLock<std::path::PathBuf>>,
    /// Knowledge dir ({data_dir}/memory) — single flat memory pool where
    /// ownership is a frontmatter attribute (set once after construction).
    knowledge_dir: Arc<OnceLock<std::path::PathBuf>>,
    /// Config file path for config read/write API (set once after construction).
    config_path: Arc<OnceLock<std::path::PathBuf>>,
    /// Skill manager for skills API (set once after construction). The
    /// inner `RwLock` stays — skills reload at runtime; only the outer
    /// deferred-init wrapper is flattened from `RwLock<Option<_>>`.
    skill_manager: Arc<OnceLock<Arc<RwLock<crate::agents::SkillManager>>>>,
    /// Service registry for models API (set once after construction).
    provider_registry: Arc<OnceLock<Arc<dyn crate::providers::ProviderRegistry>>>,
    /// Shared user resolver (routing_key → user_id) for per-user memory
    /// paths in the management API (set once after construction).
    user_resolver: Arc<OnceLock<Arc<crate::agents::UserResolver>>>,
}

impl ClientChannel {
    pub fn new(config: ClientConfig) -> Self {
        let (message_tx, message_rx) = mpsc::channel(100);
        Self {
            config,
            message_tx,
            message_rx: Mutex::new(Some(message_rx)),
            pre_bound: SyncMutex::new(None),
            session_buses: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(HashMap::new())),
            session_owners: Arc::new(RwLock::new(HashMap::new())),
            session_manager: Arc::new(OnceLock::new()),
            tool_specs: Arc::new(RwLock::new(Vec::new())),
            workspace_dir: Arc::new(OnceLock::new()),
            knowledge_dir: Arc::new(OnceLock::new()),
            config_path: Arc::new(OnceLock::new()),
            skill_manager: Arc::new(OnceLock::new()),
            provider_registry: Arc::new(OnceLock::new()),
            user_resolver: Arc::new(OnceLock::new()),
        }
    }

    /// Supply a pre-bound std TcpListener (SO_REUSEPORT, from hot switch or
    /// early daemon startup).  Must be called before listen().
    pub fn set_pre_bound(&self, listener: std::net::TcpListener) {
        *self.pre_bound.lock() = Some(listener);
    }

    /// Set the session manager (called from daemon.rs after construction).
    pub fn set_session_manager(&self, sm: Arc<crate::agents::SessionManager>) {
        let _ = self.session_manager.set(sm);
    }

    /// Set the tool specs list (called from daemon.rs after construction).
    pub fn set_tool_specs(&self, specs: Vec<crate::providers::capability_tool::ToolSpec>) {
        *self.tool_specs.write() = specs;
    }

    /// Set the workspace directory (called from daemon.rs after construction).
    pub fn set_workspace_dir(&self, dir: std::path::PathBuf) {
        let _ = self.workspace_dir.set(dir);
    }

    /// Set the knowledge dir (called from daemon.rs after construction).
    pub fn set_knowledge_dir(&self, dir: std::path::PathBuf) {
        let _ = self.knowledge_dir.set(dir);
    }

    /// Set the config file path (called from daemon.rs after construction).
    pub fn set_config_path(&self, path: std::path::PathBuf) {
        let _ = self.config_path.set(path);
    }

    /// Set the skill manager (called from daemon.rs after construction).
    pub fn set_skill_manager(&self, sm: Arc<RwLock<crate::agents::SkillManager>>) {
        let _ = self.skill_manager.set(sm);
    }

    /// Set the service registry (called from daemon.rs after construction).
    pub fn set_provider_registry(&self, sr: Arc<dyn crate::providers::ProviderRegistry>) {
        let _ = self.provider_registry.set(sr);
    }

    /// Set the shared user resolver (routing_key → user_id), called from
    /// daemon.rs after construction. Enables per-user memory paths.
    pub fn set_user_resolver(&self, resolver: Arc<crate::agents::UserResolver>) {
        let _ = self.user_resolver.set(resolver);
    }

    /// Start the WebSocket server (spawns a background task).
    /// Called lazily from listen() — the first time the Orchestrator starts consuming.
    async fn start(&self) -> anyhow::Result<()> {
        // Prefer a pre-bound listener (hot switch / SO_REUSEPORT inheritance).
        // Extract from the sync lock before any await so MutexGuard is not held
        // across an await point (parking_lot::MutexGuard is not Send).
        let pre_bound = self.pre_bound.lock().take();
        let listener = if let Some(std_listener) = pre_bound {
            std_listener.set_nonblocking(true).map_err(|e| {
                anyhow::anyhow!(
                    "failed to set nonblocking on inherited client socket: {}",
                    e
                )
            })?;
            let l = TcpListener::from_std(std_listener)
                .map_err(|e| anyhow::anyhow!("failed to convert inherited client socket: {}", e))?;
            tracing::info!(
                addr = %l.local_addr().unwrap_or_else(|_| self.config.bind.parse().unwrap()),
                "WebSocket server reusing inherited socket (hot switch)"
            );
            l
        } else {
            let addr: SocketAddr = self.config.bind.parse().map_err(|e| {
                anyhow::anyhow!("invalid client bind address '{}': {}", self.config.bind, e)
            })?;
            TcpListener::bind(addr).await.map_err(|e| {
                anyhow::anyhow!("failed to bind WebSocket server to {}: {}", addr, e)
            })?
        };

        let max_connections = self.config.max_connections;
        let auth_token = self.config.auth_token.clone();
        let message_tx = self.message_tx.clone();
        let session_buses = self.session_buses.clone();
        let connections = self.connections.clone();
        let session_owners = self.session_owners.clone();
        let session_manager = self.session_manager.clone();
        let tool_specs = self.tool_specs.clone();
        let workspace_dir = self.workspace_dir.clone();
        let knowledge_dir = self.knowledge_dir.clone();
        let config_path = self.config_path.clone();
        let skill_manager = self.skill_manager.clone();
        let provider_registry = self.provider_registry.clone();
        let user_resolver = self.user_resolver.clone();

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
                        let ws_config = {
                            let mut cfg =
                                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default(
                                );
                            cfg.max_message_size = Some(200 << 20); // 200 MiB
                            cfg.max_frame_size = Some(64 << 20); // 64 MiB
                            cfg
                        };
                        let ws_result =
                            tokio_tungstenite::accept_async_with_config(stream, Some(ws_config))
                                .await;
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
                            conns.insert(
                                conn_id.clone(),
                                ClientConnection {
                                    ws_sender: ws_sender.clone(),
                                    active_session: session_key.clone(),
                                    sessions: {
                                        let mut set = std::collections::HashSet::new();
                                        set.insert(session_key.clone());
                                        set
                                    },
                                },
                            );
                            let mut owners = session_owners.write();
                            owners.insert(session_key.clone(), conn_id.clone());
                        }

                        let conn_id_clone = conn_id.clone();
                        let mut session_key_clone = session_key.clone();
                        let message_tx_clone = message_tx.clone();
                        let session_buses_clone = session_buses.clone();
                        let connections_clone = connections.clone();
                        let session_owners_clone = session_owners.clone();
                        let session_manager_clone = session_manager.clone();
                        let tool_specs_clone = tool_specs.clone();
                        let workspace_dir_clone = workspace_dir.clone();
                        let knowledge_dir_clone = knowledge_dir.clone();
                        let config_path_clone = config_path.clone();
                        let skill_manager_clone = skill_manager.clone();
                        let provider_registry_clone = provider_registry.clone();
                        let user_resolver_clone = user_resolver.clone();
                        let auth_token_clone = auth_token.clone();

                        tracing::info!(
                            conn_id = %conn_id,
                            peer = %peer_addr,
                            session = %session_key,
                            "WebSocket client connected"
                        );

                        let connected_at = Instant::now();

                        // Spawn per-connection handler.
                        tokio::spawn(async move {
                            let close_reason = Arc::new(SyncMutex::new("task ended".to_string()));
                            let incoming_close_reason = close_reason.clone();

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
                                // Permission/session owner identity. For WebUI this is a stable
                                // logical user; client_id remains only a per-browser device id.
                                let mut client_user_id = conn_id_clone.clone();

                                while let Some(msg_result) =
                                    futures_util::StreamExt::next(&mut ws_stream).await
                                {
                                    let msg = match msg_result {
                                        Ok(Message::Text(text)) => text.to_string(),
                                        Ok(Message::Close(frame)) => {
                                            let reason = match frame {
                                                Some(frame) => format!(
                                                    "close frame: code={} reason={}",
                                                    frame.code, frame.reason
                                                ),
                                                None => "close frame without details".to_string(),
                                            };
                                            *incoming_close_reason.lock() = reason;
                                            break;
                                        }
                                        Ok(Message::Ping(payload)) => {
                                            tracing::debug!(
                                                conn_id = %conn_id_clone,
                                                payload_len = payload.len(),
                                                "WebSocket protocol ping received"
                                            );
                                            continue;
                                        }
                                        Ok(Message::Pong(payload)) => {
                                            tracing::debug!(
                                                conn_id = %conn_id_clone,
                                                payload_len = payload.len(),
                                                "WebSocket protocol pong received"
                                            );
                                            continue;
                                        }
                                        Ok(Message::Binary(payload)) => {
                                            tracing::debug!(
                                                conn_id = %conn_id_clone,
                                                bytes = payload.len(),
                                                "WebSocket binary message ignored"
                                            );
                                            continue;
                                        }
                                        Ok(Message::Frame(_)) => continue,
                                        Err(e) => {
                                            *incoming_close_reason.lock() =
                                                format!("read error: {}", e);
                                            tracing::warn!(conn_id = %conn_id_clone, err = %e, "WebSocket read error");
                                            break;
                                        }
                                    };

                                    // Parse the incoming JSON message.
                                    let parsed: serde_json::Value = match serde_json::from_str(&msg)
                                    {
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
                                            let _ = ws_sender
                                                .send(r#"{"type":"auth_ok"}"#.to_string())
                                                .await;
                                            tracing::debug!(conn_id = %conn_id_clone, "WebSocket client authenticated");
                                            let client_user = parsed["user_id"]
                                                .as_str()
                                                .or_else(|| parsed["user"].as_str())
                                                .map(str::trim)
                                                .filter(|id| !id.is_empty())
                                                .unwrap_or("default");
                                            client_user_id = format!("web-user:{}", client_user);
                                            if let Some(cid) = parsed["client_id"].as_str() {
                                                let cid = cid.trim();
                                                if !cid.is_empty() {
                                                    let new_client_id = format!("web:{}", cid);

                                                    // Phase 6: migrate bus to stable session_key
                                                    // so reconnects route to the same bus.
                                                    let old_sk = session_key_clone.clone();
                                                    let new_sk =
                                                        format!("client:default:{}", new_client_id);
                                                    if new_sk != old_sk {
                                                        // Migrate bus
                                                        {
                                                            let mut buses =
                                                                session_buses_clone.write();
                                                            if let Some(bus) = buses.remove(&old_sk)
                                                            {
                                                                buses.insert(new_sk.clone(), bus);
                                                            } else {
                                                                buses
                                                                    .entry(new_sk.clone())
                                                                    .or_insert_with(|| {
                                                                        Arc::new(SyncMutex::new(
                                                                            SessionOutputBus::new(),
                                                                        ))
                                                                    });
                                                            }
                                                        }
                                                        // Migrate session_owners
                                                        {
                                                            let mut owners =
                                                                session_owners_clone.write();
                                                            owners.remove(&old_sk);
                                                            owners.insert(
                                                                new_sk.clone(),
                                                                conn_id_clone.clone(),
                                                            );
                                                        }
                                                        // Update connection sessions set
                                                        {
                                                            let mut conns =
                                                                connections_clone.write();
                                                            if let Some(conn) =
                                                                conns.get_mut(&conn_id_clone)
                                                            {
                                                                conn.sessions.remove(&old_sk);
                                                                conn.sessions
                                                                    .insert(new_sk.clone());
                                                                conn.active_session =
                                                                    new_sk.clone();
                                                            }
                                                        }
                                                        session_key_clone = new_sk;
                                                        // Subscribe + replay buffered content
                                                        let api_user = format!(
                                                            "client:default:{}",
                                                            client_user_id
                                                        );
                                                        let session_id = session_manager_clone
                                                            .get()
                                                            .and_then(|sm| {
                                                                sm.active_session_id(&api_user)
                                                            })
                                                            .unwrap_or_default();
                                                        let bus = {
                                                            let mut buses =
                                                                session_buses_clone.write();
                                                            buses
                                                                .entry(session_key_clone.clone())
                                                                .or_insert_with(|| {
                                                                    Arc::new(SyncMutex::new(
                                                                        SessionOutputBus::new(),
                                                                    ))
                                                                })
                                                                .clone()
                                                        };
                                                        let (queued_msgs, replay_events) = {
                                                            let mut bg = bus.lock();
                                                            let first = bg.subscribe(
                                                                conn_id_clone.clone(),
                                                                ws_sender.clone(),
                                                                session_id.clone(),
                                                            );
                                                            if first {
                                                                (
                                                                    bg.drain_messages(),
                                                                    bg.drain_events(),
                                                                )
                                                            } else {
                                                                (Vec::new(), Vec::new())
                                                            }
                                                        };
                                                        for msg_json in queued_msgs {
                                                            let _ = ws_sender.send(msg_json).await;
                                                        }
                                                        for event in replay_events {
                                                            let versioned = event.versioned();
                                                            let mut jv =
                                                                serde_json::to_value(&versioned)
                                                                    .unwrap_or_default();
                                                            if let serde_json::Value::Object(
                                                                ref mut map,
                                                            ) = jv
                                                            {
                                                                map.insert(
                                                                    "session_id".to_string(),
                                                                    serde_json::Value::String(
                                                                        session_id.clone(),
                                                                    ),
                                                                );
                                                            }
                                                            let _ = ws_sender
                                                                .send(jv.to_string())
                                                                .await;
                                                        }
                                                        tracing::info!(
                                                            conn_id = %conn_id_clone,
                                                            old_session = %old_sk,
                                                            new_session = %session_key_clone,
                                                            "session migrated to stable key on auth"
                                                        );
                                                    }
                                                }
                                            }
                                        } else {
                                            let err = serde_json::json!({
                                                "type": "error",
                                                "message": "Unauthorized: invalid token"
                                            });
                                            let _ = ws_sender.send(err.to_string()).await;
                                            tracing::warn!(conn_id = %conn_id_clone, "WebSocket auth failed, closing connection");
                                            *incoming_close_reason.lock() =
                                                "auth failed".to_string();
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
                                        *incoming_close_reason.lock() =
                                            format!("unauthenticated message: {}", msg_type);
                                        break;
                                    }

                                    match msg_type {
                                        "message" => {
                                            let mut content = parsed["content"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string();

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

                                            // Decode base64 files (images, audio, video, docs)
                                            // and save to temp files. Accept both the legacy
                                            // `image_base64` array (bare strings) and a richer
                                            // `files_base64` array of {data, mime_type, file_name}.
                                            let mut image_files: Vec<ChannelFile> = Vec::new();
                                            if let Some(arr) = parsed["files_base64"].as_array() {
                                                use base64::Engine;
                                                for (idx, entry) in arr.iter().enumerate() {
                                                    let raw = match entry
                                                        .get("data")
                                                        .and_then(|v| v.as_str())
                                                    {
                                                        Some(s) => s,
                                                        None => continue,
                                                    };
                                                    let b64 = raw
                                                        .split_once("base64,")
                                                        .map(|(_, b)| b)
                                                        .unwrap_or(raw);
                                                    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                                                        Ok(b) => b,
                                                        Err(_) => continue,
                                                    };
                                                    let mime = entry
                                                        .get("mime_type")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("application/octet-stream")
                                                        .to_string();
                                                    let file_name = entry
                                                        .get("file_name")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or(&format!("file-{}", idx + 1))
                                                        .to_string();
                                                    let ext =
                                                        crate::providers::media::modality_from_mime(
                                                            Some(&mime),
                                                            &file_name,
                                                        );
                                                    let suffix = match ext {
                                                        crate::providers::media::FileModality::Image => "img",
                                                        crate::providers::media::FileModality::Audio => "audio",
                                                        crate::providers::media::FileModality::Video => "video",
                                                        crate::providers::media::FileModality::Other => "file",
                                                    };
                                                    let temp_path =
                                                        std::env::temp_dir().join(format!(
                                                            "myclaw-client-{suffix}-{}",
                                                            uuid::Uuid::new_v4()
                                                        ));
                                                    if tokio::fs::write(&temp_path, &bytes)
                                                        .await
                                                        .is_ok()
                                                    {
                                                        image_files.push(ChannelFile {
                                                            meta: ChannelFileMeta {
                                                                file_name,
                                                                mime_type: Some(mime),
                                                                size_bytes: Some(bytes.len() as u64),
                                                                source_url: None,
                                                            },
                                                            body: std::sync::Arc::new(LocalFileBody::new(temp_path)),
                                                        });
                                                    }
                                                }
                                            }
                                            // Legacy: bare image_base64 array (backwards compat).
                                            if let Some(arr) = parsed["image_base64"].as_array() {
                                                use base64::Engine;
                                                for (idx, v) in arr.iter().enumerate() {
                                                    if let Some(raw) = v.as_str() {
                                                        let b64 = raw
                                                            .split_once("base64,")
                                                            .map(|(_, b)| b)
                                                            .unwrap_or(raw);
                                                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                                                            let fname = format!("image-{}.png", idx + 1);
                                                            let temp_path = std::env::temp_dir().join(format!(
                                                                "myclaw-client-img-{}.png",
                                                                uuid::Uuid::new_v4()
                                                            ));
                                                            if tokio::fs::write(&temp_path, &bytes).await.is_ok() {
                                                                image_files.push(ChannelFile {
                                                                    meta: ChannelFileMeta {
                                                                        file_name: fname,
                                                                        mime_type: Some("image/png".to_string()),
                                                                        size_bytes: Some(bytes.len() as u64),
                                                                        source_url: None,
                                                                    },
                                                                    body: std::sync::Arc::new(LocalFileBody::new(temp_path)),
                                                                });
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            let has_images = !image_files.is_empty();

                                            if content.trim().is_empty() && !has_images {
                                                let err = serde_json::json!({"type":"error","message":"empty content"});
                                                let _ = ws_sender.send(err.to_string()).await;
                                                continue;
                                            }

                                            // Ensure session bus exists + subscribe (in case
                                            // auth wasn't called, e.g. TUI). For WebUI the
                                            // bus was already subscribed during auth migration.
                                            let fwd_api_user =
                                                format!("client:default:{}", client_user_id);
                                            let fwd_session_id = session_manager_clone
                                                .get()
                                                .and_then(|sm| sm.active_session_id(&fwd_api_user))
                                                .unwrap_or_default();
                                            let replay_data = {
                                                let bus = {
                                                    let mut buses = session_buses_clone.write();
                                                    buses
                                                        .entry(session_key_clone.clone())
                                                        .or_insert_with(|| {
                                                            Arc::new(SyncMutex::new(
                                                                SessionOutputBus::new(),
                                                            ))
                                                        })
                                                        .clone()
                                                };
                                                let mut bg = bus.lock();
                                                if bg.subscribe(
                                                    conn_id_clone.clone(),
                                                    ws_sender.clone(),
                                                    fwd_session_id.clone(),
                                                ) {
                                                    Some((bg.drain_messages(), bg.drain_events()))
                                                } else {
                                                    None
                                                }
                                                // bg dropped here, before any .await
                                            };
                                            if let Some((queued, events)) = replay_data {
                                                for msg_json in queued {
                                                    let _ = ws_sender.send(msg_json).await;
                                                }
                                                for event in events {
                                                    let versioned = event.versioned();
                                                    let mut jv = serde_json::to_value(&versioned)
                                                        .unwrap_or_default();
                                                    if let serde_json::Value::Object(ref mut map) =
                                                        jv
                                                    {
                                                        map.insert(
                                                            "session_id".to_string(),
                                                            serde_json::Value::String(
                                                                fwd_session_id.clone(),
                                                            ),
                                                        );
                                                    }
                                                    let _ = ws_sender.send(jv.to_string()).await;
                                                }
                                            }

                                            // Create ChannelInboundMessage for Orchestrator.
                                            let channel_msg = ChannelInboundMessage {
                                                id: format!(
                                                    "{}-{}",
                                                    conn_id_clone,
                                                    chrono::Utc::now().timestamp_millis()
                                                ),
                                                sender: MessageSender::new(client_user_id.clone()),
                                                receiver: MessageReceiver::new(
                                                    session_key_clone.clone(),
                                                ),
                                                content: ChannelMessageContent {
                                                    text: content,
                                                    files: image_files,
                                                    buttons: vec![],
                                                },
                                                timestamp: chrono::Utc::now().timestamp() as u64,
                                                interruption_scope_id: None,
                                                silenced_override: None,
                                                run_mode: Default::default(),
                                            };

                                            if message_tx_clone.send(channel_msg).await.is_err() {
                                                *incoming_close_reason.lock() =
                                                    "orchestrator message channel closed"
                                                        .to_string();
                                                tracing::warn!(
                                                    "orchestrator message channel closed"
                                                );
                                                break;
                                            }
                                        }

                                        "cancel" => {
                                            // Cancel current turn via the bus's cancel token.
                                            let buses = session_buses_clone.read();
                                            if let Some(bus) = buses.get(&session_key_clone) {
                                                bus.lock().cancel.cancel();
                                                tracing::debug!(session = %session_key_clone, "turn cancelled by client");
                                            }
                                        }

                                        "api" => {
                                            // Management API.
                                            let id =
                                                parsed["id"].as_str().unwrap_or("").to_string();
                                            let method =
                                                parsed["method"].as_str().unwrap_or("").to_string();
                                            let params = parsed
                                                .get("params")
                                                .cloned()
                                                .unwrap_or(serde_json::Value::Null);

                                            // Align the management-API session scope
                                            // with the orchestrator's session key
                                            // (channel:account:sender) so the WebUI
                                            // sees the same sessions chat actually uses.
                                            let api_user_id =
                                                format!("client:default:{}", client_user_id);
                                            let resp = handle_api_request(
                                                &id,
                                                &method,
                                                &params,
                                                &ApiContext {
                                                    user_id: &api_user_id,
                                                    session_manager: &session_manager_clone,
                                                    tool_specs: &tool_specs_clone,
                                                    workspace_dir: &workspace_dir_clone,
                                                    knowledge_dir: &knowledge_dir_clone,
                                                    config_path: &config_path_clone,
                                                    skill_manager: &skill_manager_clone,
                                                    provider_registry: &provider_registry_clone,
                                                    user_resolver: &user_resolver_clone,
                                                },
                                            );
                                            let _ = ws_sender.send(resp).await;
                                        }

                                        "ping" => {
                                            let _ = ws_sender
                                                .send(r#"{"type":"pong"}"#.to_string())
                                                .await;
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
                                _ = outgoing => {
                                    *close_reason.lock() = "outgoing task ended".to_string();
                                },
                                _ = incoming => {},
                            }

                            // Clean up on disconnect. Collect the
                            // connection's owned session_keys first, then
                            // drop the connections read-lock before taking
                            // writes on session_owners + session_buses.
                            //
                            // KEY CHANGE: detach subscribers but do NOT remove
                            // buses or trigger cancel. The bus survives so
                            // buffered events + queued messages can be replayed
                            // on reconnect. Turns continue to completion.
                            let owned_keys: Vec<String> = {
                                let conns = connections_clone.read();
                                conns
                                    .get(&conn_id_clone)
                                    .map(|conn| conn.sessions.iter().cloned().collect())
                                    .unwrap_or_default()
                            };
                            // Detach subscriber on each bus (bus survives)
                            {
                                let buses = session_buses_clone.read();
                                for sk in &owned_keys {
                                    if let Some(bus) = buses.get(sk) {
                                        bus.lock().detach(&conn_id_clone);
                                    }
                                }
                            }
                            {
                                let mut owners = session_owners_clone.write();
                                for sk in &owned_keys {
                                    owners.remove(sk);
                                }
                            }
                            {
                                let mut conns = connections_clone.write();
                                conns.remove(&conn_id_clone);
                            }

                            let close_reason = close_reason.lock().clone();
                            tracing::debug!(
                                conn_id = %conn_id_clone,
                                uptime_ms = connected_at.elapsed().as_millis(),
                                reason = %close_reason,
                                "WebSocket client disconnected"
                            );
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

static CLIENT_CAPS: crate::channels::message::ChannelCapabilities =
    crate::channels::message::ChannelCapabilities::client();

#[async_trait]
impl Channel for ClientChannel {
    fn name(&self) -> &str {
        "client"
    }

    fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities {
        &CLIENT_CAPS
    }

    fn tts_enabled(&self) -> bool {
        self.config.tts
    }

    async fn send_message(
        &self,
        msg: &ChannelOutboundMessage,
    ) -> anyhow::Result<OutboundSendResult> {
        // Route through the session bus. If subscriber is online, forwards
        // immediately. If offline (WS disconnected), queues for replay on
        // reconnect. This replaces the old session_owners → connections →
        // ws_sender chain which silently dropped messages on disconnect.
        //
        // session_buses is keyed by the full session key (e.g.
        // "client:default:web-user:default" — see ChannelInboundMessage's
        // receiver field, set to session_key_clone, and session_buses.entry()
        // insertion). But the generic cross-channel convention — used by
        // every other Channel impl, and by friends.rs::notify_peer (the
        // /link verification-code push) — is that receiver.id is a bare,
        // channel-local address with no channel:account prefix. Without this
        // normalization, notify_peer's receiver.id ("web-user:default")
        // never matches any bus key, send_message silently no-ops (still
        // returns Ok(..), so /link reports "code sent" while nothing is
        // ever delivered), and the mismatch is invisible unless you go
        // looking for the "no session bus found" warning.
        let recipient = msg.receiver.id.clone();
        let recipient = if recipient.starts_with("client:") {
            recipient
        } else {
            format!("client:default:{recipient}")
        };

        // Clone the Arc to the bus before any .await — parking_lot guards
        // are not Send and must not cross await points.
        let bus = {
            let buses = self.session_buses.read();
            match buses.get(&recipient) {
                Some(b) => Arc::clone(b),
                None => {
                    tracing::warn!(recipient = %recipient, "no session bus found for send_message");
                    return Ok(OutboundSendResult::empty());
                }
            }
        };

        if msg.content.files.is_empty() {
            let json = serde_json::json!({
                "type": "message",
                "session": recipient,
                "content": msg.content.text,
            })
            .to_string();
            bus.lock().push_message(json);
        } else {
            // File messages: encode as base64 and push each as a separate
            // message JSON. File messages are NOT queued on disconnect
            // (base64 data may be large / stale) — only text is queued.
            use base64::Engine;
            // Check if subscriber is online before encoding files
            let subscriber_online = {
                let bg = bus.lock();
                !bg.subscribers.is_empty()
            };
            if !subscriber_online {
                tracing::debug!(recipient = %recipient, "subscriber offline, skipping file messages");
                return Ok(OutboundSendResult::empty());
            }
            for (idx, file) in msg.content.files.iter().enumerate() {
                let mut reader = file.body.open().await?;
                let mut data = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut data).await?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                let caption = if idx == 0 && !msg.content.text.trim().is_empty() {
                    Some(msg.content.text.as_str())
                } else {
                    None
                };
                let json = serde_json::json!({
                    "type": "file",
                    "session": recipient,
                    "file_name": file.meta.file_name,
                    "mime_type": file.meta.mime_type,
                    "size": file.meta.size_bytes,
                    "data": b64,
                    "caption": caption,
                })
                .to_string();
                bus.lock().push_message(json);
            }
        }
        Ok(OutboundSendResult::empty())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelInboundMessage>> {
        // Lazily start the WebSocket server on first listen() call.
        self.start().await?;
        let rx =
            self.message_rx.lock().await.take().ok_or_else(|| {
                anyhow::anyhow!("listen() called more than once on ClientChannel")
            })?;
        Ok(rx)
    }

    async fn health_check(&self) -> bool {
        true // Local WebSocket server is always healthy.
    }

    /// RFC §7.6: build a per-turn TurnStream over the WebSocket stream
    /// already registered for `reply_target`. Returns None if no client
    /// is currently subscribed for this target — caller falls through to
    /// the non-streaming `send` path.
    fn create_stream(&self, reply_target: &str) -> Option<Box<dyn crate::channels::TurnStream>> {
        let buses = self.session_buses.read();
        let bus = buses.get(reply_target)?;
        let mut bus_guard = bus.lock();
        // Fresh cancel token per turn
        bus_guard.cancel = CancellationToken::new();
        bus_guard.turn_active = true;
        Some(Box::new(ClientTurnStream {
            bus: Arc::clone(bus),
            status: crate::channels::StreamDelivery::Pending,
            finished: false,
        }))
    }
}

/// Per-turn streaming handle for ClientChannel (RFC §7.6).
///
/// Holds a shared reference to the session's `SessionOutputBus`. `push`
/// writes through the bus — it **never fails** because the bus buffers
/// when no subscriber is attached. This is the core decoupling: Agent
/// runs to completion regardless of WS connection state.
pub(crate) struct ClientTurnStream {
    bus: Arc<SyncMutex<SessionOutputBus>>,
    status: crate::channels::StreamDelivery,
    finished: bool,
}

#[async_trait]
impl crate::channels::TurnStream for ClientTurnStream {
    async fn push(&mut self, event: TurnEvent) -> anyhow::Result<crate::channels::StreamDelivery> {
        self.bus.lock().push_event(event);
        self.status = crate::channels::StreamDelivery::Visible;
        Ok(self.status)
    }

    fn status(&self) -> crate::channels::StreamDelivery {
        self.status
    }

    async fn finish(mut self: Box<Self>) -> crate::channels::StreamDelivery {
        self.finished = true;
        self.status = crate::channels::StreamDelivery::FinalDelivered;
        let mut bus = self.bus.lock();
        bus.turn_active = false;
        self.status
    }

    async fn abort(mut self: Box<Self>) {
        self.finished = true;
        self.bus.lock().cancel.cancel();
    }

    fn cancel_token(&self) -> Option<CancellationToken> {
        Some(self.bus.lock().cancel.clone())
    }
}

// Drop-based safety net: if a TurnStream is dropped without finish/abort
// (panic, accidental field overwrite), cancel the turn so Agent::run stops.
// The bus itself survives — only the cancel token fires.
impl Drop for ClientTurnStream {
    fn drop(&mut self) {
        if !self.finished {
            self.bus.lock().cancel.cancel();
        }
    }
}

// ── Management API Router ───────────────────────────────────────────────────

fn is_safe_skill_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.starts_with('.')
}

/// Resolve the on-disk skill directory for a skill name.
///
/// Prefer the path recorded on `Skill.skill_dir` (from SKILL.md source_path),
/// so frontmatter `name` may differ from the directory name. Fall back to
/// `workspace/skills/{name}` only when the manager has no entry.
fn resolve_skill_dir(ctx: &ApiContext<'_>, name: &str) -> Option<std::path::PathBuf> {
    if let Some(mgr_arc) = ctx.skill_manager.get() {
        if let Some(dir) = mgr_arc.read().skill_dir(name) {
            return Some(dir.to_path_buf());
        }
    }
    ctx.workspace_dir
        .get()
        .map(|ws| ws.join("skills").join(name))
        .filter(|p| p.exists())
}

fn reload_skills_from_workspace(ctx: &ApiContext<'_>, workspace: &std::path::Path) {
    if let Some(mgr_arc) = ctx.skill_manager.get() {
        let defs = skill_loader::load_skills_from_dir(&workspace.join("skills"));
        let new_skills: Vec<Skill> = defs.iter().map(Skill::from_definition).collect();
        mgr_arc.write().reload(new_skills);
    }
}

/// Shared handles passed to every API request handler.
struct ApiContext<'a> {
    /// Session-manager scope key (channel:account:sender), stable across reconnects.
    user_id: &'a str,
    session_manager: &'a Arc<OnceLock<Arc<crate::agents::SessionManager>>>,
    tool_specs: &'a Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
    workspace_dir: &'a Arc<OnceLock<std::path::PathBuf>>,
    knowledge_dir: &'a Arc<OnceLock<std::path::PathBuf>>,
    config_path: &'a Arc<OnceLock<std::path::PathBuf>>,
    skill_manager: &'a Arc<OnceLock<Arc<RwLock<crate::agents::SkillManager>>>>,
    provider_registry: &'a Arc<OnceLock<Arc<dyn crate::providers::ProviderRegistry>>>,
    user_resolver: &'a Arc<OnceLock<Arc<crate::agents::UserResolver>>>,
}

/// Resolve the memory directory for a scope.
/// P1-B2: single flat knowledge dir for both scopes — ownership is a
/// frontmatter attribute (`scope` + `user_id`), not a path segment.
/// Falls back to the legacy `{workspace}/memory` when the knowledge dir
/// handle was not installed (older embedders / tests).
fn memory_scope_dir(
    workspace: &std::path::Path,
    scope: &str,
    uid: &str,
    knowledge_dir: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let _ = (scope, uid);
    match knowledge_dir {
        Some(kd) => kd.to_path_buf(),
        None => workspace.join(crate::memory::MEMORY_DIR_NAME),
    }
}

/// Whether a memory file belongs to the given scope (frontmatter-based).
/// Missing `scope` is treated as the agent layer; `scope=user` requires an
/// exact `user_id` match.
fn memory_file_in_scope(f: &crate::memory::MemoryFile, scope: &str, uid: &str) -> bool {
    let f_scope = f.scope.as_deref().unwrap_or("agent");
    if scope == "agent" {
        f_scope == "agent"
    } else {
        f_scope == "user" && f.user_id.as_deref() == Some(uid)
    }
}

/// Resolve the request's user_id via the shared resolver (routing_key → uid).
/// Falls back to the raw routing key when no resolver is installed.
fn memory_user_id(ctx: &ApiContext<'_>) -> String {
    ctx.user_resolver
        .get()
        .map(|r| r.resolve(ctx.user_id))
        .unwrap_or_else(|| ctx.user_id.to_string())
}

/// Route a management API request and return a JSON response string.
fn handle_api_request(
    id: &str,
    method: &str,
    params: &serde_json::Value,
    ctx: &ApiContext<'_>,
) -> String {
    let sm = match ctx.session_manager.get() {
        Some(sm) => sm,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "session manager not available"
            })
            .to_string();
        }
    };

    let user_id = ctx.user_id;

    match method {
        "sessions.list" => {
            // Aggregate across every routing_key linked to this identity via
            // `/link` (UserResolver), not just this web-client connection's
            // own routing_key — otherwise sessions created from a
            // previously-used channel become invisible the moment that
            // channel is linked to the account.
            let resolved = memory_user_id(ctx);
            let linked_routing_keys = sm.resolver().routing_keys_for(&resolved);
            let sessions = sm.list_sessions_for_user(&resolved);
            tracing::info!(
                raw_routing_key = %ctx.user_id,
                resolved_uid = %resolved,
                linked_routing_keys = ?linked_routing_keys,
                session_count = sessions.len(),
                "sessions.list diagnostic"
            );
            let active = sm.active_session_id(user_id);
            let result: Vec<serde_json::Value> = sessions.iter().map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "name": s.display_name,
                    "created_at": s.created_at.to_rfc3339(),
                    "owner": s.owner,
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
            // Evict cached SessionContext so the next message materializes
            // a fresh one for the newly-active session (mirrors /new).
            sm.drop_context(user_id);
            match sm.new_session(user_id, name) {
                Ok(info) => {
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
            // Evict cached SessionContext so the next message loads the
            // switched-to session's history (mirrors /switch).
            sm.drop_context(user_id);
            match sm.switch_session(user_id, session_id) {
                Ok(info) => {
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
            let existing_owner = sm.backend().get_session(session_id).map(|s| s.owner);
            match sm.delete_session(user_id, session_id) {
                Ok(()) => {
                    serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": null
                    }).to_string()
                }
                Err(e) => {
                    tracing::warn!(
                        attempted_user = %user_id,
                        session = %session_id,
                        actual_owner = existing_owner.as_deref().unwrap_or("<missing>"),
                        error_kind = ?e.kind(),
                        err = %e,
                        "failed to delete WebUI session"
                    );
                    serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": format!("failed to delete session: {}", e)
                    }).to_string()
                }
            }
        }

        "sessions.delete_message" => {
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
            let message_id = match params["message_id"].as_i64() {
                Some(n) => n,
                None => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": "missing message_id parameter"
                    }).to_string();
                }
            };
            match sm.backend().delete_message_by_id(session_id, message_id) {
                Ok(true) => serde_json::json!({
                    "type": "api_response",
                    "id": id,
                    "result": null
                }).to_string(),
                Ok(false) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": "message not found"
                }).to_string(),
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to delete message: {}", e)
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
            let scope = params["scope"].as_str().unwrap_or("all");
            match ctx.knowledge_dir.get().cloned().or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME))) {
                Some(dir) => {
                    let dir = dir.as_path();
                    let uid = memory_user_id(ctx);
                    // Collect (scope, file) rows; agent layer first so a
                    // same-named agent entry wins dedup (matches scan_merged).
                    let mut rows: Vec<(&str, crate::memory::MemoryFile)> = Vec::new();
                    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                    if scope == "all" || scope == "agent" {
                        for f in crate::memory::scan_memory_files(&memory_scope_dir(dir, "agent", &uid, Some(dir)))
                            .into_iter()
                            .filter(|f| memory_file_in_scope(f, "agent", &uid))
                        {
                            if seen.insert(f.name.clone()) {
                                rows.push(("agent", f));
                            }
                        }
                    }
                    if scope == "all" || scope == "user" {
                        for f in crate::memory::scan_memory_files(&memory_scope_dir(dir, "user", &uid, Some(dir)))
                            .into_iter()
                            .filter(|f| memory_file_in_scope(f, "user", &uid))
                        {
                            if seen.insert(f.name.clone()) {
                                rows.push(("user", f));
                            }
                        }
                    }
                    let all_files: Vec<crate::memory::MemoryFile> =
                        rows.iter().map(|(_, f)| f.clone()).collect();
                    let backlinks = crate::memory::build_backlinks(&all_files);
                    let result: Vec<serde_json::Value> = rows.iter().map(|(scope_name, f)| {
                        let bl_count = backlinks.get(&f.name).map(|b| b.len()).unwrap_or(0);
                        serde_json::json!({
                            "name": f.path.file_name().and_then(|n| n.to_str()).unwrap_or(&f.name).to_string(),
                            "size": std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0),
                            "mem_name": f.name,
                            "description": f.description,
                            "tags": f.tags,
                            "type": f.mem_type,
                            "inject": f.inject,
                            "scope": scope_name,
                            "link_count": f.links.len(),
                            "backlink_count": bl_count,
                            "created_at": f.created_at,
                            "updated_at": f.updated_at,
                            "content": f.content,
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
            }
        }

        "memory.write" => {
            let filename = match params["name"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
            };
            if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid filename" }).to_string();
            }
            let scope = match params["scope"].as_str() {
                Some("user") => "user",
                _ => "agent",
            };
            let content = params["content"].as_str().unwrap_or("");
            let dir_opt = ctx
                .knowledge_dir
                .get()
                .cloned()
                .or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME)));
            match dir_opt {
                Some(dir) => {
                    let uid = memory_user_id(ctx);
                    let memory_dir = memory_scope_dir(&dir, scope, &uid, Some(&dir));
                    let _ = std::fs::create_dir_all(&memory_dir);
                    // P1-B2: write ownership into frontmatter. If the payload
                    // already starts with frontmatter containing scope, trust
                    // it as-is (raw mgmt-API write); otherwise prepend a
                    // minimal ownership header before any existing frontmatter
                    // keys would break parsing — simplest correct form: write
                    // body-only content under a generated frontmatter.
                    let path = memory_dir.join(filename);
                    // Minimal header must satisfy parse_memory_file: name and
                    // type are required, otherwise the file is invisible to scans.
                    let stem = filename.strip_suffix(".md").unwrap_or(filename);
                    let body = if content.trim_start().starts_with("---") {
                        // Caller-supplied frontmatter: inject/patch scope keys.
                        let trimmed = content.trim_start();
                        let rest = &trimmed[3..];
                        let rest = rest.trim_start_matches(['\r', '\n']);
                        if let Some(end) = rest.find("\n---") {
                            let fm = &rest[..end];
                            let body = &rest[end + 4..];
                            if fm.lines().any(|l| l.trim().starts_with("scope:")) {
                                content.to_string()
                            } else {
                                let extra = if scope == "user" {
                                    format!("scope: user\nuser_id: {}", uid)
                                } else {
                                    "scope: agent".to_string()
                                };
                                format!("---\n{}\n{}\n---{}", fm, extra, body)
                            }
                        } else {
                            content.to_string()
                        }
                    } else {
                        let extra = if scope == "user" {
                            format!(
                                "name: {}\ntype: project\nscope: user\nuser_id: {}\n",
                                stem, uid
                            )
                        } else {
                            format!("name: {}\ntype: project\nscope: agent\n", stem)
                        };
                        format!("---\n{}---\n\n{}", extra, content)
                    };
                    match std::fs::write(&path, body) {
                        Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
                        Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to write file: {}", e) }).to_string(),
                    }
                }
                None => serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string(),
            }
        }

        "memory.delete" => {
            let filename = match params["name"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
            };
            if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid filename" }).to_string();
            }
            let scope = params["scope"].as_str();
            let dir_opt = ctx
                .knowledge_dir
                .get()
                .cloned()
                .or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME)));
            match dir_opt {
                Some(dir) => {
                    let uid = memory_user_id(ctx);
                    // P1-B2: single flat dir — scope matching is done on
                    // parsed frontmatter, not by path.
                    let candidates = crate::memory::scan_memory_files(&memory_scope_dir(
                        &dir,
                        "all",
                        &uid,
                        Some(&dir),
                    ));
                    let stem = filename.strip_suffix(".md").unwrap_or(filename);
                    let matches_scope = |f: &crate::memory::MemoryFile, scope_name: &str| {
                        f.name == stem && memory_file_in_scope(f, scope_name, &uid)
                    };
                    let result = match scope {
                        Some("user") => candidates
                            .iter()
                            .find(|f| matches_scope(f, "user"))
                            .map(|f| std::fs::remove_file(&f.path))
                            .unwrap_or_else(|| {
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    "file not found in user scope",
                                ))
                            }),
                        Some("agent") => candidates
                            .iter()
                            .find(|f| matches_scope(f, "agent"))
                            .map(|f| std::fs::remove_file(&f.path))
                            .unwrap_or_else(|| {
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    "file not found in agent scope",
                                ))
                            }),
                        _ => candidates
                            .iter()
                            .find(|f| matches_scope(f, "agent"))
                            .or_else(|| candidates.iter().find(|f| matches_scope(f, "user")))
                            .map(|f| std::fs::remove_file(&f.path))
                            .unwrap_or_else(|| {
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    "file not found",
                                ))
                            }),
                    };
                    match result {
                        Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
                        Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to delete file: {}", e) }).to_string(),
                    }
                }
                None => serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string(),
            }
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
            let dir_opt = ctx
                .knowledge_dir
                .get()
                .cloned()
                .or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME)));
            match dir_opt {
                Some(dir) => {
                    let uid = memory_user_id(ctx);
                    let scope = params["scope"].as_str();
                    // P1-B2: single flat dir — scope routing via frontmatter.
                    let candidates = crate::memory::scan_memory_files(&memory_scope_dir(
                        &dir,
                        "all",
                        &uid,
                        Some(&dir),
                    ));
                    let stem = filename.strip_suffix(".md").unwrap_or(filename);
                    let matches_scope = |f: &crate::memory::MemoryFile, scope_name: &str| {
                        f.name == stem && memory_file_in_scope(f, scope_name, &uid)
                    };
                    let found = match scope {
                        Some("user") => candidates
                            .iter()
                            .find(|f| matches_scope(f, "user"))
                            .map(|f| ("user".to_string(), std::fs::read_to_string(&f.path).ok())),
                        Some("agent") => candidates
                            .iter()
                            .find(|f| matches_scope(f, "agent"))
                            .map(|f| ("agent".to_string(), std::fs::read_to_string(&f.path).ok())),
                        // Backwards-compatible default: agent layer first,
                        // fall back to the per-user layer.
                        _ => candidates
                            .iter()
                            .find(|f| matches_scope(f, "agent"))
                            .or_else(|| candidates.iter().find(|f| matches_scope(f, "user")))
                            .map(|f| {
                                let s = if memory_file_in_scope(f, "agent", &uid) {
                                    "agent"
                                } else {
                                    "user"
                                };
                                (s.to_string(), std::fs::read_to_string(&f.path).ok())
                            }),
                    }
                    .and_then(|(s, content)| content.map(|c| (s, c)));
                    match found {
                        Some((s, content)) => serde_json::json!({
                            "type": "api_response",
                            "id": id,
                            "result": { "name": filename, "content": content, "scope": s }
                        }).to_string(),
                        None => serde_json::json!({
                            "type": "api_error",
                            "id": id,
                            "error": format!("failed to read file: {}", filename)
                        }).to_string(),
                    }
                }
                None => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": "workspace directory not configured"
                }).to_string(),
            }
        }

        // ── file.read: read a workspace-relative file and return base64 ──
        "file.read" => {
            let rel_path = match params["path"].as_str() {
                Some(s) => s,
                None => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": "missing path parameter"
                    }).to_string();
                }
            };
            // Reject absolute paths and traversal attempts.
            if rel_path.starts_with('/') || rel_path.starts_with('\\')
                || rel_path.contains("..") || rel_path.contains('~')
            {
                return serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": "invalid path"
                }).to_string();
            }
            match ctx.workspace_dir.get() {
                Some(dir) => {
                    let abs = dir.join(rel_path);
                    match std::fs::read(&abs) {
                        Ok(bytes) => {
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            // Infer MIME from extension.
                            let mime = match abs.extension().and_then(|e| e.to_str()).unwrap_or("") {
                                "jpg" | "jpeg" => "image/jpeg",
                                "png" => "image/png",
                                "gif" => "image/gif",
                                "webp" => "image/webp",
                                "svg" => "image/svg+xml",
                                "bmp" => "image/bmp",
                                "ico" => "image/x-icon",
                                "mp3" => "audio/mpeg",
                                "ogg" => "audio/ogg",
                                "wav" => "audio/wav",
                                "mp4" => "video/mp4",
                                "webm" => "video/webm",
                                "mov" => "video/quicktime",
                                "mkv" => "video/x-matroska",
                                "avi" => "video/x-msvideo",
                                "flac" => "audio/flac",
                                "m4a" => "audio/m4a",
                                "aac" => "audio/aac",
                                "pdf" => "application/pdf",
                                _ => "application/octet-stream",
                            };
                            serde_json::json!({
                                "type": "api_response",
                                "id": id,
                                "result": { "path": rel_path, "data": b64, "mime": mime, "size": bytes.len() }
                            }).to_string()
                        }
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
            }
        }

        "config.get" => {
            let specs = ctx.tool_specs.read();
            let ws_dir = ctx.workspace_dir.get();
            let cfg_path = ctx.config_path.get();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": {
                    "tool_count": specs.len(),
                    "workspace_dir": ws_dir.map(|p| p.to_string_lossy().to_string()),
                    "config_path": cfg_path.map(|p| p.to_string_lossy().to_string()),
                }
            }).to_string()
        }

        "config.get_raw" => {
            match ctx.config_path.get() {
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
            match ctx.config_path.get() {
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
            let result: Vec<serde_json::Value> = match ctx.skill_manager.get() {
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

        "skills.read" => {
            let name = match params["name"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
            };
            if !is_safe_skill_name(name) {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid skill name" }).to_string();
            }
            // Prefer SkillManager.skill_dir so frontmatter name may differ from directory name.
            let path = match resolve_skill_dir(ctx, name) {
                Some(dir) => dir.join("SKILL.md"),
                None => {
                    return serde_json::json!({
                        "type": "api_error",
                        "id": id,
                        "error": format!("skill '{}' not found", name)
                    })
                    .to_string();
                }
            };
            match std::fs::read_to_string(&path) {
                Ok(content) => serde_json::json!({
                    "type": "api_response",
                    "id": id,
                    "result": { "name": name, "content": content }
                })
                .to_string(),
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to read skill file: {}", e)
                })
                .to_string(),
            }
        }

        "skills.write" => {
            let name = match params["name"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
            };
            if !is_safe_skill_name(name) {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid skill name" }).to_string();
            }
            let content = params["content"].as_str().unwrap_or("");
            let Some(workspace) = ctx.workspace_dir.get() else {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string();
            };
            // Existing skills keep their real directory (name may != dir name).
            // New skills fall back to workspace/skills/{name}.
            let skill_dir = resolve_skill_dir(ctx, name)
                .unwrap_or_else(|| workspace.join("skills").join(name));
            let path = skill_dir.join("SKILL.md");
            if let Err(e) = std::fs::create_dir_all(&skill_dir) {
                return serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to create skill directory: {}", e)
                })
                .to_string();
            }
            match std::fs::write(&path, content) {
                Ok(()) => {
                    reload_skills_from_workspace(ctx, workspace);
                    serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string()
                }
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to write skill file: {}", e)
                })
                .to_string(),
            }
        }

        "skills.delete" => {
            let name = match params["name"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
            };
            if !is_safe_skill_name(name) {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid skill name" }).to_string();
            }
            let Some(workspace) = ctx.workspace_dir.get() else {
                return serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string();
            };
            let Some(skill_dir) = resolve_skill_dir(ctx, name) else {
                return serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("skill '{}' not found", name)
                })
                .to_string();
            };
            match std::fs::remove_dir_all(&skill_dir) {
                Ok(()) => {
                    reload_skills_from_workspace(ctx, workspace);
                    serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string()
                }
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to delete skill: {}", e)
                })
                .to_string(),
            }
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
            match ctx.provider_registry.get() {
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
                ContentPart::File {
                    path, mime_type, ..
                } if crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                    == crate::providers::media::FileModality::Image =>
                {
                    has_image = true
                }
                _ => {}
            }
        }
        match m.role.as_str() {
            "user" => {
                let has_files = m
                    .parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::File { .. }));
                let content = if !text.is_empty() {
                    text
                } else if has_image {
                    "🖼️ (image)".to_string()
                } else if has_files {
                    "📎 (file)".to_string()
                } else {
                    continue;
                };
                // Collect file references for the frontend to display.
                let mut images: Vec<serde_json::Value> = Vec::new();
                let mut files: Vec<serde_json::Value> = Vec::new();
                for p in &m.parts {
                    if let ContentPart::File {
                        path,
                        mime_type,
                        name,
                        ..
                    } = p
                    {
                        let entry = serde_json::json!({
                            "path": path,
                            "mime": mime_type,
                            "name": name,
                        });
                        let is_image =
                            crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                                == crate::providers::media::FileModality::Image;
                        if is_image {
                            images.push(entry);
                        } else {
                            files.push(entry);
                        }
                    }
                }
                counter += 1;
                let mut msg = serde_json::json!({
                    "role": "user",
                    "content": content,
                    "id": format!("h-{}", counter),
                });
                if !images.is_empty() {
                    msg["images"] = serde_json::Value::Array(images);
                }
                if !files.is_empty() {
                    msg["files"] = serde_json::Value::Array(files);
                }
                out.push(msg);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{SessionManager, SkillManager, UserResolver};
    use crate::providers::capability_tool::ToolSpec;
    use crate::providers::ProviderRegistry;

    const USER_KEY: &str = "client:default:web-user:default";
    const USER_UID: &str = "myclaw/u/019fe342-test";
    const OTHER_UID: &str = "myclaw/u/019fe342-other";

    fn mem_body(name: &str, body: &str) -> String {
        // No `scope` field → agent layer (matches legacy agent files).
        format!(
            "---\nname: \"{name}\"\ndescription: \"test entry\"\ntype: \"project\"\ninject: \"search\"\ncreated_at: \"2026-08-14\"\ntags: []\n---\n\n{body}"
        )
    }

    fn user_mem_body(name: &str, body: &str, uid: &str) -> String {
        format!(
            "---\nname: \"{name}\"\nscope: \"user\"\nuser_id: \"{uid}\"\ndescription: \"test entry\"\ntype: \"project\"\ninject: \"search\"\ncreated_at: \"2026-08-14\"\ntags: []\n---\n\n{body}"
        )
    }

    fn write_mem(path: &std::path::Path, name: &str, body: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(format!("{name}.md")), mem_body(name, body)).unwrap();
    }

    fn write_user_mem(path: &std::path::Path, name: &str, body: &str, uid: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join(format!("{name}.md")),
            user_mem_body(name, body, uid),
        )
        .unwrap();
    }

    fn test_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // P1-B2: single flat memory dir; ownership via frontmatter.
        // (A name is unique in the flat dir — the old two-layer
        // same-name-in-both-scopes case no longer exists.)
        let mem = ws.join("memory");
        write_mem(&mem, "agent_only", "agent body");
        write_mem(&mem, "agent_second", "second agent body");
        write_user_mem(&mem, "user_only", "user body", USER_UID);
        write_user_mem(&mem, "user_second", "second user body", USER_UID);
        // Another user's private entry — must be invisible to USER_UID.
        write_user_mem(&mem, "other_user_only", "other body", OTHER_UID);
        tmp
    }

    /// Build an ApiContext with a resolver pinning USER_KEY → USER_UID.
    fn api(method: &str, params: serde_json::Value, ws: &std::path::Path) -> serde_json::Value {
        let sm: Arc<OnceLock<Arc<SessionManager>>> = Arc::new(OnceLock::new());
        let wd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
        let ur: Arc<OnceLock<Arc<UserResolver>>> = Arc::new(OnceLock::new());
        let ts: Arc<RwLock<Vec<ToolSpec>>> = Arc::new(RwLock::new(Vec::new()));
        let cp: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
        let sk: Arc<OnceLock<Arc<RwLock<SkillManager>>>> = Arc::new(OnceLock::new());
        let pr: Arc<OnceLock<Arc<dyn ProviderRegistry>>> = Arc::new(OnceLock::new());
        let resolver = Arc::new(UserResolver::new());
        resolver.set(USER_KEY.to_string(), USER_UID.to_string());
        let _ = ur.set(Arc::clone(&resolver));
        // Mirrors daemon.rs: SessionManager and ApiContext share one
        // resolver instance — sessions.list depends on that (G44).
        let _ = sm.set(Arc::new(SessionManager::in_memory().with_resolver(resolver)));
        let _ = wd.set(ws.to_path_buf());
        let kd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
        let _ = kd.set(ws.join("memory"));
        let ctx = ApiContext {
            user_id: USER_KEY,
            session_manager: &sm,
            tool_specs: &ts,
            workspace_dir: &wd,
            knowledge_dir: &kd,
            config_path: &cp,
            skill_manager: &sk,
            provider_registry: &pr,
            user_resolver: &ur,
        };
        let resp = handle_api_request("t1", method, &params, &ctx);
        serde_json::from_str(&resp).unwrap()
    }

    fn rows(resp: &serde_json::Value) -> Vec<serde_json::Value> {
        resp["result"].as_array().cloned().unwrap_or_default()
    }

    #[test]
    fn memory_list_merges_scopes_with_dedup() {
        let tmp = test_workspace();
        let resp = api("memory.list", serde_json::json!({}), tmp.path());
        assert_eq!(resp["type"], "api_response");
        let r = rows(&resp);
        // agent(2) + own user(2) = 4; other user's entry invisible
        assert_eq!(r.len(), 4);
        let agent = r.iter().find(|x| x["mem_name"] == "agent_only").unwrap();
        assert_eq!(agent["scope"], "agent");
        assert_eq!(agent["content"], "agent body");
        let user_only = r.iter().find(|x| x["mem_name"] == "user_only").unwrap();
        assert_eq!(user_only["scope"], "user");
        // Missing-scope fixture files count as agent layer.
        assert_eq!(
            r.iter()
                .filter(|x| x["scope"] == "agent")
                .count(),
            2
        );
    }

    #[test]
    fn memory_list_scope_filter() {
        let tmp = test_workspace();
        let resp = api(
            "memory.list",
            serde_json::json!({ "scope": "user" }),
            tmp.path(),
        );
        let r = rows(&resp);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x["scope"] == "user"));
        let resp_agent = api(
            "memory.list",
            serde_json::json!({ "scope": "agent" }),
            tmp.path(),
        );
        let r_agent = rows(&resp_agent);
        assert_eq!(r_agent.len(), 2);
        assert!(r_agent.iter().all(|x| x["scope"] == "agent"));
    }

    #[test]
    fn memory_read_scope_routing() {
        let tmp = test_workspace();
        // No scope: agent entry resolves to agent layer.
        let resp = api(
            "memory.read",
            serde_json::json!({ "name": "agent_only.md" }),
            tmp.path(),
        );
        assert_eq!(resp["result"]["content"], mem_body("agent_only", "agent body"));
        assert_eq!(resp["result"]["scope"], "agent");
        // Explicit user scope on a user-owned file.
        let resp = api(
            "memory.read",
            serde_json::json!({ "name": "user_only.md", "scope": "user" }),
            tmp.path(),
        );
        assert_eq!(resp["result"]["content"], user_mem_body("user_only", "user body", USER_UID));
        assert_eq!(resp["result"]["scope"], "user");
        // User-only entry found via fallback (agent miss → user hit).
        let resp = api(
            "memory.read",
            serde_json::json!({ "name": "user_only.md" }),
            tmp.path(),
        );
        assert_eq!(resp["result"]["content"], user_mem_body("user_only", "user body", USER_UID));
        assert_eq!(resp["result"]["scope"], "user");
        // Asking for user scope on an agent file misses.
        let resp = api(
            "memory.read",
            serde_json::json!({ "name": "agent_only.md", "scope": "user" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_error");
        // Missing entry.
        let resp = api(
            "memory.read",
            serde_json::json!({ "name": "nope" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_error");
    }

    #[test]
    fn memory_write_user_scope() {
        let tmp = test_workspace();
        let resp = api(
            "memory.write",
            serde_json::json!({ "name": "fresh_user.md", "content": "new user body", "scope": "user" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_response");
        let user_path = tmp.path().join("memory").join("fresh_user.md");
        let written = std::fs::read_to_string(&user_path).unwrap();
        assert!(written.contains("scope: user"));
        assert!(written.contains(&format!("user_id: {}", USER_UID)));
        assert!(written.contains("new user body"));
        // Visible to this user via scope=user listing…
        let resp = api(
            "memory.list",
            serde_json::json!({ "scope": "user" }),
            tmp.path(),
        );
        let r = rows(&resp);
        assert!(r.iter().any(|x| x["mem_name"] == "fresh_user"));
        // Default scope (agent) writes the agent marker instead.
        let resp = api(
            "memory.write",
            serde_json::json!({ "name": "default_scope.md", "content": "body" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_response");
        let agent_written =
            std::fs::read_to_string(tmp.path().join("memory").join("default_scope.md")).unwrap();
        assert!(agent_written.contains("scope: agent"));
        assert!(!agent_written.contains("user_id"));
    }

    #[test]
    fn memory_delete_scope_routing() {
        let tmp = test_workspace();
        // Explicit user scope removes the user-owned file.
        let resp = api(
            "memory.delete",
            serde_json::json!({ "name": "user_second.md", "scope": "user" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_response");
        assert!(!tmp.path().join("memory").join("user_second.md").exists());
        // Default fallback removes the agent-layer copy.
        let resp = api(
            "memory.delete",
            serde_json::json!({ "name": "agent_only.md" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_response");
        assert!(!tmp.path().join("memory").join("agent_only.md").exists());        // Another user's entry is not deletable via any scope param.
        let resp = api(
            "memory.delete",
            serde_json::json!({ "name": "other_user_only.md", "scope": "user" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_error");
        assert!(tmp.path().join("memory").join("other_user_only.md").exists());
    }

    #[test]
    fn memory_user_isolation_flat_dir() {
        let tmp = test_workspace();
        // This user sees: agent layer (2) + own user layer (2) — NOT the
        // other user's private entry.
        let resp = api("memory.list", serde_json::json!({}), tmp.path());
        let r = rows(&resp);
        assert_eq!(r.len(), 4);
        assert!(r.iter().any(|x| x["mem_name"] == "agent_only"));
        assert!(r.iter().any(|x| x["mem_name"] == "user_only"));
        assert!(!r.iter().any(|x| x["mem_name"] == "other_user_only"));
        // Read cannot reach the other user's entry either.
        let resp = api(
            "memory.read",
            serde_json::json!({ "name": "other_user_only.md", "scope": "user" }),
            tmp.path(),
        );
        assert_eq!(resp["type"], "api_error");
    }

    /// Regression test for the bug where linking a channel's routing_key to
    /// an existing user (via `/link`) did not surface that user's
    /// pre-existing sessions in the web client's session list: `sessions.list`
    /// used to call `list_sessions(raw_routing_key)` instead of the
    /// resolver-aware `list_sessions_for_user(resolved_uid)` (G44).
    #[test]
    fn sessions_list_surfaces_linked_channel_sessions() {
        let tmp = test_workspace();
        let sm: Arc<OnceLock<Arc<SessionManager>>> = Arc::new(OnceLock::new());
        let wd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
        let ur: Arc<OnceLock<Arc<UserResolver>>> = Arc::new(OnceLock::new());
        let ts: Arc<RwLock<Vec<ToolSpec>>> = Arc::new(RwLock::new(Vec::new()));
        let cp: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
        let sk: Arc<OnceLock<Arc<RwLock<SkillManager>>>> = Arc::new(OnceLock::new());
        let pr: Arc<OnceLock<Arc<dyn ProviderRegistry>>> = Arc::new(OnceLock::new());
        let kd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
        let _ = kd.set(tmp.path().join("memory"));
        let _ = wd.set(tmp.path().to_path_buf());

        // Both a pre-existing Telegram channel and the web client's own
        // routing_key are folded into the same uid — exactly what
        // `/link` + `/link_confirm` does at runtime.
        const TELEGRAM_KEY: &str = "telegram:default:12345";
        let resolver = Arc::new(UserResolver::new());
        resolver.set(USER_KEY.to_string(), USER_UID.to_string());
        resolver.set(TELEGRAM_KEY.to_string(), USER_UID.to_string());
        let _ = ur.set(Arc::clone(&resolver));

        let session_manager = Arc::new(SessionManager::in_memory().with_resolver(resolver));
        // A session that already existed on the Telegram channel, created
        // before the user ever touched the web client.
        let telegram_session = session_manager
            .new_session(TELEGRAM_KEY, Some("old telegram chat"))
            .unwrap();
        let _ = sm.set(session_manager);

        let ctx = ApiContext {
            user_id: USER_KEY,
            session_manager: &sm,
            tool_specs: &ts,
            workspace_dir: &wd,
            knowledge_dir: &kd,
            config_path: &cp,
            skill_manager: &sk,
            provider_registry: &pr,
            user_resolver: &ur,
        };

        let resp: serde_json::Value = serde_json::from_str(&handle_api_request(
            "t1",
            "sessions.list",
            &serde_json::json!({}),
            &ctx,
        ))
        .unwrap();
        let r = rows(&resp);
        assert!(
            r.iter().any(|s| s["id"] == telegram_session.id),
            "linked channel's pre-existing session should be visible: {r:?}"
        );
    }

    /// Regression test for the /link verification code silently vanishing:
    /// `friends.rs::notify_peer` builds `MessageReceiver` with a bare,
    /// channel-local id (the convention every `Channel` impl's `send_message`
    /// expects, matching telegram/qqbot's own addressing), but
    /// `session_buses` is keyed by the full `client:default:<tail>` session
    /// key. Before normalizing in `send_message`, that mismatch meant the
    /// bus lookup always missed, the message was dropped, and — because the
    /// miss path still returns `Ok(..)` — `/link` reported the code as sent
    /// with nothing ever delivered to the browser.
    #[tokio::test]
    async fn send_message_resolves_bare_receiver_id_to_full_session_key() {
        let channel = ClientChannel::new(ClientConfig::default());
        let full_key = "client:default:web-user:default".to_string();
        channel
            .session_buses
            .write()
            .insert(full_key.clone(), Arc::new(SyncMutex::new(SessionOutputBus::new())));

        // Bare id, exactly as friends.rs::notify_peer builds it via split_rk.
        let msg = ChannelOutboundMessage::text("web-user:default", "🔐 code: 123456");
        channel.send_message(&msg).await.unwrap();

        let bus = channel.session_buses.read().get(&full_key).unwrap().clone();
        let queued = bus.lock().drain_messages();
        assert_eq!(queued.len(), 1, "message should land in the full-key bus");
        assert!(queued[0].contains("123456"));
    }
}
