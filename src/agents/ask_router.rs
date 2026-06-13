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
//! - **No artificial timeout.** A human's reply latency is unbounded, so the
//!   wait ends only on a real event (reply, supersession, or cancellation). A
//!   per-wait drop-guard (tagged by a generation counter) clears the slot on
//!   ANY exit, so a cancelled / dropped future never leaks a pending entry.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use tokio::sync::oneshot;

use crate::channels::ChannelInboundMessage;

/// One pending ask: a generation tag (to disambiguate replacements) + the
/// oneshot sender that wakes the waiting `ask_user`.
type Pending = (u64, oneshot::Sender<ChannelInboundMessage>);

/// Manages outstanding `ask_user` calls. The API mirrors RFC v2 §三.B:
/// `wait_for_reply` (caller-facing) + `fulfill` (orchestrator-facing).
#[derive(Clone, Default)]
pub struct AskRouter {
    pending: Arc<DashMap<String, Pending>>,
    /// Monotonic generation source. Each `wait_for_reply` tags its slot so the
    /// drop-guard only clears the slot it actually registered — never a newer
    /// ask that replaced it (the classic ABA hazard of the replacement rule).
    next_gen: Arc<AtomicU64>,
}

/// Clears a session's pending slot when the waiting future ends — by reply,
/// supersession, OR cancellation (parent turn interrupted, sub-agent dropped,
/// process shutting down). This is what makes cleanup reliable regardless of
/// how the wait terminates: the previous timeout-only cleanup leaked the slot
/// whenever the future was dropped instead of timing out. Removes the slot only
/// if its generation still matches, so it never evicts a newer ask.
struct PendingGuard {
    pending: Arc<DashMap<String, Pending>>,
    session_id: String,
    generation: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending
            .remove_if(&self.session_id, |_, (g, _)| *g == self.generation);
    }
}

impl AskRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending ask for `session_id` and wait — with NO artificial
    /// timeout — for the user's next ChannelInboundMessage. The wait ends only on a
    /// real event: the reply arrives (`fulfill`), a newer ask supersedes this
    /// one, or the future is cancelled (turn interrupted / shutdown). In every
    /// case the `PendingGuard` clears this session's slot on the way out.
    pub async fn wait_for_reply(&self, session_id: &str) -> anyhow::Result<ChannelInboundMessage> {
        let (tx, rx) = oneshot::channel();
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        if self
            .pending
            .insert(session_id.to_string(), (generation, tx))
            .is_some()
        {
            tracing::warn!(
                session = %session_id,
                "replaced outstanding ask_user with a newer one"
            );
        }
        // Held across the await: removes our slot on ANY exit (Ok / Err / cancel).
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            session_id: session_id.to_string(),
            generation,
        };
        match rx.await {
            Ok(msg) => Ok(msg),
            // Sender dropped: superseded by a newer ask, or the router was torn
            // down. Surface as a non-fatal error for the caller.
            Err(_) => Err(anyhow::anyhow!(
                "ask_user: superseded before a reply arrived"
            )),
        }
    }

    /// Fulfill a pending ask with the user's reply. Returns true if
    /// there was an outstanding ask to fulfill, false otherwise.
    ///
    /// The orchestrator calls this before dispatching the message to
    /// `process_turn` — if `fulfill` returns true, the inbound message
    /// is consumed by the ask_user resolution and should NOT trigger a
    /// fresh turn.
    pub fn fulfill(&self, session_id: &str, reply: ChannelInboundMessage) -> bool {
        match self.pending.remove(session_id) {
            Some((_, (_gen, sender))) => {
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

    fn msg(content: &str) -> ChannelInboundMessage {
        ChannelInboundMessage {
            id: "test".into(),
            sender: crate::channels::MessageSender::new("s"),
            receiver: crate::channels::MessageReceiver::new("rt"),
            content: crate::channels::ChannelMessageContent::text(content),
            timestamp: 0,
            interruption_scope_id: None,
        }
    }

    #[tokio::test]
    async fn fulfill_resolves_future() {
        let r = AskRouter::new();
        let router = r.clone();
        let waiter = tokio::spawn(async move { router.wait_for_reply("s1").await });
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

    /// The core leak fix: a cancelled (dropped) wait must clear its pending slot
    /// — previously only the timeout branch did, so a dropped future leaked it.
    #[tokio::test]
    async fn cancelled_wait_clears_pending_slot() {
        let r = AskRouter::new();
        let router = r.clone();
        let waiter = tokio::spawn(async move { router.wait_for_reply("s").await });
        tokio::task::yield_now().await; // let it register
        waiter.abort(); // cancel the wait without reply or timeout
        let _ = waiter.await; // join the aborted task so its Drop runs
        // The slot must be gone — nothing to fulfill.
        assert!(
            !r.fulfill("s", msg("late")),
            "cancelled wait must not leak a pending slot"
        );
    }

    /// A newer ask replaces the older one; when the OLD wait unwinds, its
    /// drop-guard must NOT evict the NEW ask's slot (generation guard).
    #[tokio::test]
    async fn supersede_does_not_evict_newer_ask() {
        let r = AskRouter::new();
        let r1 = r.clone();
        let old = tokio::spawn(async move { r1.wait_for_reply("s").await });
        tokio::task::yield_now().await;
        // Second ask supersedes the first (drops old sender → old wait errors).
        let r2 = r.clone();
        let new = tokio::spawn(async move { r2.wait_for_reply("s").await });
        tokio::task::yield_now().await;
        // The old wait has now returned Err and its guard ran; the new slot
        // must still be fulfillable.
        let _ = old.await;
        tokio::task::yield_now().await;
        assert!(
            r.fulfill("s", msg("answer")),
            "newer ask's slot must survive the old guard"
        );
        let reply = new.await.unwrap().unwrap();
        assert_eq!(reply.content, "answer");
    }
}
