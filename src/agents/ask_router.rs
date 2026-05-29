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
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::oneshot;

use crate::channels::ChannelMessage;

/// Manages outstanding `ask_user` calls. The API mirrors RFC v2 §三.B:
/// `wait_for_reply` (caller-facing) + `fulfill` (orchestrator-facing).
#[derive(Clone, Default)]
pub struct AskRouter {
    pending: Arc<DashMap<String, oneshot::Sender<ChannelMessage>>>,
}

impl AskRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending ask for `session_id` and wait up to `timeout`
    /// for the user's next ChannelMessage. RFC §三.B reference impl.
    ///
    /// If a previous pending ask exists for the same session, it is
    /// dropped (the older receiver future resolves with `RecvError`).
    /// On timeout, the pending slot is cleared so a stale sender doesn't
    /// linger.
    pub async fn wait_for_reply(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> anyhow::Result<ChannelMessage> {
        let (tx, rx) = oneshot::channel();
        if self.pending.insert(session_id.to_string(), tx).is_some() {
            tracing::warn!(
                session = %session_id,
                "replaced outstanding ask_user with a newer one"
            );
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(_)) => Err(anyhow::anyhow!("ask_user: receiver cancelled")),
            Err(_) => {
                self.pending.remove(session_id);
                Err(anyhow::anyhow!(
                    "ask_user: timed out after {}s",
                    timeout.as_secs()
                ))
            }
        }
    }

    /// Fulfill a pending ask with the user's reply. Returns true if
    /// there was an outstanding ask to fulfill, false otherwise.
    ///
    /// The orchestrator calls this before dispatching the message to
    /// `process_turn` — if `fulfill` returns true, the inbound message
    /// is consumed by the ask_user resolution and should NOT trigger a
    /// fresh turn.
    pub fn fulfill(&self, session_id: &str, reply: ChannelMessage) -> bool {
        match self.pending.remove(session_id) {
            Some((_, sender)) => {
                // Send result is Ok unless the receiver has been dropped
                // (the sub-agent task that asked has been cancelled). Either
                // way we treat this as "ask consumed" — the session has
                // moved on.
                let _ = sender.send(reply);
                true
            }
            None => false,
        }
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
        let router = r.clone();
        let waiter = tokio::spawn(async move {
            router.wait_for_reply("s1", Duration::from_secs(5)).await
        });
        // Let the waiter register before we fulfill.
        tokio::task::yield_now().await;
        assert!(r.fulfill("s1", msg("yes")));
        let reply = waiter.await.unwrap().unwrap();
        assert_eq!(reply.content, "yes");
    }

    #[tokio::test]
    async fn fulfill_missing_returns_false() {
        let r = AskRouter::new();
        assert!(!r.fulfill("nope", msg("x")));
    }

    #[tokio::test]
    async fn timeout_clears_pending_slot() {
        let r = AskRouter::new();
        let err = r
            .wait_for_reply("s", Duration::from_millis(10))
            .await
            .expect_err("should time out");
        assert!(err.to_string().contains("timed out"));
        // Fulfilling after timeout should report no pending ask.
        assert!(!r.fulfill("s", msg("late")));
    }
}
