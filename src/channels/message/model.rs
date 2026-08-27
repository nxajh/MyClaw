use std::sync::{Arc, Mutex};

/// Dedup state for a channel adapter (in-memory).
///
/// Uses a bounded FIFO ring: when `seen` reaches `capacity`, the oldest
/// half is evicted. This caps memory at O(capacity) regardless of how
/// long the daemon runs. Telegram update_ids are monotonic so eviction
/// never causes false negatives in practice — by the time an id is
/// evicted, Telegram's never going to redeliver an update from millions
/// of messages ago.
#[derive(Clone)]
pub struct DedupState {
    inner: Arc<Mutex<DedupInner>>,
    capacity: usize,
}

struct DedupInner {
    /// Set membership for O(1) lookup.
    seen: std::collections::HashSet<String>,
    /// Insertion order for FIFO eviction.
    order: std::collections::VecDeque<String>,
}

/// Cap memory at ~50K entries. At 32 bytes/entry average that's ~1.5 MB
/// per channel — well below noise floor. High-volume Telegram bots
/// processing 100 msg/sec keep ~8 minutes of history; recovery /
/// replay windows are far shorter than that.
const DEFAULT_DEDUP_CAPACITY: usize = 50_000;

impl Default for DedupState {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_DEDUP_CAPACITY)
    }
}

impl DedupState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DedupInner {
                seen: std::collections::HashSet::with_capacity(capacity),
                order: std::collections::VecDeque::with_capacity(capacity),
            })),
            capacity,
        }
    }

    /// Check if an update ID has been seen, and record it if not.
    /// Returns true if the ID was already seen (should skip), false if new.
    pub fn check_and_record(&self, id: &str) -> bool {
        // Poison-recover: the critical section never panics, and a poisoned
        // dedup cache must not cascade-crash the inbound path of a daemon.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.seen.contains(id) {
            return true;
        }
        let owned = id.to_string();
        inner.seen.insert(owned.clone());
        inner.order.push_back(owned);
        // Evict the oldest half when we hit capacity. Half-and-clear is
        // amortized O(1) per insert and avoids per-insert eviction churn.
        if inner.order.len() > self.capacity {
            let drop_n = self.capacity / 2;
            for _ in 0..drop_n {
                if let Some(old) = inner.order.pop_front() {
                    inner.seen.remove(&old);
                }
            }
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap().seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_state_basic_dedup() {
        let d = DedupState::new();
        assert!(!d.check_and_record("a")); // first sight → false
        assert!(d.check_and_record("a")); // duplicate → true
        assert!(!d.check_and_record("b"));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn dedup_state_bounded_eviction() {
        // capacity=10: insert 15 distinct ids. After 11th insert, evict
        // the oldest 5 (ids 0..5). Remaining 6 + 4 more inserts = 10.
        let d = DedupState::with_capacity(10);
        for i in 0..15 {
            assert!(!d.check_and_record(&format!("id-{i}")));
        }
        assert!(d.len() <= 10, "len {} must not exceed capacity 10", d.len());

        // id-0 was the oldest, should have been evicted — re-insert
        // returns false (i.e. treated as new, not duplicate).
        assert!(!d.check_and_record("id-0"), "id-0 should have been evicted");
    }

    #[test]
    fn dedup_state_recent_ids_still_dedup() {
        let d = DedupState::with_capacity(100);
        for i in 0..150 {
            assert!(!d.check_and_record(&format!("id-{i}")));
        }
        // Re-record the most recent id — must still be deduped
        assert!(d.check_and_record("id-149"));
    }
}
