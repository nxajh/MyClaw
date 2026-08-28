//! WebSocketChannel core: `WebSocketConnection`, `WebSocketChannel` and its inherent
//! impl (construction, deferred-init setters, the WebSocket-server `start`
//! loop), extracted verbatim from `client.rs` (RFC
//! docs/websocket-client-split-rfc.md, batch 3: pure move).
//!
//! Only `pub(super)` was added where sibling `turn.rs` references the item:
//! fields `config` / `session_buses` / `message_rx` / `user_resolver` and
//! the `start()` method.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use parking_lot::{Mutex as SyncMutex, RwLock};
use std::sync::OnceLock;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender,
};
use crate::config::channel::WebSocketConfig;

use super::api::{ApiContext, handle_api_request};
use super::bus::{SessionOutputBus, bus_key_candidates};

// ── Client Connection ───────────────────────────────────────────────────────

/// A single connected client.
struct WebSocketConnection {
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

// ── WebSocketChannel ───────────────────────────────────────────────────────────

pub struct WebSocketChannel {
    pub(super) config: WebSocketConfig,
    /// Outgoing messages for Orchestrator (filled by WS handlers).
    message_tx: mpsc::Sender<ChannelInboundMessage>,
    /// One-time take for listen().
    pub(super) message_rx: Mutex<Option<mpsc::Receiver<ChannelInboundMessage>>>,
    /// Pre-bound listener passed from the old process during hot switch.
    /// When set, start() reuses it instead of calling bind().
    pre_bound: SyncMutex<Option<std::net::TcpListener>>,
    /// Per-session output buses (survive WS disconnects).
    pub(super) session_buses: Arc<RwLock<HashMap<String, Arc<SyncMutex<SessionOutputBus>>>>>,
    /// Active connections: connection_id → WebSocketConnection.
    connections: Arc<RwLock<HashMap<String, WebSocketConnection>>>,
    /// Session manager for management API (set once after construction).
    session_manager: Arc<OnceLock<Arc<crate::agents::SessionManager>>>,
    /// Tool specs for management API (set after construction).
    tool_specs: Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
    /// Workspace directory for memory API (set once after construction).
    workspace_dir: Arc<OnceLock<std::path::PathBuf>>,
    /// Memory root ({base_dir}/memory) — single flat memory pool where
    /// ownership is a frontmatter attribute (set once after construction).
    memory_root: Arc<OnceLock<std::path::PathBuf>>,
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
    pub(super) user_resolver: Arc<OnceLock<Arc<crate::agents::UserResolver>>>,
}

impl WebSocketChannel {
    pub fn new(config: WebSocketConfig) -> Self {
        let (message_tx, message_rx) = mpsc::channel(100);
        Self {
            config,
            message_tx,
            message_rx: Mutex::new(Some(message_rx)),
            pre_bound: SyncMutex::new(None),
            session_buses: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(HashMap::new())),
            session_manager: Arc::new(OnceLock::new()),
            tool_specs: Arc::new(RwLock::new(Vec::new())),
            workspace_dir: Arc::new(OnceLock::new()),
            memory_root: Arc::new(OnceLock::new()),
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

    /// Set the memory root (called from daemon.rs after construction).
    pub fn set_memory_root(&self, dir: std::path::PathBuf) {
        let _ = self.memory_root.set(dir);
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
    pub(super) async fn start(&self) -> anyhow::Result<()> {
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
        let session_manager = self.session_manager.clone();
        let tool_specs = self.tool_specs.clone();
        let workspace_dir = self.workspace_dir.clone();
        let memory_root = self.memory_root.clone();
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

                        let (mut ws_sink, mut ws_stream) = ws_stream.split();

                        // Create mpsc channel for outgoing messages to this client.
                        let (ws_sender, mut ws_receiver) = mpsc::channel::<String>(64);

                        // Register connection. conn_id is only for logging and
                        // connection bookkeeping — bus keys are identity-based
                        // (client:default:web-user:{user}), assigned on auth.
                        {
                            let mut conns = connections.write();
                            conns.insert(
                                conn_id.clone(),
                                WebSocketConnection {
                                    ws_sender: ws_sender.clone(),
                                    active_session: String::new(),
                                    sessions: std::collections::HashSet::new(),
                                },
                            );
                        }

                        let conn_id_clone = conn_id.clone();
                        // Identity routing key. Initially unset; assigned on auth
                        // (or on first message for token-less TUI flows).
                        let mut session_key_clone = String::new();
                        let message_tx_clone = message_tx.clone();
                        let session_buses_clone = session_buses.clone();
                        let connections_clone = connections.clone();
                        let session_manager_clone = session_manager.clone();
                        let tool_specs_clone = tool_specs.clone();
                        let workspace_dir_clone = workspace_dir.clone();
                        let memory_root_clone = memory_root.clone();
                        let config_path_clone = config_path.clone();
                        let skill_manager_clone = skill_manager.clone();
                        let provider_registry_clone = provider_registry.clone();
                        let user_resolver_clone = user_resolver.clone();
                        let auth_token_clone = auth_token.clone();

                        tracing::info!(
                            conn_id = %conn_id,
                            peer = %peer_addr,
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
                                // Permission/session owner identity. For WebSocket this is a stable
                                // logical user; client_id remains only a per-browser device id.
                                // Default matches the auth branch's fallback ("default") so
                                // token-less deployments derive the same identity rk
                                // (client:default:web-user:default) on their first message.
                                let mut client_user_id = "web-user:default".to_string();

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
                                            // client_id is a per-browser device id — no longer
                                            // part of the bus key. Logged for tracing only.
                                            if let Some(cid) = parsed["client_id"].as_str() {
                                                let cid = cid.trim();
                                                if !cid.is_empty() {
                                                    tracing::debug!(
                                                        conn_id = %conn_id_clone,
                                                        client_id = %cid,
                                                        "auth client_id received (informational)"
                                                    );
                                                }
                                            }
                                            // Identity bus key == the identity routing key
                                            // used by send_message / notify_peer, so
                                            // cross-channel pushes always hit this bus.
                                            session_key_clone =
                                                format!("client:default:{}", client_user_id);
                                            // Track identity ownership on the connection.
                                            {
                                                let mut conns = connections_clone.write();
                                                if let Some(conn) =
                                                    conns.get_mut(&conn_id_clone)
                                                {
                                                    conn.sessions
                                                        .insert(session_key_clone.clone());
                                                    conn.active_session =
                                                        session_key_clone.clone();
                                                }
                                            }
                                            // Ensure bus + subscribe + (0->1) replay.
                                            let api_user =
                                                format!("client:default:{}", client_user_id);
                                            let session_id = session_manager_clone
                                                .get()
                                                .and_then(|sm| {
                                                    sm.active_session_id(&api_user)
                                                })
                                                .unwrap_or_default();
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
                                                session = %session_key_clone,
                                                "session bound to identity key on auth"
                                            );
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
                                            // ({name, content} pairs sent by the WebSocket).
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
                                            // auth wasn't called, e.g. token-less TUI). For
                                            // WebSocket the bus was already subscribed during auth.
                                            // session_key_clone is already the identity rk;
                                            // derive it from client_user_id if still unset.
                                            if session_key_clone.is_empty() {
                                                session_key_clone =
                                                    format!("client:default:{}", client_user_id);
                                                let mut conns = connections_clone.write();
                                                if let Some(conn) = conns.get_mut(&conn_id_clone)
                                                {
                                                    conn.sessions
                                                        .insert(session_key_clone.clone());
                                                    conn.active_session =
                                                        session_key_clone.clone();
                                                }
                                            }
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
                                            // session_key_clone is already the
                                            // identity rk (exact hit), but go
                                            // through the same resolver path as
                                            // send_message for uniformity.
                                            let candidates = bus_key_candidates(
                                                &user_resolver_clone,
                                                &session_key_clone,
                                            );
                                            let buses = session_buses_clone.read();
                                            for key in &candidates {
                                                if let Some(bus) = buses.get(key) {
                                                    bus.lock().cancel.cancel();
                                                    tracing::debug!(session = %key, "turn cancelled by client");
                                                    break;
                                                }
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
                                            // (channel:account:sender) so the WebSocket
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
                                                    memory_root: &memory_root_clone,
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
                            // the write on session_buses.
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
