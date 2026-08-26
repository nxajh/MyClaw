//! Flow control and state management components for QQBotChannel.
//!
//! Includes ReconnectManager, ReplyLimiter, RateLimiter, and DeliverDebouncer.

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crate::channels::OutboundSendResult;
use super::types::WsDisconnect;

/// Reconnect delay schedule (seconds).
pub(crate) const RECONNECT_DELAYS: &[u64] = &[1, 2, 5, 10, 30, 60];
/// Maximum rapid reconnects before backing off.
pub(crate) const RAPID_RECONNECT_LIMIT: usize = 3;
pub(crate) const RAPID_RECONNECT_WINDOW_SECS: u64 = 5;

// ── Reconnect state machine ──────────────────────────────────────────────────

/// Manages WebSocket reconnection backoff state, extracted from `ws_loop`
/// for testability.
///
/// Tracks the number of consecutive rapid disconnects and computes the
/// appropriate sleep duration before the next attempt based on the disconnect
/// type.
pub(crate) struct ReconnectManager {
    attempt: usize,
    last_disconnect: std::time::Instant,
}

impl ReconnectManager {
    pub(crate) fn new() -> Self {
        Self {
            attempt: 0,
            last_disconnect: std::time::Instant::now(),
        }
    }

    /// Returns the delay before the next reconnect attempt based on disconnect
    /// type.
    ///
    /// - `TryResume`: resets attempt counter, returns 1 s (fast retry).
    /// - `Clean`: increments attempt on rapid disconnect, uses indexed backoff
    ///   schedule (caps at 60 s after `RAPID_RECONNECT_LIMIT` rapid failures).
    /// - `TokenExpired`: returns 3 s.
    /// - `Fatal`: returns 0 s (caller stops reconnecting).
    pub(crate) fn next_delay(&mut self, disconnect: &WsDisconnect) -> Duration {
        let now = std::time::Instant::now();
        let rapid =
            now.duration_since(self.last_disconnect).as_secs() < RAPID_RECONNECT_WINDOW_SECS;
        self.last_disconnect = now;

        match disconnect {
            WsDisconnect::TryResume => {
                self.attempt = 0;
                Duration::from_secs(1)
            }
            WsDisconnect::Clean => {
                if rapid {
                    self.attempt += 1;
                } else {
                    self.attempt = 0;
                }
                if self.attempt >= RAPID_RECONNECT_LIMIT {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(
                        RECONNECT_DELAYS[self.attempt.min(RECONNECT_DELAYS.len() - 1)],
                    )
                }
            }
            WsDisconnect::TokenExpired => Duration::from_secs(3),
            WsDisconnect::Fatal => Duration::from_secs(0),
        }
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }
}

// ── Reply limiter (passive reply cap) ─────────────────────────────────────────

/// Tracks passive reply counts per message_id to avoid hitting QQ's limit
/// (~4 replies/msg_id within 1 hour).
pub(crate) struct ReplyLimiter {
    /// msg_id → (reply_count, first_seen_ms)
    entries: std::collections::HashMap<String, (u32, u128)>,
    /// Max replies per msg_id
    limit: u32,
    /// TTL in ms (1 hour)
    ttl_ms: u128,
}

impl ReplyLimiter {
    pub(crate) fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            limit: 4,
            ttl_ms: 3_600_000,
        }
    }

    /// Check if we can still reply passively to this msg_id.
    /// Returns false when limit exceeded or TTL expired.
    pub(crate) fn check_and_record(&mut self, msg_id: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        // Periodic cleanup: remove expired entries
        self.entries
            .retain(|_, (_, first_seen)| now - *first_seen < self.ttl_ms);

        match self.entries.get_mut(msg_id) {
            Some((count, _)) => {
                if *count >= self.limit {
                    return false;
                }
                *count += 1;
                true
            }
            None => {
                // LRU eviction if too many entries
                if self.entries.len() > 10_000 {
                    if let Some(oldest_key) = self
                        .entries
                        .iter()
                        .min_by_key(|(_, (_, first))| *first)
                        .map(|(k, _)| k.clone())
                    {
                        self.entries.remove(&oldest_key);
                    }
                }
                self.entries.insert(msg_id.to_string(), (1, now));
                true
            }
        }
    }
}


// ── Rate limiter ──────────────────────────────────────────────────────────────

/// Simple token-bucket rate limiter with per-sender and global tracking.
pub(crate) struct RateLimiter {
    sender_buckets: std::collections::HashMap<String, (u32, u128)>,
    global_count: u32,
    global_window_start: u128,
    sender_limit: u32,
    global_limit: u32,
}

impl RateLimiter {
    pub(crate) fn new() -> Self {
        Self {
            sender_buckets: std::collections::HashMap::new(),
            global_count: 0,
            global_window_start: 0,
            sender_limit: 30,
            global_limit: 300,
        }
    }

    pub(crate) fn check(&mut self, sender_id: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let window_ms: u128 = 60_000;
        if now - self.global_window_start > window_ms {
            self.global_count = 0;
            self.global_window_start = now;
        }
        if self.global_count >= self.global_limit {
            return false;
        }
        let sender_key = sender_id.to_string();
        match self.sender_buckets.get_mut(&sender_key) {
            Some((count, window_start)) => {
                if now - *window_start > window_ms {
                    *count = 0;
                    *window_start = now;
                }
                if *count >= self.sender_limit {
                    return false;
                }
                *count += 1;
            }
            None => {
                if self.sender_buckets.len() > 5000 {
                    self.sender_buckets.retain(|_, (_, ws)| now - *ws < window_ms);
                }
                self.sender_buckets.insert(sender_key, (1, now));
            }
        }
        self.global_count += 1;
        true
    }
}

// ── Deliver debouncer ─────────────────────────────────────────────────────────

/// A pending debounced delivery for one recipient.
pub(crate) struct PendingDeliver {
    pub(crate) texts: Vec<String>,
    pub(crate) msg_id: String,
    pub(crate) waiters: Vec<
        tokio::sync::oneshot::Sender<anyhow::Result<crate::channels::OutboundSendResult>>,
    >,
}

/// Outbound debounce merge: coalesces rapid text-only sends to the same
/// recipient within a window into a single message (avoids message bombing).
///
/// Modelled after the official plugin's `DeliverDebouncer`. The first send to a
/// recipient within an idle window drives a flush task; subsequent sends within
/// the window append to the buffer and await the shared flush result.
pub(crate) struct DeliverDebouncer {
    pub(crate) window_ms: u64,
    pub(crate) separator: String,
    pending: parking_lot::Mutex<std::collections::HashMap<String, PendingDeliver>>,
}

impl DeliverDebouncer {
    pub(crate) fn new(window_ms: u64, separator: String) -> Self {
        Self {
            window_ms,
            separator,
            pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.window_ms > 0
    }

    /// Buffer a text. Returns a receiver for the eventual send result and
    /// whether this caller is the first in the window (and must drive the flush).
    pub(crate) fn enqueue(
        &self,
        recipient: &str,
        text: String,
        msg_id: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<anyhow::Result<crate::channels::OutboundSendResult>>,
        bool,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut pending = self.pending.lock();
        let is_first = !pending.contains_key(recipient);
        let entry = pending
            .entry(recipient.to_string())
            .or_insert_with(|| PendingDeliver {
                texts: Vec::new(),
                msg_id: String::new(),
                waiters: Vec::new(),
            });
        entry.texts.push(text);
        if !msg_id.is_empty() {
            entry.msg_id = msg_id.to_string();
        }
        entry.waiters.push(tx);
        (rx, is_first)
    }

    /// Remove and return the pending entry for a recipient (flush driver only).
    pub(crate) fn take(&self, recipient: &str) -> Option<PendingDeliver> {
        self.pending.lock().remove(recipient)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn reconnect_manager_try_resume_resets_attempt() {
        let mut mgr = ReconnectManager::new();
        // Simulate a few rapid Clean disconnects to ramp up attempt.
        mgr.attempt = 2;
        let delay = mgr.next_delay(&WsDisconnect::TryResume);
        assert_eq!(delay, Duration::from_secs(1));
        assert_eq!(mgr.attempt, 0);
    }

    #[test]
    fn reconnect_manager_clean_first_rapid() {
        let mut mgr = ReconnectManager::new();
        // Immediately after construction → rapid.
        let delay = mgr.next_delay(&WsDisconnect::Clean);
        assert_eq!(delay, Duration::from_secs(RECONNECT_DELAYS[1]));
        assert_eq!(mgr.attempt, 1);
    }

    #[test]
    fn reconnect_manager_caps_at_60s() {
        let mut mgr = ReconnectManager::new();
        // Simulate RAPID_RECONNECT_LIMIT rapid disconnects.
        for _ in 0..RAPID_RECONNECT_LIMIT {
            mgr.next_delay(&WsDisconnect::Clean);
        }
        let delay = mgr.next_delay(&WsDisconnect::Clean);
        assert_eq!(delay, Duration::from_secs(60));
    }

    #[test]
    fn reconnect_manager_token_expired_fixed_delay() {
        let mut mgr = ReconnectManager::new();
        let delay = mgr.next_delay(&WsDisconnect::TokenExpired);
        assert_eq!(delay, Duration::from_secs(3));
    }

    #[test]
    fn reconnect_manager_reset() {
        let mut mgr = ReconnectManager::new();
        mgr.attempt = 5;
        mgr.reset();
        assert_eq!(mgr.attempt, 0);
    }

    // ── Feature 1: Quote message extraction ──────────────────────────────────

    #[test]
    fn reply_limiter_allows_then_blocks() {
        let mut rl = ReplyLimiter::new();
        // First 4 replies should be allowed.
        for i in 1..=4 {
            assert!(
                rl.check_and_record("msg-001"),
                "reply #{i} should be allowed"
            );
        }
        // 5th reply should be blocked.
        assert!(
            !rl.check_and_record("msg-001"),
            "reply #5 should be blocked"
        );
        // A different msg_id should still work.
        assert!(
            rl.check_and_record("msg-002"),
            "different msg_id should be allowed"
        );
    }

    #[test]
    fn debouncer_buffers_and_merges_texts() {
        let d = DeliverDebouncer::new(100, "\n---\n".to_string());
        assert!(d.enabled());

        let (_, first1) = d.enqueue("c2c:u1", "hello".to_string(), "m1");
        assert!(first1, "first enqueue should drive the flush");
        let (_, first2) = d.enqueue("c2c:u1", "world".to_string(), "m1");
        assert!(!first2, "second enqueue within window should not drive");

        let entry = d.take("c2c:u1").expect("pending entry expected");
        assert_eq!(entry.texts.len(), 2);
        assert_eq!(entry.texts.join("\n---\n"), "hello\n---\nworld");
        assert_eq!(entry.msg_id, "m1");
        assert_eq!(entry.waiters.len(), 2);
    }

    #[test]
    fn debouncer_separate_recipients_are_independent() {
        let d = DeliverDebouncer::new(100, "\n".to_string());
        let (_, f1) = d.enqueue("c2c:a", "one".to_string(), "");
        let (_, f2) = d.enqueue("c2c:b", "two".to_string(), "");
        assert!(f1 && f2, "distinct recipients each drive their own flush");
        assert!(d.take("c2c:a").is_some());
        assert!(d.take("c2c:b").is_some());
    }

    #[test]
    fn debouncer_disabled_when_window_is_zero() {
        let d = DeliverDebouncer::new(0, "\n".to_string());
        assert!(!d.enabled());
    }

}
