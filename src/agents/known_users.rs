//! KnownUsersRegistry — global user registry shared across all channels.
//!
//! Replaces QQBot's internal `KnownSenders` + `RateLimiter` with a single
//! orchestrator-level table. Every inbound message from any channel is
//! registered here, giving:
//!
//! - **User tracking**: who has talked to the bot, across which channel/account.
//! - **Rate limiting**: per-routing_key (30/min) + global (300/min).
//! - **Proactive send targets**: `all_users()` / `users_for(channel, account)`.
//! - **Persistence**: single `known_users.json`, flushed every 60s.
//!
//! The registry is an `Arc` shared between the orchestrator (record side,
//! in `inbound::dispatch`) and slash commands (query side, via `CommandContext`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ── Rate limit constants ────────────────────────────────────────────────────

const SENDER_LIMIT_PER_MIN: u32 = 30;
const GLOBAL_LIMIT_PER_MIN: u32 = 300;
const WINDOW_MS: u64 = 60_000;

/// Maximum entries before oldest-eviction kicks in.
const MAX_USERS: usize = 10_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Build the canonical routing key: `channel:account:user_id`.
fn routing_key(channel: &str, account: &str, user_id: &str) -> String {
    format!("{channel}:{account}:{user_id}")
}

// ── KnownUser ───────────────────────────────────────────────────────────────

/// A known user entry, keyed by routing_key in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownUser {
    pub channel: String,
    pub account: String,
    pub user_id: String,
    pub message_count: u32,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// "c2c" or "group:{group_id}" — the most recent message scope.
    pub scope: String,
}

// ── Persisted file format ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct PersistedFile {
    version: u32,
    users: HashMap<String, KnownUser>,
}

// ── Legacy migration format (old qqbot_known_users_{account}.json) ──────────

#[derive(Serialize, Deserialize)]
struct LegacyUserEntry {
    message_count: u32,
    first_seen_ms: u64,
    last_seen_ms: u64,
    scope: String,
}

// ── Registry ────────────────────────────────────────────────────────────────

pub struct KnownUsersRegistry {
    /// routing_key → KnownUser
    users: DashMap<String, KnownUser>,
    /// routing_key → (count, window_start_ms) for per-sender rate limiting
    rate_buckets: DashMap<String, (u32, u64)>,
    /// Global rate counter across all channels/accounts
    global_count: AtomicU32,
    global_window_start: AtomicU64,
    /// File path for persistence (empty = in-memory, no flush)
    data_path: PathBuf,
    /// Set when users map changes; cleared on successful flush
    dirty: AtomicBool,
}

impl KnownUsersRegistry {
    /// Create a persistent registry backed by `{data_dir}/known_users.json`.
    pub fn new(data_dir: &Path) -> Self {
        let data_path = data_dir.join("known_users.json");
        let reg = Self {
            users: DashMap::new(),
            rate_buckets: DashMap::new(),
            global_count: AtomicU32::new(0),
            global_window_start: AtomicU64::new(0),
            data_path: data_path.clone(),
            dirty: AtomicBool::new(false),
        };
        reg.load_from_disk();
        reg
    }

    /// Create an in-memory registry (for tests / CLI mode). No persistence.
    pub fn in_memory() -> Self {
        Self {
            users: DashMap::new(),
            rate_buckets: DashMap::new(),
            global_count: AtomicU32::new(0),
            global_window_start: AtomicU64::new(0),
            data_path: PathBuf::new(),
            dirty: AtomicBool::new(false),
        }
    }

    fn load_from_disk(&self) {
        if self.data_path.as_os_str().is_empty() {
            return;
        }
        let contents = match std::fs::read_to_string(&self.data_path) {
            Ok(c) => c,
            Err(_) => return, // file doesn't exist yet — normal on first run
        };
        match serde_json::from_str::<PersistedFile>(&contents) {
            Ok(file) => {
                for (k, v) in file.users {
                    self.users.insert(k, v);
                }
                info!(
                    path = %self.data_path.display(),
                    count = self.users.len(),
                    "known_users: loaded from disk"
                );
            }
            Err(e) => {
                warn!(
                    path = %self.data_path.display(),
                    err = %e,
                    "known_users: failed to parse, starting empty"
                );
            }
        }
    }

    /// Migrate legacy `qqbot_known_users_{account}.json` files into the
    /// unified `known_users.json`. Called once at startup. No-op if the
    /// unified file already exists (already migrated or created fresh).
    pub fn migrate_legacy(&self, data_dir: &Path) {
        if !self.users.is_empty() {
            return; // unified file already has data
        }
        let mut migrated = 0;
        let read_dir = match std::fs::read_dir(data_dir) {
            Ok(rd) => rd,
            Err(_) => return,
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Match qqbot_known_users_{account}.json
            let Some(account) = fname
                .strip_prefix("qqbot_known_users_")
                .and_then(|s| s.strip_suffix(".json"))
            else {
                continue;
            };
            let contents = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let map: HashMap<String, LegacyUserEntry> = match serde_json::from_str(&contents) {
                Ok(m) => m,
                Err(e) => {
                    warn!(file = %fname, err = %e, "known_users: skipping unparseable legacy file");
                    continue;
                }
            };
            for (user_id, entry) in map {
                let rk = routing_key("qqbot", account, &user_id);
                self.users.insert(
                    rk,
                    KnownUser {
                        channel: "qqbot".to_string(),
                        account: account.to_string(),
                        user_id,
                        message_count: entry.message_count,
                        first_seen_ms: entry.first_seen_ms,
                        last_seen_ms: entry.last_seen_ms,
                        scope: entry.scope,
                    },
                );
                migrated += 1;
            }
        }
        if migrated > 0 {
            info!(migrated, "known_users: migrated from legacy qqbot files");
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    // ── Recording ───────────────────────────────────────────────────────────

    /// Rate-limit check + user registration in one call.
    /// Returns `true` if the message is allowed, `false` if rate-limited
    /// (caller should drop the message).
    pub fn check_and_record(
        &self,
        channel: &str,
        account: &str,
        user_id: &str,
        scope: &str,
    ) -> bool {
        let rk = routing_key(channel, account, user_id);
        if !self.check_rate_limit_global() {
            return false;
        }
        if !self.check_rate_limit_sender(&rk) {
            return false;
        }
        self.insert_or_update(&rk, channel, account, user_id, scope);
        true
    }

    /// Register a user without rate-limiting (e.g. INTERACTION_CREATE,
    /// button clicks that shouldn't consume the sender's quota).
    pub fn record(&self, channel: &str, account: &str, user_id: &str, scope: &str) {
        let rk = routing_key(channel, account, user_id);
        self.insert_or_update(&rk, channel, account, user_id, scope);
    }

    fn insert_or_update(
        &self,
        rk: &str,
        channel: &str,
        account: &str,
        user_id: &str,
        scope: &str,
    ) {
        let now = now_ms();
        self.dirty.store(true, Ordering::Relaxed);

        // Evict oldest if at capacity (only on new insert).
        if !self.users.contains_key(rk) && self.users.len() >= MAX_USERS {
            if let Some(oldest) = self
                .users
                .iter()
                .min_by_key(|e| e.value().last_seen_ms)
                .map(|e| e.key().clone())
            {
                self.users.remove(&oldest);
            }
        }

        match self.users.get_mut(rk) {
            Some(mut entry) => {
                entry.message_count += 1;
                entry.last_seen_ms = now;
                entry.scope = scope.to_string();
            }
            None => {
                self.users.insert(
                    rk.to_string(),
                    KnownUser {
                        channel: channel.to_string(),
                        account: account.to_string(),
                        user_id: user_id.to_string(),
                        message_count: 1,
                        first_seen_ms: now,
                        last_seen_ms: now,
                        scope: scope.to_string(),
                    },
                );
            }
        }
    }

    fn check_rate_limit_global(&self) -> bool {
        let now = now_ms();
        let window_start = self.global_window_start.load(Ordering::Relaxed);
        if now.saturating_sub(window_start) > WINDOW_MS {
            self.global_count.store(0, Ordering::Relaxed);
            self.global_window_start.store(now, Ordering::Relaxed);
        }
        let count = self.global_count.fetch_add(1, Ordering::Relaxed);
        count < GLOBAL_LIMIT_PER_MIN
    }

    fn check_rate_limit_sender(&self, rk: &str) -> bool {
        let now = now_ms();
        let mut entry = self.rate_buckets.entry(rk.to_string()).or_insert((0, now));
        let (count, window_start) = entry.value_mut();
        if now.saturating_sub(*window_start) > WINDOW_MS {
            *count = 0;
            *window_start = now;
        }
        if *count >= SENDER_LIMIT_PER_MIN {
            return false;
        }
        *count += 1;
        true
    }

    // ── Queries ─────────────────────────────────────────────────────────────

    /// Total unique users across all channels.
    pub fn count(&self) -> usize {
        self.users.len()
    }

    /// Total messages across all users.
    pub fn total_messages(&self) -> u32 {
        self.users.iter().map(|e| e.value().message_count).sum()
    }

    /// All known users.
    pub fn all_users(&self) -> Vec<KnownUser> {
        self.users.iter().map(|e| e.value().clone()).collect()
    }

    /// Users for a specific channel + account.
    pub fn users_for(&self, channel: &str, account: &str) -> Vec<KnownUser> {
        self.users
            .iter()
            .filter(|e| {
                e.value().channel == channel && e.value().account == account
            })
            .map(|e| e.value().clone())
            .collect()
    }

    // ── Persistence ─────────────────────────────────────────────────────────

    /// Flush to disk if dirty. No-op for in-memory registries.
    pub fn flush(&self) {
        if self.data_path.as_os_str().is_empty() {
            return;
        }
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return; // not dirty
        }
        let users: HashMap<String, KnownUser> = self
            .users
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        let file = PersistedFile {
            version: 1,
            users,
        };
        match serde_json::to_string_pretty(&file) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.data_path, json) {
                    warn!(
                        path = %self.data_path.display(),
                        err = %e,
                        "known_users: failed to flush"
                    );
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
            Err(e) => {
                warn!(err = %e, "known_users: failed to serialize");
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_creates_and_updates() {
        let reg = KnownUsersRegistry::in_memory();
        reg.record("qqbot", "xiaoer", "user_a", "c2c");
        assert_eq!(reg.count(), 1);
        assert_eq!(reg.total_messages(), 1);

        reg.record("qqbot", "xiaoer", "user_a", "c2c");
        assert_eq!(reg.count(), 1);
        assert_eq!(reg.total_messages(), 2);
    }

    #[test]
    fn check_and_record_respects_sender_limit() {
        let reg = KnownUsersRegistry::in_memory();
        for _ in 0..30 {
            assert!(
                reg.check_and_record("qqbot", "xiaoer", "spammer", "c2c"),
                "should allow within sender limit"
            );
        }
        assert!(
            !reg.check_and_record("qqbot", "xiaoer", "spammer", "c2c"),
            "should block after sender limit exceeded"
        );
        assert!(
            reg.check_and_record("qqbot", "xiaoer", "user_b", "c2c"),
            "different sender should be allowed"
        );
    }

    #[test]
    fn check_and_record_respects_global_limit() {
        let reg = KnownUsersRegistry::in_memory();
        for i in 0..300 {
            assert!(
                reg.check_and_record("qqbot", "xiaoer", &format!("user_{i}"), "c2c"),
                "should allow within global limit (sender {i})"
            );
        }
        assert!(
            !reg.check_and_record("qqbot", "xiaoer", "user_300", "c2c"),
            "should block after global limit exceeded"
        );
    }

    #[test]
    fn users_for_filters_by_channel_account() {
        let reg = KnownUsersRegistry::in_memory();
        reg.record("qqbot", "xiaoer", "user_a", "c2c");
        reg.record("qqbot", "xiaosan", "user_b", "c2c");
        reg.record("telegram", "default", "user_c", "c2c");

        let xiaoer = reg.users_for("qqbot", "xiaoer");
        assert_eq!(xiaoer.len(), 1);
        assert_eq!(xiaoer[0].user_id, "user_a");

        let all = reg.all_users();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn flush_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "myclaw_test_known_users_{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let reg = KnownUsersRegistry::new(&dir);
        // Override path to our test file.
        // KnownUsersRegistry::new always uses known_users.json, so we
        // test via the public flush + manual load path.
        reg.record("qqbot", "xiaoer", "user_a", "c2c");
        reg.record("telegram", "default", "12345", "c2c");
        reg.flush();

        // Load from the same file.
        let expected_path = dir.join("known_users.json");
        let contents = std::fs::read_to_string(&expected_path).unwrap();
        let file: PersistedFile = serde_json::from_str(&contents).unwrap();
        assert_eq!(file.users.len(), 2);

        let _ = std::fs::remove_file(&expected_path);
    }
}
