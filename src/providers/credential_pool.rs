//! Persistent multi-credential pool for same-provider failover.
//!
//! When a single API key hits a rate limit or billing exhaustion, the pool
//! rotates to the next available key instead of failing over to a different
//! provider (which may have higher cost or lower quality).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::providers::FailoverReason;

/// A single credential entry in the pool.
///
/// issue #197: quota exhaustion is scoped per `(key, scope)`, not per key
/// alone. A single provider config can share one pool of keys across
/// multiple models (`daemon/builder.rs` builds one `CredentialPool` per
/// provider block and attaches a clone to every model under it), and on
/// some providers a key's quota for one model is entirely independent of
/// its quota for another. Recording exhaustion per key only meant a
/// quota error on model A wrongly cooled that key down for every other
/// model sharing the pool. `scope` is typically a chat model_id; callers
/// with no per-model quota concept (e.g. search providers) pass `""`,
/// which reduces to the old per-key-only behavior since every call for
/// that pool then agrees on the same single scope.
#[derive(Debug, Clone)]
pub struct CredentialEntry {
    pub key: String,
    pub exhausted_until: HashMap<String, Instant>,
    pub last_used: Option<Instant>,
    pub use_count: u64,
}

/// Strategy for selecting the next credential from the pool.
/// (Canonical definition: `crate::api::capability::RotationStrategy`, moved in #151 Phase 3c.)
pub use crate::api::capability::RotationStrategy;

/// Multi-credential pool with rotation and cooldown management.
pub struct CredentialPool {
    entries: Vec<CredentialEntry>,
    strategy: RotationStrategy,
    provider_name: String,
    /// Index for round-robin.
    round_robin_idx: usize,
}

impl CredentialPool {
    /// Create a new pool from a list of API keys.
    pub fn new(
        provider_name: impl Into<String>,
        keys: Vec<String>,
        strategy: RotationStrategy,
    ) -> Self {
        let entries = keys
            .into_iter()
            .map(|key| CredentialEntry {
                key,
                exhausted_until: HashMap::new(),
                last_used: None,
                use_count: 0,
            })
            .collect();
        Self {
            entries,
            strategy,
            provider_name: provider_name.into(),
            round_robin_idx: 0,
        }
    }

    /// Number of credentials in the pool.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the pool has no credentials.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop expired cooldown entries for `scope` across all keys.
    fn refresh(&mut self, scope: &str) {
        let now = Instant::now();
        for entry in &mut self.entries {
            if let Some(&until) = entry.exhausted_until.get(scope) {
                if now >= until {
                    entry.exhausted_until.remove(scope);
                    tracing::info!(
                        provider = %self.provider_name,
                        key_prefix = %Self::mask_key(&entry.key),
                        scope,
                        "credential cooldown expired, restored to active"
                    );
                }
            }
        }
    }

    /// Get the next available credential key for `scope`.
    /// Returns None if every key is currently exhausted for this scope.
    pub fn next_credential(&mut self, scope: &str) -> Option<&str> {
        self.refresh(scope);

        let active_indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.exhausted_until.contains_key(scope))
            .map(|(i, _)| i)
            .collect();

        if active_indices.is_empty() {
            return None;
        }

        let selected_idx = match self.strategy {
            RotationStrategy::FillFirst => active_indices[0],
            RotationStrategy::RoundRobin => {
                let idx = self.round_robin_idx % active_indices.len();
                self.round_robin_idx = self.round_robin_idx.wrapping_add(1);
                active_indices[idx]
            }
            RotationStrategy::Random => {
                let idx = rand::random_range(0..active_indices.len());
                active_indices[idx]
            }
            RotationStrategy::LeastUsed => active_indices
                .into_iter()
                .min_by_key(|i| self.entries[*i].use_count)
                .unwrap_or(0),
        };

        let entry = &mut self.entries[selected_idx];
        entry.last_used = Some(Instant::now());
        entry.use_count += 1;
        Some(&entry.key)
    }

    /// Mark `key` exhausted for `scope` until `duration` from now.
    ///
    /// `duration` comes from the caller's already-classified error
    /// (`ClassifiedError::cooldown_duration()`) rather than a second,
    /// independently-maintained reason→duration table here — issue #197
    /// found the old local table (`RateLimit` = 1h) silently disagreeing
    /// with `error_class.rs`'s (`RateLimit` default = 60s). `reason` is
    /// kept only for logging.
    pub fn mark_exhausted(
        &mut self,
        key: &str,
        scope: &str,
        reason: &FailoverReason,
        duration: Duration,
    ) {
        let now = Instant::now();

        for entry in &mut self.entries {
            if entry.key == key {
                entry.exhausted_until.insert(scope.to_string(), now + duration);
                tracing::warn!(
                    provider = %self.provider_name,
                    key_prefix = %Self::mask_key(key),
                    scope,
                    reason = ?reason,
                    cooldown_secs = duration.as_secs(),
                    "credential marked exhausted"
                );
                break;
            }
        }
    }

    /// Time until the soonest key currently exhausted for `scope` recovers.
    /// `None` if no key is exhausted for this scope right now (including
    /// when the pool has never seen an exhaustion for it).
    pub fn soonest_recovery(&self, scope: &str) -> Option<Duration> {
        let now = Instant::now();
        self.entries
            .iter()
            .filter_map(|e| e.exhausted_until.get(scope))
            .filter(|&&until| until > now)
            .map(|&until| until - now)
            .min()
    }

    /// Snapshot of current pool state for `scope`, for diagnostics.
    pub fn snapshot(&self, scope: &str) -> Vec<(String, Option<Duration>)> {
        let now = Instant::now();
        self.entries
            .iter()
            .map(|e| {
                let remaining = e.exhausted_until.get(scope).map(|&u| {
                    if u > now {
                        u - now
                    } else {
                        Duration::ZERO
                    }
                });
                (Self::mask_key(&e.key), remaining)
            })
            .collect()
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    fn mask_key(key: &str) -> String {
        if key.len() <= 8 {
            "***".to_string()
        } else {
            format!("{}...{}", &key[..4], &key[key.len() - 4..])
        }
    }
}

/// Shared, mutable API key that allows a credential pool to rotate keys at runtime.
///
/// All provider protocol clients that support credential rotation hold a clone
/// of this cell. Each HTTP request reads the current key via [`get`](Self::get);
/// the fallback layer swaps the active key via [`set`](Self::set) when a
/// credential is exhausted.
#[derive(Clone)]
pub struct SharedApiKey(Arc<RwLock<String>>);

impl SharedApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(Arc::new(RwLock::new(key.into())))
    }

    /// Read the current key (call before each HTTP request).
    pub fn get(&self) -> String {
        self.0.read().unwrap().clone()
    }

    /// Write a new key (called by credential rotation).
    pub fn set(&self, key: impl Into<String>) {
        *self.0.write().unwrap() = key.into();
    }
}

impl From<String> for SharedApiKey {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl std::fmt::Debug for SharedApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = self.get();
        let masked = if key.len() > 8 {
            format!("{}...{}", &key[..4], &key[key.len() - 4..])
        } else {
            "***".to_string()
        };
        f.debug_tuple("SharedApiKey").field(&masked).finish()
    }
}

/// Shared pool wrapper for thread-safe access.
#[derive(Clone)]
pub struct SharedCredentialPool {
    inner: Arc<Mutex<CredentialPool>>,
}

impl SharedCredentialPool {
    pub fn new(pool: CredentialPool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(pool)),
        }
    }

    pub fn next_credential(&self, scope: &str) -> Option<String> {
        let mut pool = self.inner.lock().unwrap();
        pool.next_credential(scope).map(|s| s.to_string())
    }

    pub fn mark_exhausted(&self, key: &str, scope: &str, reason: &FailoverReason, duration: Duration) {
        let mut pool = self.inner.lock().unwrap();
        pool.mark_exhausted(key, scope, reason, duration);
    }

    /// Time until the soonest key currently exhausted for `scope` recovers.
    pub fn soonest_recovery(&self, scope: &str) -> Option<Duration> {
        let pool = self.inner.lock().unwrap();
        pool.soonest_recovery(scope)
    }

    pub fn snapshot(&self, scope: &str) -> Vec<(String, Option<Duration>)> {
        let pool = self.inner.lock().unwrap();
        pool.snapshot(scope)
    }

    pub fn len(&self) -> usize {
        let pool = self.inner.lock().unwrap();
        pool.len()
    }

    pub fn is_empty(&self) -> bool {
        let pool = self.inner.lock().unwrap();
        pool.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL_A: &str = "model-a";
    const MODEL_B: &str = "model-b";

    #[test]
    fn fill_first_uses_first_active() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::FillFirst,
        );
        assert_eq!(pool.next_credential(MODEL_A), Some("key1"));
        assert_eq!(pool.next_credential(MODEL_A), Some("key1"));
    }

    #[test]
    fn round_robin_rotates() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::RoundRobin,
        );
        assert_eq!(pool.next_credential(MODEL_A), Some("key1"));
        assert_eq!(pool.next_credential(MODEL_A), Some("key2"));
        assert_eq!(pool.next_credential(MODEL_A), Some("key1"));
    }

    #[test]
    fn exhausted_key_skipped() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::FillFirst,
        );
        pool.mark_exhausted("key1", MODEL_A, &FailoverReason::RateLimit, Duration::from_secs(3600));
        assert_eq!(pool.next_credential(MODEL_A), Some("key2"));
    }

    #[test]
    fn cooldown_expires_and_restores() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string()],
            RotationStrategy::FillFirst,
        );
        pool.mark_exhausted("key1", MODEL_A, &FailoverReason::Auth, Duration::from_secs(300));
        assert_eq!(pool.next_credential(MODEL_A), None);

        // Simulate cooldown expiration by manipulating the timestamp
        pool.entries[0]
            .exhausted_until
            .insert(MODEL_A.to_string(), Instant::now() - Duration::from_secs(1));
        assert_eq!(pool.next_credential(MODEL_A), Some("key1"));
    }

    /// issue #197 (problem 1 root cause): a key exhausted for one model
    /// must remain fully usable for a different model sharing the same
    /// pool — `daemon/builder.rs` attaches one `CredentialPool` to every
    /// model under a provider, and quota is per (key, model) on providers
    /// like GLM, not per key alone.
    #[test]
    fn exhaustion_is_scoped_per_model_not_global_to_the_key() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string()],
            RotationStrategy::FillFirst,
        );
        pool.mark_exhausted("key1", MODEL_A, &FailoverReason::Billing, Duration::from_secs(5 * 3600));

        assert_eq!(
            pool.next_credential(MODEL_A),
            None,
            "key1 must be unavailable for the model that actually hit quota"
        );
        assert_eq!(
            pool.next_credential(MODEL_B),
            Some("key1"),
            "the same key must still be usable for an unrelated model"
        );
    }

    /// Complementary case: a *different* key must never be blocked by
    /// another key's exhaustion, for the same model or otherwise.
    #[test]
    fn one_key_exhausted_does_not_block_other_keys() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::FillFirst,
        );
        pool.mark_exhausted("key1", MODEL_A, &FailoverReason::Billing, Duration::from_secs(5 * 3600));
        assert_eq!(pool.next_credential(MODEL_A), Some("key2"));
    }

    #[test]
    fn soonest_recovery_reflects_the_shortest_remaining_cooldown_for_scope() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::FillFirst,
        );
        // key1: long Billing cooldown for MODEL_A.
        pool.mark_exhausted("key1", MODEL_A, &FailoverReason::Billing, Duration::from_secs(5 * 3600));
        // key2: much shorter RateLimit cooldown, same scope.
        pool.mark_exhausted("key2", MODEL_A, &FailoverReason::RateLimit, Duration::from_secs(60));

        let remaining = pool.soonest_recovery(MODEL_A).unwrap();
        assert!(
            remaining <= Duration::from_secs(60),
            "soonest_recovery must reflect key2's short cooldown, not key1's long one, got {remaining:?}"
        );

        // A model that never had any exhaustion recorded for it sees nothing.
        assert_eq!(pool.soonest_recovery(MODEL_B), None);
    }

    #[test]
    fn mask_key_hides_middle() {
        assert_eq!(
            CredentialPool::mask_key("sk-abcdefghijklmnopqrstuvwxyz"),
            "sk-a...wxyz"
        );
    }
}
