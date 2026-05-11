//! Persistent multi-credential pool for same-provider failover.
//!
//! When a single API key hits a rate limit or billing exhaustion, the pool
//! rotates to the next available key instead of failing over to a different
//! provider (which may have higher cost or lower quality).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::providers::FailoverReason;

/// Status of a single credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    /// Key is healthy and available.
    Active,
    /// Key is temporarily exhausted (rate limit, billing) — on cooldown.
    Exhausted,
    /// Key has been manually disabled.
    Disabled,
}

/// A single credential entry in the pool.
#[derive(Debug, Clone)]
pub struct CredentialEntry {
    pub key: String,
    pub status: CredentialStatus,
    pub exhausted_until: Option<Instant>,
    pub last_used: Option<Instant>,
    pub use_count: u64,
}

/// Strategy for selecting the next credential from the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationStrategy {
    /// Use the first key until exhausted, then move to next.
    #[default]
    FillFirst,
    /// Round-robin across all keys.
    RoundRobin,
    /// Random selection among active keys.
    Random,
    /// Pick the key with the lowest use_count.
    LeastUsed,
}

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
    pub fn new(provider_name: impl Into<String>, keys: Vec<String>, strategy: RotationStrategy) -> Self {
        let entries = keys.into_iter().map(|key| CredentialEntry {
            key,
            status: CredentialStatus::Active,
            exhausted_until: None,
            last_used: None,
            use_count: 0,
        }).collect();
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

    /// Refresh exhausted credentials whose cooldown has expired.
    pub fn refresh(&mut self) {
        let now = Instant::now();
        for entry in &mut self.entries {
            if let Some(until) = entry.exhausted_until {
                if now >= until {
                    entry.status = CredentialStatus::Active;
                    entry.exhausted_until = None;
                    tracing::info!(
                        provider = %self.provider_name,
                        key_prefix = %Self::mask_key(&entry.key),
                        "credential cooldown expired, restored to active"
                    );
                }
            }
        }
    }

    /// Get the next available credential key.
    /// Returns None if all credentials are exhausted or disabled.
    pub fn next_credential(&mut self) -> Option<&str> {
        self.refresh();

        let active_indices: Vec<usize> = self.entries.iter()
            .enumerate()
            .filter(|(_, e)| e.status == CredentialStatus::Active)
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
            RotationStrategy::LeastUsed => {
                active_indices.into_iter()
                    .min_by_key(|i| self.entries[*i].use_count)
                    .unwrap_or(0)
            }
        };

        let entry = &mut self.entries[selected_idx];
        entry.last_used = Some(Instant::now());
        entry.use_count += 1;
        Some(&entry.key)
    }

    /// Mark a credential as exhausted based on the error reason.
    pub fn mark_exhausted(&mut self, key: &str, reason: &FailoverReason) {
        let cooldown = Self::cooldown_for_reason(reason);
        let now = Instant::now();

        for entry in &mut self.entries {
            if entry.key == key {
                entry.status = CredentialStatus::Exhausted;
                entry.exhausted_until = Some(now + cooldown);
                tracing::warn!(
                    provider = %self.provider_name,
                    key_prefix = %Self::mask_key(key),
                    reason = ?reason,
                    cooldown_secs = ?cooldown,
                    "credential marked exhausted"
                );
                break;
            }
        }
    }

    /// Snapshot of current pool state for diagnostics.
    pub fn snapshot(&self) -> Vec<(String, CredentialStatus, Option<Duration>)> {
        let now = Instant::now();
        self.entries.iter().map(|e| {
            let remaining = e.exhausted_until.map(|u| {
                if u > now { u.duration_since(now) } else { Duration::ZERO }
            });
            (Self::mask_key(&e.key), e.status, remaining)
        }).collect()
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    fn cooldown_for_reason(reason: &FailoverReason) -> Duration {
        match reason {
            FailoverReason::Auth => Duration::from_secs(5 * 60),        // 5 minutes
            FailoverReason::RateLimit => Duration::from_secs(60 * 60),  // 1 hour
            FailoverReason::Billing => Duration::from_secs(24 * 3600),  // 24 hours
            FailoverReason::Overloaded => Duration::from_secs(5 * 60),  // 5 minutes
            FailoverReason::ServerError => Duration::from_secs(10 * 60), // 10 minutes
            FailoverReason::Timeout => Duration::from_secs(5 * 60),      // 5 minutes
            _ => Duration::from_secs(60 * 60),                           // 1 hour default
        }
    }

    fn mask_key(key: &str) -> String {
        if key.len() <= 8 {
            "***".to_string()
        } else {
            format!("{}...{}", &key[..4], &key[key.len()-4..])
        }
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

    pub fn next_credential(&self) -> Option<String> {
        let mut pool = self.inner.lock().unwrap();
        pool.next_credential().map(|s| s.to_string())
    }

    pub fn mark_exhausted(&self, key: &str, reason: &FailoverReason) {
        let mut pool = self.inner.lock().unwrap();
        pool.mark_exhausted(key, reason);
    }

    pub fn snapshot(&self) -> Vec<(String, CredentialStatus, Option<Duration>)> {
        let pool = self.inner.lock().unwrap();
        pool.snapshot()
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

    #[test]
    fn fill_first_uses_first_active() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::FillFirst,
        );
        assert_eq!(pool.next_credential(), Some("key1"));
        assert_eq!(pool.next_credential(), Some("key1"));
    }

    #[test]
    fn round_robin_rotates() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::RoundRobin,
        );
        assert_eq!(pool.next_credential(), Some("key1"));
        assert_eq!(pool.next_credential(), Some("key2"));
        assert_eq!(pool.next_credential(), Some("key1"));
    }

    #[test]
    fn exhausted_key_skipped() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string(), "key2".to_string()],
            RotationStrategy::FillFirst,
        );
        pool.mark_exhausted("key1", &FailoverReason::RateLimit);
        assert_eq!(pool.next_credential(), Some("key2"));
    }

    #[test]
    fn cooldown_expires_and_restores() {
        let mut pool = CredentialPool::new(
            "test",
            vec!["key1".to_string()],
            RotationStrategy::FillFirst,
        );
        pool.mark_exhausted("key1", &FailoverReason::Auth);
        assert_eq!(pool.next_credential(), None);

        // Simulate cooldown expiration by manipulating the timestamp
        pool.entries[0].exhausted_until = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(pool.next_credential(), Some("key1"));
    }

    #[test]
    fn mask_key_hides_middle() {
        assert_eq!(CredentialPool::mask_key("sk-abcdefghijklmnopqrstuvwxyz"), "sk-a...wxyz");
    }
}
