//! AskRouter — pending `ask_user` replies indexed by session_id.
//!
//! RFC v2 §三.B: when an Agent calls `ask_user(question)`, the orchestrator
//! sends the question through the channel and stashes a oneshot sender so
//! the next inbound message from that session resolves the future. AskRouter
//! is the table that holds those pending senders.
//!
//! Key design points:
//! - Indexed by **session_id**, not routing_key. A sub-agent running on a
//!   sub-session needs to receive its parent's incoming messages until the
//!   ask is fulfilled; routing_key alone can't disambiguate.
//! - One pending ask per session. If a second `ask_user` fires before the
//!   first is answered, the first one is dropped (with a warning) and the
//!   second takes its place — UI conventions assume the latest question wins.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::oneshot;

use crate::channels::ChannelMessage;

/// Per-pending-ask state stored in the router.
struct Pending {
    /// Sends the full reply ChannelMessage so AskUserTool can read the
    /// answer content + any image attachments the user included. RFC
    /// §三.B "AskRouter.wait_for_reply → 返回 ChannelMessage".
    sender: oneshot::Sender<ChannelMessage>,
    /// reply_target captured at ask time — used so the orchestrator sends
    /// the question to the same channel + thread, not the latest one.
    reply_target: String,
}

/// Manages outstanding `ask_user` calls.
#[derive(Clone, Default)]
pub struct AskRouter {
    pending: Arc<DashMap<String, Pending>>,
}

impl AskRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending ask. Returns the receiver future that resolves with
    /// the user's eventual answer.
    ///
    /// If a previous pending ask exists for the same session, it is dropped
    /// (sending None to its receiver) so this new ask replaces it.
    pub fn register(
        &self,
        session_id: &str,
        reply_target: String,
    ) -> oneshot::Receiver<ChannelMessage> {
        let (tx, rx) = oneshot::channel();
        let pending = Pending {
            sender: tx,
            reply_target,
        };
        if let Some(_old) = self.pending.insert(session_id.to_string(), pending) {
            tracing::warn!(
                session = %session_id,
                "replaced outstanding ask_user with a newer one"
            );
        }
        rx
    }

    /// Fulfill a pending ask with the user's reply. Returns true if there
    /// was an outstanding ask to fulfill, false otherwise.
    ///
    /// E29 orchestrator calls this before dispatching the message to
    /// `process_turn` — if `fulfill` returns true, the inbound message is
    /// consumed by the ask_user resolution and should NOT trigger a fresh
    /// turn.
    pub fn fulfill(&self, session_id: &str, reply: ChannelMessage) -> bool {
        match self.pending.remove(session_id) {
            Some((_, Pending { sender, .. })) => {
                // Send result is Ok unless the receiver has been dropped (the
                // sub-agent task that asked has been cancelled). Either way
                // we treat this as "ask consumed" because the session has
                // moved on.
                let _ = sender.send(reply);
                true
            }
            None => false,
        }
    }

    /// Read the reply_target for an outstanding ask without resolving it.
    /// Used when the orchestrator wants to know where to send the question.
    pub fn pending_reply_target(&self, session_id: &str) -> Option<String> {
        self.pending.get(session_id).map(|p| p.reply_target.clone())
    }

    /// True if a session has an outstanding ask.
    pub fn has_pending(&self, session_id: &str) -> bool {
        self.pending.contains_key(session_id)
    }

    /// Cancel a pending ask without delivering an answer. The asking
    /// future receives a `RecvError`. Used by the /reset slash command and
    /// by session deletion.
    pub fn cancel(&self, session_id: &str) {
        self.pending.remove(session_id);
    }

    /// Count of outstanding asks (diagnostics).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(content: &str) -> ChannelMessage {
        ChannelMessage {
            id: "test".into(),
            sender: "s".into(),
            reply_target: "rt".into(),
            content: content.into(),
            timestamp: 0,
            thread_ts: None,
            interruption_scope_id: None,
            attachments: Vec::new(),
            image_urls: None,
            image_base64: None,
        }
    }

    #[tokio::test]
    async fn fulfill_resolves_future() {
        let r = AskRouter::new();
        let rx = r.register("s1", "tg:42".into());
        assert!(r.fulfill("s1", msg("yes")));
        let reply = rx.await.unwrap();
        assert_eq!(reply.content, "yes");
        assert!(!r.has_pending("s1"));
    }

    #[tokio::test]
    async fn fulfill_missing_returns_false() {
        let r = AskRouter::new();
        assert!(!r.fulfill("nope", msg("x")));
    }

    #[tokio::test]
    async fn replacing_pending_drops_old() {
        let r = AskRouter::new();
        let rx1 = r.register("s", "rt".into());
        let _rx2 = r.register("s", "rt".into());
        // rx1's sender was dropped when rx2 was registered
        assert!(rx1.await.is_err());
    }

    #[test]
    fn pending_reply_target_roundtrip() {
        let r = AskRouter::new();
        let _rx = r.register("s", "telegram:default:42".into());
        assert_eq!(
            r.pending_reply_target("s"),
            Some("telegram:default:42".to_string())
        );
        assert_eq!(r.pending_count(), 1);
    }
}
