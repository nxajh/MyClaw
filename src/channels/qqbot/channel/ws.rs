use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use super::super::flow::ReconnectManager;
use super::super::types::{GatewayPayload, SessionState, WsDisconnect};

use super::QQBotChannel;
use super::extract_quote_content;
use super::protocol::{
    OP_DISPATCH, OP_HEARTBEAT, OP_HEARTBEAT_ACK, OP_HELLO, OP_INVALID_SESSION, OP_RECONNECT,
    OP_RESUME,
};
use crate::channels::message::ChannelInboundMessage;

// ── WebSocket loop ────────────────────────────────────────────────────────────
impl QQBotChannel {
    /// Main WebSocket loop with auto-reconnect and incremental delay.
    ///
    /// Reconnection state (attempt counter, rapid-disconnect window) is
    /// delegated to [`ReconnectManager`].
    pub(super) async fn ws_loop(&self, tx: mpsc::Sender<ChannelInboundMessage>) {
        let mut mgr = ReconnectManager::new();

        loop {
            let result = self.ws_connect(&tx).await;

            match result {
                Ok(WsDisconnect::TryResume) => {
                    info!(account = %self.account_id, "QQ Bot WebSocket disconnected (resumable), reconnecting");
                    let delay = mgr.next_delay(&WsDisconnect::TryResume);
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::Clean) => {
                    warn!(account = %self.account_id, "QQ Bot WebSocket disconnected, reconnecting");
                    let delay = mgr.next_delay(&WsDisconnect::Clean);
                    // Clean disconnect clears session
                    *self.session.lock() = None;
                    info!(account = %self.account_id, delay_secs = delay.as_secs(), "reconnecting");
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::TokenExpired) => {
                    warn!(account = %self.account_id, "QQ Bot token expired, forcing refresh before reconnect");
                    if let Err(e) = self.token_manager.refresh().await {
                        error!(account = %self.account_id, err = %e, "token refresh failed");
                    }
                    *self.session.lock() = None;
                    let delay = mgr.next_delay(&WsDisconnect::TokenExpired);
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::Fatal) => {
                    error!(account = %self.account_id, "QQ Bot WebSocket fatal disconnect, stopping reconnect");
                    return;
                }
                Err(e) => {
                    error!(account = %self.account_id, err = %e, "QQ Bot WebSocket error, reconnecting");
                    let delay = mgr.next_delay(&WsDisconnect::Clean);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Connect to the WebSocket gateway and handle the session.
    ///
    /// Uses `tokio::select!` to multiplex heartbeat sending and message reading
    /// in a single task, avoiding the need to clone `SplitSink`.
    async fn ws_connect(
        &self,
        tx: &mpsc::Sender<ChannelInboundMessage>,
    ) -> anyhow::Result<WsDisconnect> {
        // 1. Get gateway URL.
        let ws_url = self.fetch_gateway_url().await?;
        info!(account = %self.account_id, url = %ws_url, "connecting to QQ Bot WebSocket gateway");

        // 2. Connect.
        let (ws_stream, _response) = connect_async(&ws_url)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {}", e))?;

        info!(account = %self.account_id, "QQ Bot WebSocket connected");
        let (mut write, mut read) = ws_stream.split();

        // 3. Wait for Hello (OpCode 10).
        let hello_msg = read
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("WebSocket closed before Hello"))?
            .map_err(|e| anyhow::anyhow!("WebSocket read error on Hello: {}", e))?;

        let hello_text = match hello_msg {
            Message::Text(t) => t,
            _ => return Err(anyhow::anyhow!("expected text Hello message")),
        };

        let hello: GatewayPayload = serde_json::from_str(&hello_text)
            .map_err(|e| anyhow::anyhow!("Hello parse error: {}", e))?;

        if hello.op != OP_HELLO {
            return Err(anyhow::anyhow!(
                "expected OpCode 10 (Hello), got {}",
                hello.op
            ));
        }

        let heartbeat_interval: u64 = hello.d["heartbeat_interval"].as_u64().unwrap_or(41250);

        info!(account = %self.account_id, heartbeat_interval_ms = heartbeat_interval, "received Hello");

        // 4. Send Identify or Resume.
        let token = self.token_manager.get_token().await?;
        let session = self.session.lock().clone();
        let init_payload = match session {
            Some(ref s) => {
                info!(account = %self.account_id, session_id = %s.session_id, seq = s.last_seq, "sending Resume");
                serde_json::json!({
                    "op": OP_RESUME,
                    "d": {
                        "token": format!("QQBot {}", token),
                        "session_id": s.session_id,
                        "seq": s.last_seq,
                    }
                })
                .to_string()
            }
            None => self.build_identify(&token),
        };
        write
            .send(Message::Text(init_payload.into()))
            .await
            .map_err(|e| anyhow::anyhow!("Identify/Resume send failed: {}", e))?;

        info!(account = %self.account_id, "QQ Bot Identify/Resume sent");

        // 5. Main loop: select between heartbeat tick and incoming messages.
        let mut heartbeat_ticker = tokio::time::interval(Duration::from_millis(heartbeat_interval));
        // Consume the first immediate tick.
        heartbeat_ticker.tick().await;

        loop {
            tokio::select! {
                // Heartbeat tick.
                _ = heartbeat_ticker.tick() => {
                    let seq = *self.last_seq.lock();
                    let payload = serde_json::json!({
                        "op": OP_HEARTBEAT,
                        "d": seq,
                    });
                    let text = serde_json::to_string(&payload).unwrap_or_default();
                    if let Err(e) = write.send(Message::Text(text.into())).await {
                        warn!(account = %self.account_id, err = %e, "heartbeat send failed, connection likely closed");
                        return Ok(WsDisconnect::TryResume);
                    }
                    debug!(account = %self.account_id, "heartbeat sent");
                }
                // Incoming WebSocket message.
                msg = read.next() => {
                    match msg {
                        Some(Ok(ws_msg)) => {
                            if let Some(disconnect) = self.handle_ws_message(ws_msg, tx).await {
                                return Ok(disconnect);
                            }
                        }
                        Some(Err(e)) => {
                            warn!(account = %self.account_id, err = %e, "WebSocket read error");
                            return Ok(WsDisconnect::Clean);
                        }
                        None => {
                            info!("WebSocket stream ended");
                            return Ok(WsDisconnect::Clean);
                        }
                    }
                }
            }
        }
    }

    /// Handle a single WebSocket message. Returns `Some(WsDisconnect)` if we should
    /// disconnect, `None` to continue processing.
    async fn handle_ws_message(
        &self,
        ws_msg: Message,
        tx: &mpsc::Sender<ChannelInboundMessage>,
    ) -> Option<WsDisconnect> {
        let text = match ws_msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                let code = frame.as_ref().map(|f| f.code.into()).unwrap_or(0u16);
                info!(account = %self.account_id, close_code = code, "WebSocket closed by server");
                return Some(match code {
                    // Token expired — refresh and reconnect
                    4004 => {
                        warn!(account = %self.account_id, "close 4004: token expired");
                        WsDisconnect::TokenExpired
                    }
                    // Session invalid — clear session, reconnect with Identify
                    4006 | 4007 | 4009 => {
                        warn!(account = %self.account_id, code, "close: session invalidated, clearing session");
                        *self.session.lock() = None;
                        *self.last_seq.lock() = None;
                        WsDisconnect::Clean
                    }
                    // Rate limited — reconnect normally (ws_loop handles delay via attempt counter)
                    4008 => {
                        warn!(account = %self.account_id, "close 4008: rate limited");
                        WsDisconnect::Clean
                    }
                    // Fatal — stop reconnecting
                    4914 | 4915 => {
                        error!(account = %self.account_id, code, "fatal close code");
                        WsDisconnect::Fatal
                    }
                    _ => WsDisconnect::Clean,
                });
            }
            Message::Ping(_) | Message::Pong(_) => return None,
            _ => return None,
        };

        let payload: GatewayPayload = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warn!(account = %self.account_id, err = %e, "failed to parse WebSocket payload");
                return None;
            }
        };

        // Update sequence number.
        if let Some(s) = payload.s {
            *self.last_seq.lock() = Some(s);
        }

        match payload.op {
            OP_DISPATCH => {
                if let Some(ref event_type) = payload.t {
                    // Internal events first
                    match event_type.as_str() {
                        "READY" => {
                            if let Some(session_id) =
                                payload.d.get("session_id").and_then(|v| v.as_str())
                            {
                                info!(
                                    account = %self.account_id,
                                    session_id = session_id,
                                    "READY received, session established"
                                );
                                *self.session.lock() = Some(SessionState {
                                    session_id: session_id.to_string(),
                                    last_seq: payload.s.unwrap_or(0),
                                });
                            }
                        }
                        "RESUMED" => {
                            info!(account = %self.account_id, "RESUMED received, session restored");
                        }
                        _ => {}
                    }
                    // Feature 2: Record group history for ALL group events
                    // (before auth/filtering) so even rejected messages are
                    // captured as context for future @-mentions.
                    if event_type.contains("GROUP") {
                        if let (Some(group_openid), Some(sender), Some(content)) = (
                            payload.d.get("group_openid").and_then(|v| v.as_str()),
                            payload
                                .d
                                .get("author")
                                .and_then(|a| a.get("member_openid"))
                                .and_then(|v| v.as_str()),
                            payload
                                .d
                                .get("content")
                                .and_then(|v| v.as_str())
                                .map(str::trim),
                        ) {
                            self.record_group_history(group_openid, sender, content);
                        }
                    }
                    // User messages
                    if let Some(mut channel_msg) = self.handle_dispatch(event_type, &payload.d) {
                        // Feature 1: Quote message resolution — when the user
                        // replied to a message (message_type=103), prepend the
                        // quoted content so the model has full context.
                        if let Some(quoted) = extract_quote_content(&payload.d) {
                            channel_msg.content.text = format!(
                                "[Quoted message begins]\n{}\n[Quoted message ends]\n[Current message]\n{}",
                                quoted, channel_msg.content.text
                            );
                        }

                        // Voice/audio: use QQ's native ASR text when present, else
                        // attach downloaded bytes for the auxiliary STT model.
                        self.ingest_voice_attachments(&payload.d, &mut channel_msg)
                            .await;

                        // Image: download and save to temp file so vision models
                        // can see the image without relying on proxy URL fetching.
                        self.ingest_image_attachments(&payload.d, &mut channel_msg)
                            .await;

                        // Video / file: download and save to temp file.
                        self.ingest_video_file_attachments(&payload.d, &mut channel_msg)
                            .await;

                        match tx.try_send(channel_msg.clone()) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                error!(
                                    account = %self.account_id,
                                    "QQBot inbound queue full, dropping message"
                                );
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                warn!(account = %self.account_id, "channel receiver dropped, stopping listen");
                                return Some(WsDisconnect::Clean);
                            }
                        }
                        // Start typing keep-alive for C2C messages.
                        self.start_internal_typing(&channel_msg.receiver.id);
                    }
                }
            }
            OP_HEARTBEAT_ACK => {
                debug!(account = %self.account_id, "heartbeat ACK received");
            }
            OP_RECONNECT => {
                warn!(account = %self.account_id, "server requested reconnect");
                return Some(WsDisconnect::TryResume);
            }
            OP_INVALID_SESSION => {
                warn!(account = %self.account_id, "invalid session (OpCode 9), clearing session for fresh identify");
                *self.last_seq.lock() = None;
                *self.session.lock() = None;
                return Some(WsDisconnect::Clean);
            }
            _ => {
                debug!(op = payload.op, "unknown opcode");
            }
        }

        None
    }
}
