//! `Channel` trait impl for `WebSocketChannel` plus the per-turn
//! `WebSocketTurnStream` handle (`impl TurnStream` / `impl Drop`), extracted
//! verbatim from `client.rs` (RFC docs/websocket-client-split-rfc.md, batch 3:
//! pure move). Also hosts the `CLIENT_CAPS` static used by `capabilities()`.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex as SyncMutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agents::TurnEvent;
use crate::channels::message::{
    Channel, ChannelInboundMessage, ChannelOutboundMessage, OutboundSendResult,
};

use super::bus::{SessionOutputBus, bus_key_candidates};
use super::channel::WebSocketChannel;

static CLIENT_CAPS: crate::channels::message::ChannelCapabilities =
    crate::channels::message::ChannelCapabilities::client();

#[async_trait]
impl Channel for WebSocketChannel {
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
        // session_buses is keyed by the identity routing key (e.g.
        // "client:default:web-user:default"), but receiver.id may arrive as
        // the full rk, a bare channel-local tail ("web-user:default" —
        // friends.rs::notify_peer / the /link code push), or a legacy key
        // ("ws-3") only the resolver still maps. bus_key_candidates covers
        // all three forms; the first live bus wins.
        //
        // A total miss is an Err, not a silent no-op: callers like
        // notify_peer must be able to tell the user "channel unreachable"
        // instead of reporting "code sent" while nothing was delivered.
        let recipient = msg.receiver.id.clone();
        let candidates = bus_key_candidates(&self.user_resolver, &recipient);

        // Clone the Arc to the bus before any .await — parking_lot guards
        // are not Send and must not cross await points.
        let bus = {
            let buses = self.session_buses.read();
            let mut found = None;
            for key in &candidates {
                if let Some(b) = buses.get(key) {
                    tracing::debug!(
                        recipient = %recipient,
                        resolved_key = %key,
                        "send_message resolved bus key"
                    );
                    found = Some((Arc::clone(b), key.clone()));
                    break;
                }
            }
            match found {
                Some((b, key)) => (b, key),
                None => {
                    return Err(anyhow::anyhow!(
                        "recipient {} has no live client bus; not deliverable via client channel",
                        recipient
                    ));
                }
            }
        };
        let (bus, recipient) = (bus.0, bus.1);

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
                return Err(anyhow::anyhow!(
                    "recipient {} has no live client bus subscriber; files are not queued on disconnect",
                    recipient
                ));
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
                anyhow::anyhow!("listen() called more than once on WebSocketChannel")
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
        // Same address forms as send_message: reply_target is usually the
        // full identity rk, but cross-channel callers may pass a bare tail
        // or legacy key. Miss → None (caller falls back to non-streaming).
        let candidates = bus_key_candidates(&self.user_resolver, reply_target);
        let buses = self.session_buses.read();
        let bus = candidates
            .iter()
            .find_map(|key| buses.get(key))
            .map(Arc::clone)?;
        let mut bus_guard = bus.lock();
        // Fresh cancel token per turn
        bus_guard.cancel = CancellationToken::new();
        bus_guard.turn_active = true;
        Some(Box::new(WebSocketTurnStream {
            bus: Arc::clone(&bus),
            status: crate::channels::StreamDelivery::Pending,
            finished: false,
        }))
    }
}

/// Per-turn streaming handle for WebSocketChannel (RFC §7.6).
///
/// Holds a shared reference to the session's `SessionOutputBus`. `push`
/// writes through the bus — it **never fails** because the bus buffers
/// when no subscriber is attached. This is the core decoupling: Agent
/// runs to completion regardless of WS connection state.
pub(crate) struct WebSocketTurnStream {
    bus: Arc<SyncMutex<SessionOutputBus>>,
    status: crate::channels::StreamDelivery,
    finished: bool,
}

#[async_trait]
impl crate::channels::TurnStream for WebSocketTurnStream {
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
impl Drop for WebSocketTurnStream {
    fn drop(&mut self) {
        if !self.finished {
            self.bus.lock().cancel.cancel();
        }
    }
}
