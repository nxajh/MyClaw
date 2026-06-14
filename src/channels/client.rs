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

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::{Mutex as SyncMutex, RwLock};
use std::sync::OnceLock;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::agents::TurnEvent;
use crate::channels::message::{
    Channel, ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent,
    ChannelOutboundMessage, LocalFileBody, MessageReceiver, MessageSender, OutboundSendResult,
};
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
    message_tx: mpsc::Sender<ChannelInboundMessage>,
    /// One-time take for listen().
    message_rx: Mutex<Option<mpsc::Receiver<ChannelInboundMessage>>>,
    /// Pre-bound listener passed from the old process during hot switch.
    /// When set, start() reuses it instead of calling bind().
    pre_bound: SyncMutex<Option<std::net::TcpListener>>,
    /// Per-session streaming context.
    stream_contexts: Arc<RwLock<HashMap<String, StreamContext>>>,
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
    /// Config file path for config read/write API (set once after construction).
    config_path: Arc<OnceLock<std::path::PathBuf>>,
    /// Skill manager for skills API (set once after construction). The
    /// inner `RwLock` stays — skills reload at runtime; only the outer
    /// deferred-init wrapper is flattened from `RwLock<Option<_>>`.
    skill_manager: Arc<OnceLock<Arc<RwLock<crate::agents::SkillManager>>>>,
    /// Service registry for models API (set once after construction).
    provider_registry: Arc<OnceLock<Arc<dyn crate::providers::ProviderRegistry>>>,
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
            session_manager: Arc::new(OnceLock::new()),
            tool_specs: Arc::new(RwLock::new(Vec::new())),
            workspace_dir: Arc::new(OnceLock::new()),
            config_path: Arc::new(OnceLock::new()),
            skill_manager: Arc::new(OnceLock::new()),
            provider_registry: Arc::new(OnceLock::new()),
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

                                while let Some(msg_result) =
                                    futures_util::StreamExt::next(&mut ws_stream).await
                                {
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
                                            if let Some(cid) = parsed["client_id"].as_str() {
                                                let cid = cid.trim();
                                                if !cid.is_empty() {
                                                    client_id = format!("web:{}", cid);
                                                }
                                            }
                                            let _ = ws_sender
                                                .send(r#"{"type":"auth_ok"}"#.to_string())
                                                .await;
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
                                                    let raw = match entry.get("data").and_then(|v| v.as_str()) {
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
                                                    let ext = crate::providers::media::modality_from_mime(
                                                        Some(&mime),
                                                        &file_name,
                                                    );
                                                    let suffix = match ext {
                                                        crate::providers::media::FileModality::Image => "img",
                                                        crate::providers::media::FileModality::Audio => "audio",
                                                        crate::providers::media::FileModality::Video => "video",
                                                        crate::providers::media::FileModality::Other => "file",
                                                    };
                                                    let temp_path = std::env::temp_dir().join(format!(
                                                        "myclaw-client-{suffix}-{}",
                                                        uuid::Uuid::new_v4()
                                                    ));
                                                    if tokio::fs::write(&temp_path, &bytes).await.is_ok() {
                                                        image_files.push(ChannelFile {
                                                            meta: ChannelFileMeta {
                                                                file_name,
                                                                mime_type: Some(mime),
                                                                size_bytes: Some(bytes.len() as u64),
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

                                            // E32: stream context indexed by reply_target.
                                            // For ClientChannel today reply_target ==
                                            // session_key, but using reply_target
                                            // semantically lets sub-agents push events
                                            // to the parent's UI when their session
                                            // inherits the parent's reply_target.
                                            let (event_tx, mut event_rx) =
                                                mpsc::channel::<TurnEvent>(64);
                                            let cancel = CancellationToken::new();
                                            let reply_target_key = session_key_clone.clone();
                                            {
                                                let mut contexts = stream_contexts_clone.write();
                                                contexts.insert(
                                                    reply_target_key.clone(),
                                                    StreamContext {
                                                        event_tx: event_tx.clone(),
                                                        cancel: cancel.clone(),
                                                    },
                                                );
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
                                                            tracing::warn!(
                                                                "failed to serialize TurnEvent: {}",
                                                                e
                                                            );
                                                            continue;
                                                        }
                                                    };
                                                    if fwd_sender.send(json).await.is_err() {
                                                        break; // Client gone
                                                    }
                                                }
                                            });

                                            // Create ChannelInboundMessage for Orchestrator.
                                            let channel_msg = ChannelInboundMessage {
                                                id: format!(
                                                    "{}-{}",
                                                    conn_id_clone,
                                                    chrono::Utc::now().timestamp_millis()
                                                ),
                                                sender: MessageSender::new(client_id.clone()),
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
                                            };

                                            if message_tx_clone.send(channel_msg).await.is_err() {
                                                tracing::warn!(
                                                    "orchestrator message channel closed"
                                                );
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
                                                format!("client:default:{}", client_id);
                                            let resp = handle_api_request(
                                                &id,
                                                &method,
                                                &params,
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
                                _ = outgoing => {},
                                _ = incoming => {},
                            }

                            // Clean up on disconnect. Collect the
                            // connection's owned session_keys first, then
                            // drop the connections read-lock before taking
                            // writes on session_owners + stream_contexts.
                            // Also cancel any in-flight turns BEFORE the
                            // stream_contexts removal so the cancel signal
                            // actually reaches Agent::run.
                            let owned_keys: Vec<String> = {
                                let conns = connections_clone.read();
                                conns
                                    .get(&conn_id_clone)
                                    .map(|conn| conn.sessions.iter().cloned().collect())
                                    .unwrap_or_default()
                            };
                            // F7: cancel + remove stream_contexts entries
                            // for every session this connection owned —
                            // without this, the StreamContext lingers in
                            // the map forever (per-disconnect leak) and
                            // any in-flight Agent::run keeps streaming
                            // into a dead channel.
                            {
                                let mut contexts = stream_contexts_clone.write();
                                for sk in &owned_keys {
                                    if let Some(ctx) = contexts.remove(sk) {
                                        ctx.cancel.cancel();
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

    async fn send_message(
        &self,
        msg: &ChannelOutboundMessage,
    ) -> anyhow::Result<OutboundSendResult> {
        // msg.receiver.id is the session_key (e.g. "client:ws-1")
        // Find the connection that owns this session.
        let ws_sender = {
            let owners = self.session_owners.read();
            let conn_id = match owners.get(&msg.receiver.id) {
                Some(id) => id.clone(),
                None => {
                    tracing::warn!(recipient = %msg.receiver.id, "no connection found for session");
                    return Ok(OutboundSendResult::empty());
                }
            };
            drop(owners); // Release lock before await.

            let conns = self.connections.read();
            conns.get(&conn_id).map(|conn| conn.ws_sender.clone())
        }; // Lock released here.

        if let Some(sender) = ws_sender {
            if msg.content.files.is_empty() {
                let outgoing = serde_json::json!({
                    "type": "message",
                    "session": msg.receiver.id,
                    "content": msg.content.text,
                });
                let _ = sender.send(outgoing.to_string()).await;
            } else {
                // Send each file as a separate WebSocket message with base64 data.
                use base64::Engine;
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
                    let outgoing = serde_json::json!({
                        "type": "file",
                        "session": msg.receiver.id,
                        "file_name": file.meta.file_name,
                        "mime_type": file.meta.mime_type,
                        "size": file.meta.size_bytes,
                        "data": b64,
                        "caption": caption,
                    });
                    let _ = sender.send(outgoing.to_string()).await;
                }
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
        let contexts = self.stream_contexts.read();
        let ctx = contexts.get(reply_target)?;
        Some(Box::new(ClientTurnStream {
            reply_target: reply_target.to_string(),
            event_tx: ctx.event_tx.clone(),
            cancel: ctx.cancel.clone(),
            status: crate::channels::StreamDelivery::Pending,
            finished: false,
        }))
    }
}

/// Per-turn streaming handle for ClientChannel (RFC §7.6).
///
/// Wraps the already-registered `StreamContext.event_tx` plus its cancel
/// token. The underlying `StreamContext` stays in `ClientChannel.stream_contexts`
/// (it tracks "where to deliver" per active WebSocket); this struct
/// represents "Agent's per-turn push handle into that channel".
pub(crate) struct ClientTurnStream {
    reply_target: String,
    event_tx: mpsc::Sender<TurnEvent>,
    cancel: CancellationToken,
    status: crate::channels::StreamDelivery,
    finished: bool,
}

#[async_trait]
impl crate::channels::TurnStream for ClientTurnStream {
    async fn push(&mut self, event: TurnEvent) -> anyhow::Result<crate::channels::StreamDelivery> {
        if self.event_tx.send(event).await.is_err() {
            tracing::debug!(
                reply_target = %self.reply_target,
                "ClientTurnStream::push: client disconnected"
            );
            anyhow::bail!("client stream closed");
        }
        // WebSocket layer takes the bytes from the mpsc; we model that as
        // Visible. FinalDelivered happens at finish() when the consumer
        // has had a chance to drain.
        self.status = crate::channels::StreamDelivery::Visible;
        Ok(self.status)
    }

    fn status(&self) -> crate::channels::StreamDelivery {
        self.status
    }

    async fn finish(mut self: Box<Self>) -> crate::channels::StreamDelivery {
        // ClientChannel ack semantics: once we've handed the bytes to the
        // mpsc and the WS forwarder runs, the consumer has the data.
        // Treat that as FinalDelivered.
        self.finished = true;
        self.status = crate::channels::StreamDelivery::FinalDelivered;
        self.status
    }

    async fn abort(mut self: Box<Self>) {
        self.finished = true;
        self.cancel.cancel();
    }

    fn cancel_token(&self) -> Option<CancellationToken> {
        Some(self.cancel.clone())
    }
}

// Drop-based safety net (RFC §7.6.5(b)): if a TurnStream is dropped
// without finish/abort (panic, accidental field overwrite), cancel the
// transport so the consumer isn't left hanging.
impl Drop for ClientTurnStream {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel.cancel();
        }
    }
}

// ── Management API Router ───────────────────────────────────────────────────

/// Shared handles passed to every API request handler.
struct ApiContext<'a> {
    /// Session-manager scope key (channel:account:sender), stable across reconnects.
    user_id: &'a str,
    session_manager: &'a Arc<OnceLock<Arc<crate::agents::SessionManager>>>,
    tool_specs: &'a Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
    workspace_dir: &'a Arc<OnceLock<std::path::PathBuf>>,
    config_path: &'a Arc<OnceLock<std::path::PathBuf>>,
    skill_manager: &'a Arc<OnceLock<Arc<RwLock<crate::agents::SkillManager>>>>,
    provider_registry: &'a Arc<OnceLock<Arc<dyn crate::providers::ProviderRegistry>>>,
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
            match ctx.workspace_dir.get() {
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
            let content = params["content"].as_str().unwrap_or("");
            match ctx.workspace_dir.get() {
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
            match ctx.workspace_dir.get() {
                Some(dir) => {
                    let path = dir.join("memory").join(filename);
                    match std::fs::remove_file(&path) {
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
            match ctx.workspace_dir.get() {
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
