//! Session output bus, extracted verbatim from `client.rs` (RFC
//! docs/websocket-client-split-rfc.md, batch 1: pure move).
//!
//! [`Subscriber`], [`SessionOutputBus`] and [`bus_key_candidates`] keep their
//! original bodies; only `pub(super)` was added where `client.rs` references
//! the item.

use std::sync::Arc;
use std::sync::OnceLock;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agents::TurnEvent;

// ── Session Output Bus ──────────────────────────────────────────────────────

pub(super) struct Subscriber {
    conn_id: String,
    sender: mpsc::Sender<String>,
}

/// Per-session output bus. Decouples turn execution from WebSocket connection
/// lifetime. Survives disconnects; buffers events and messages for replay on
/// reconnect. This is the single source of truth for output delivery — the WS
/// connection is just one (replaceable) subscriber.
pub(super) struct SessionOutputBus {
    /// TurnEvent ring buffer — only accumulates while no subscriber is attached.
    /// Drained on subscribe (replay). Capped at `event_buffer_capacity`.
    event_buffer: std::collections::VecDeque<TurnEvent>,
    event_buffer_capacity: usize,
    /// Non-streaming messages (send_message output) queued while no subscriber.
    message_queue: std::collections::VecDeque<String>,
    /// Active WS subscribers: raw text mpsc → outgoing forwarder → ws_sink.
    /// Multiple connections of the same identity may subscribe simultaneously.
    pub(super) subscribers: Vec<Subscriber>,
    /// Active session_id for event JSON injection (frontend filtering).
    session_id: String,
    /// Current turn's cancel token. Recreated each turn by create_stream.
    pub(super) cancel: CancellationToken,
    /// Whether a turn is in progress.
    pub(super) turn_active: bool,
}

impl SessionOutputBus {
    pub(super) fn new() -> Self {
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
    pub(super) fn subscribe(
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
    pub(super) fn drain_messages(&mut self) -> Vec<String> {
        self.message_queue.drain(..).collect()
    }

    /// Drain buffered TurnEvents for replay (clears the buffer).
    pub(super) fn drain_events(&mut self) -> Vec<TurnEvent> {
        self.event_buffer.drain(..).collect()
    }

    /// Detach one subscriber (on WS disconnect). Bus survives for replay.
    pub(super) fn detach(&mut self, conn_id: &str) {
        self.subscribers.retain(|s| s.conn_id != conn_id);
    }

    /// Push a TurnEvent. If subscribers are online, forwards directly via
    /// try_send (non-blocking); failed subscribers are dropped. If offline,
    /// buffers for replay. **Never fails** — this is the core decoupling
    /// invariant.
    pub(super) fn push_event(&mut self, event: TurnEvent) {
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
    pub(super) fn push_message(&mut self, json: String) {
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

/// Candidate session-bus keys for a receiver address, priority-ordered and
/// deduped. `WebSocketChannel` buses are keyed by the identity routing key
/// (`client:default:web-user:{user}`), but `receiver.id` arrives in three
/// forms depending on the caller:
///
/// 1. the full rk itself (orchestrator replies — `session_key_clone`),
/// 2. a bare channel-local tail such as `web-user:default`
///    (`friends.rs::notify_peer` splits the peer rk and passes the tail), or
/// 3. a legacy key like `ws-3` that only the user resolver still knows about
///    (mapped to the same uid as the live identity key).
///
/// Candidate order: exact match → normalized `client:default:{recipient}`
/// (skipped when already prefixed) → every `client:default:*` key the
/// resolver folds into the same uid (cross-channel keys like `telegram:*`
/// belong to other channels and are skipped). First candidate that hits a
/// live bus wins.
pub(super) fn bus_key_candidates(
    user_resolver: &Arc<OnceLock<Arc<crate::agents::UserResolver>>>,
    recipient: &str,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let push = |key: String, out: &mut Vec<String>| {
        if !out.contains(&key) {
            out.push(key);
        }
    };

    // 1. Exact form as given.
    push(recipient.to_string(), &mut candidates);

    // 2. Normalized identity rk (only when the id is not already a full
    //    client rk — `web-user:default` and `ws-3` both need the prefix,
    //    `client:default:web-user:default` does not).
    let rk = if recipient.starts_with("client:") {
        recipient.to_string()
    } else {
        format!("client:default:{recipient}")
    };
    push(rk.clone(), &mut candidates);

    // 3. Resolver: full rk → uid → all of that uid's routing keys, keeping
    //    only keys that could name a bus in *this* channel.
    if let Some(resolver) = user_resolver.get() {
        let uid = resolver.resolve(&rk);
        for key in resolver.routing_keys_for(&uid) {
            if key.starts_with("client:default:") {
                push(key, &mut candidates);
            }
        }
    }

    candidates
}
