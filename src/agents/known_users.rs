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

/// Re-request cooldown after a decline (RFC §4.1: same pair blocked for 24h).
const FRIEND_REQUEST_COOLDOWN_MS: u64 = 24 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Direction of a contact entry, from the owning user's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContactDirection {
    /// The peer requested us.
    In,
    /// We requested the peer.
    Out,
}

/// Contact relationship status (RFC §4.1 state machine).
///
/// `pending → accepted / declined / blocked`; `accepted → removed`;
/// `declined → pending` (after the 24h cooldown); `blocked → removed`
/// (unblock returns to no-relationship and requires a fresh request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContactStatus {
    Pending,
    Accepted,
    Declined,
    Blocked,
}

/// One directed contact entry. Both sides of a relationship store their own
/// entry (keyed by the peer's user_id); acceptance is mirrored so both sides
/// see `Accepted` and either side can initiate delivery (RFC §4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactEntry {
    pub status: ContactStatus,
    pub direction: ContactDirection,
    /// Peer's nickname as recorded on this side, e.g. "@alice".
    pub nickname: String,
    pub requested_at: u64,
    #[serde(default)]
    pub accepted_at: u64,
    /// Set when this side declined; blocks re-request for 24h.
    #[serde(default)]
    pub last_declined_at: u64,
}

/// A cross-user message sitting in the recipient's **user-level mailbox**
/// (RFC §3.5). Keyed by user_id (not session); injected — once — on the next
/// user message in any session. Persisted so delivery survives restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMail {
    pub msg_id: String,
    /// Full user_id (routing_key) of the sender.
    pub sender_user_id: String,
    /// Display nickname, e.g. "@alice".
    pub sender_nickname: String,
    pub text: String,
    pub sent_at: u64,
}

/// Outcome of a friend-request attempt (RFC §4.1 rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// Request created; the peer will be notified once.
    New,
    /// A request is already pending — no duplicate notification.
    AlreadyPending,
    /// The pair is already accepted — delivery is allowed.
    AlreadyAccepted,
    /// The peer blocked us — no request can be delivered.
    BlockedByPeer,
    /// The peer declined within the last 24h — re-request refused.
    DeclinedTooSoon,
}

/// Delivery check for cross-user messages (RFC §3.5: `from` may deliver to
/// `to` only when `to`'s entry for `from` is `accepted`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryVerdict {
    Allowed,
    /// No accepted relationship — the send is intercepted by the framework.
    NotFriends,
    /// The recipient blocked the sender — delivery is refused.
    Blocked,
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
    /// RFC §4.1: owner user_id → (peer user_id → contact entry).
    #[serde(default)]
    contacts: HashMap<String, HashMap<String, ContactEntry>>,
    /// RFC §3.5: recipient user_id → queued cross-user messages.
    #[serde(default)]
    user_mailbox: HashMap<String, Vec<UserMail>>,
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
    /// RFC §4.1: owner user_id → (peer user_id → contact entry)
    contacts: DashMap<String, HashMap<String, ContactEntry>>,
    /// RFC §3.5: recipient user_id → queued cross-user messages (persisted)
    user_mailbox: DashMap<String, Vec<UserMail>>,
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
            contacts: DashMap::new(),
            user_mailbox: DashMap::new(),
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
            contacts: DashMap::new(),
            user_mailbox: DashMap::new(),
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
                for (k, v) in file.contacts {
                    self.contacts.insert(k, v);
                }
                for (k, v) in file.user_mailbox {
                    self.user_mailbox.insert(k, v);
                }
                info!(
                    path = %self.data_path.display(),
                    count = self.users.len(),
                    contacts = self.contacts.len(),
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

    // ── Contacts (RFC §4) ───────────────────────────────────────────────────

    /// Default display nickname for a user_id (routing_key): "@" + last segment.
    /// Matches the RFC §2 identity model where the default user_id is the
    /// routing key and the platform user id is the human-facing alias.
    pub fn nick_of(user_id: &str) -> String {
        format!("@{}", user_id.rsplit(':').next().unwrap_or(user_id))
    }

    /// Initiate a friend request (RFC §4.1). Mirrors the request onto both
    /// sides: owner records `direction=Out`, peer records `direction=In`.
    ///
    /// Rules enforced here:
    /// - peer blocked us → `BlockedByPeer` (no delivery, no request).
    /// - peer declined within 24h → `DeclinedTooSoon` (cooldown).
    /// - request already pending → `AlreadyPending` (no duplicate notify).
    /// - already accepted → `AlreadyAccepted`.
    pub fn request_friend(&self, owner: &str, peer: &str) -> RequestOutcome {
        let now = now_ms();
        let owner_nick = Self::nick_of(owner);
        let peer_nick = Self::nick_of(peer);

        // Peer-side view decides (RFC §4.1: "发送校验查接收方名下条目").
        if let Some(peer_entry) = self.contact_entry(peer, owner) {
            match peer_entry.status {
                ContactStatus::Blocked => return RequestOutcome::BlockedByPeer,
                ContactStatus::Accepted => return RequestOutcome::AlreadyAccepted,
                ContactStatus::Declined => {
                    if now.saturating_sub(peer_entry.last_declined_at)
                        < FRIEND_REQUEST_COOLDOWN_MS
                    {
                        return RequestOutcome::DeclinedTooSoon;
                    }
                }
                ContactStatus::Pending => return RequestOutcome::AlreadyPending,
            }
        }
        // Owner-side view: an existing pending/accepted request is idempotent.
        if let Some(entry) = self.contact_entry(owner, peer) {
            match entry.status {
                ContactStatus::Accepted => return RequestOutcome::AlreadyAccepted,
                ContactStatus::Pending => return RequestOutcome::AlreadyPending,
                ContactStatus::Blocked | ContactStatus::Declined => {}
            }
        }

        self.dirty.store(true, Ordering::Relaxed);
        self.contacts
            .entry(owner.to_string())
            .or_default()
            .insert(
                peer.to_string(),
                ContactEntry {
                    status: ContactStatus::Pending,
                    direction: ContactDirection::Out,
                    nickname: peer_nick,
                    requested_at: now,
                    accepted_at: 0,
                    last_declined_at: 0,
                },
            );
        self.contacts
            .entry(peer.to_string())
            .or_default()
            .insert(
                owner.to_string(),
                ContactEntry {
                    status: ContactStatus::Pending,
                    direction: ContactDirection::In,
                    nickname: owner_nick,
                    requested_at: now,
                    accepted_at: 0,
                    last_declined_at: 0,
                },
            );
        RequestOutcome::New
    }

    /// Accept a pending inbound request. Mirrors both sides to `Accepted`.
    /// Returns `false` if there is no pending inbound request from the peer.
    pub fn accept_friend(&self, owner: &str, peer: &str) -> bool {
        let now = now_ms();
        let mut updated = false;
        if let Some(mut map) = self.contacts.get_mut(owner) {
            if let Some(entry) = map.get_mut(peer) {
                if entry.status == ContactStatus::Pending
                    && entry.direction == ContactDirection::In
                {
                    entry.status = ContactStatus::Accepted;
                    entry.accepted_at = now;
                    updated = true;
                }
            }
        }
        if updated {
            // Mirror to the requester's side.
            if let Some(mut map) = self.contacts.get_mut(peer) {
                if let Some(entry) = map.get_mut(owner) {
                    entry.status = ContactStatus::Accepted;
                    entry.accepted_at = now;
                }
            }
            self.dirty.store(true, Ordering::Relaxed);
        }
        updated
    }

    /// Decline a pending inbound request. Records the decline timestamp on
    /// both sides — this is what enforces the 24h re-request cooldown.
    pub fn decline_friend(&self, owner: &str, peer: &str) -> bool {
        let now = now_ms();
        let mut updated = false;
        if let Some(mut map) = self.contacts.get_mut(owner) {
            if let Some(entry) = map.get_mut(peer) {
                if entry.status == ContactStatus::Pending {
                    entry.status = ContactStatus::Declined;
                    entry.last_declined_at = now;
                    updated = true;
                }
            }
        }
        if updated {
            if let Some(mut map) = self.contacts.get_mut(peer) {
                if let Some(entry) = map.get_mut(owner) {
                    entry.status = ContactStatus::Declined;
                    entry.last_declined_at = now;
                }
            }
            self.dirty.store(true, Ordering::Relaxed);
        }
        updated
    }

    /// Block a user (owner-side only; the peer is never notified). Blocks
    /// all delivery in both directions immediately (RFC §4.1).
    pub fn block_friend(&self, owner: &str, peer: &str) {
        let now = now_ms();
        self.dirty.store(true, Ordering::Relaxed);
        let mut map = self.contacts.entry(owner.to_string()).or_default();
        let entry = map.entry(peer.to_string()).or_insert_with(|| ContactEntry {
            status: ContactStatus::Pending,
            direction: ContactDirection::In,
            nickname: Self::nick_of(peer),
            requested_at: now,
            accepted_at: 0,
            last_declined_at: 0,
        });
        entry.status = ContactStatus::Blocked;
    }

    /// Unblock a user — returns to no-relationship (a fresh request is
    /// required to re-establish, RFC §4.1 state machine).
    pub fn unblock_friend(&self, owner: &str, peer: &str) -> bool {
        let mut removed = false;
        if let Some(mut map) = self.contacts.get_mut(owner) {
            if let Some(entry) = map.get(peer) {
                if entry.status == ContactStatus::Blocked {
                    map.remove(peer);
                    removed = true;
                }
            }
        }
        if removed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        removed
    }

    /// Remove an established relationship. Both sides drop the accepted
    /// entry (re-request required to re-establish). Returns `false` if the
    /// pair was not established.
    pub fn remove_friend(&self, owner: &str, peer: &str) -> bool {
        let mut removed = false;
        if let Some(mut map) = self.contacts.get_mut(owner) {
            if let Some(entry) = map.get(peer) {
                if entry.status == ContactStatus::Accepted {
                    map.remove(peer);
                    removed = true;
                }
            }
        }
        if removed {
            if let Some(mut map) = self.contacts.get_mut(peer) {
                if let Some(entry) = map.get(owner) {
                    if entry.status == ContactStatus::Accepted {
                        map.remove(owner);
                    }
                }
            }
            self.dirty.store(true, Ordering::Relaxed);
        }
        removed
    }

    /// List this user's contacts as `(peer_user_id, entry)` pairs.
    pub fn list_contacts(&self, owner: &str) -> Vec<(String, ContactEntry)> {
        self.contacts
            .get(owner)
            .map(|map| {
                map.iter()
                    .map(|(peer, entry)| (peer.clone(), entry.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Pending **inbound** requests for the per-turn injection (RFC §4.3).
    pub fn pending_requests(&self, owner: &str) -> Vec<(String, ContactEntry)> {
        self.contacts
            .get(owner)
            .map(|map| {
                map.iter()
                    .filter(|(_, e)| {
                        e.status == ContactStatus::Pending
                            && e.direction == ContactDirection::In
                    })
                    .map(|(peer, entry)| (peer.clone(), entry.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Look up a contact by nickname (RFC §2: disambiguation only within
    /// existing relationships — you cannot @ a stranger). `nick` is compared
    /// without the leading `@`.
    pub fn resolve_contact_by_nick(
        &self,
        owner: &str,
        nick: &str,
    ) -> Option<(String, ContactEntry)> {
        let nick = nick.trim_start_matches('@');
        self.contacts.get(owner).and_then(|map| {
            map.iter()
                .find(|(_, e)| e.nickname.trim_start_matches('@') == nick)
                .map(|(peer, entry)| (peer.clone(), entry.clone()))
        })
    }

    /// Find known users whose platform user id (last routing-key segment)
    /// matches `nick`. Used by `/friend_request @alice` and the send_message
    /// resolution chain before a relationship exists. Multiple matches =
    /// ambiguous — the caller must ask for disambiguation (RFC P1 acceptance:
    /// "同名不同 user_id 解析正确").
    pub fn find_users_by_nick(&self, nick: &str) -> Vec<KnownUser> {
        let nick = nick.trim_start_matches('@');
        self.users
            .iter()
            .filter(|e| {
                e.value()
                    .user_id
                    .rsplit(':')
                    .next()
                    .is_some_and(|seg| seg == nick)
            })
            .map(|e| e.value().clone())
            .collect()
    }

    /// Delivery check for cross-user messages (RFC §3.5). `from` may deliver
    /// to `to` only when `to`'s entry for `from` is `accepted`.
    pub fn delivery_verdict(&self, from: &str, to: &str) -> DeliveryVerdict {
        match self.contact_entry(to, from) {
            Some(entry) if entry.status == ContactStatus::Accepted => DeliveryVerdict::Allowed,
            Some(entry) if entry.status == ContactStatus::Blocked => DeliveryVerdict::Blocked,
            _ => DeliveryVerdict::NotFriends,
        }
    }

    /// Queue a cross-user message into the recipient's user-level mailbox
    /// (RFC §3.5). Persisted; injected once on the next user message.
    pub fn push_user_mail(&self, to: &str, mail: UserMail) {
        self.dirty.store(true, Ordering::Relaxed);
        self.user_mailbox
            .entry(to.to_string())
            .or_default()
            .push(mail);
    }

    /// Drain the user-level mailbox (inject-once semantics — the caller
    /// renders the mails into the current turn, then they are gone).
    pub fn drain_user_mail(&self, user_id: &str) -> Vec<UserMail> {
        match self.user_mailbox.remove(user_id) {
            Some((_, mails)) => {
                if !mails.is_empty() {
                    self.dirty.store(true, Ordering::Relaxed);
                }
                mails
            }
            None => Vec::new(),
        }
    }

    fn contact_entry(&self, owner: &str, peer: &str) -> Option<ContactEntry> {
        self.contacts
            .get(owner)
            .and_then(|map| map.get(peer).cloned())
    }

    // ── Per-turn injection rendering (RFC §3.5 / §4.3) ───────────────────

    /// Last-seen timestamp (unix ms) of a known user, if registered (RFC §6
    /// P2 会话发现: presence of friends). Updated on every user interaction
    /// via [`Self::record`]. `None` when the peer has never interacted.
    pub fn last_seen_ms_of(&self, user_id: &str) -> Option<u64> {
        self.users.get(user_id).map(|e| e.value().last_seen_ms)
    }

    /// Render a presence summary from a last-seen timestamp (RFC §6 P2 会话
    /// 发现): status label + relative time. Thresholds: <5 min = 在线,
    /// <24 h = 最近活跃, otherwise 离线.
    pub(crate) fn render_presence(last_seen_ms: u64) -> String {
        let ago_ms = now_ms().saturating_sub(last_seen_ms);
        let label = if ago_ms < 5 * 60_000 {
            "🟢 在线"
        } else if ago_ms < 24 * 3_600_000 {
            "🟡 最近活跃"
        } else {
            "⚪ 离线"
        };
        let rel = if ago_ms < 60_000 {
            "刚刚".to_string()
        } else if ago_ms < 3_600_000 {
            format!("{} 分钟前", ago_ms / 60_000)
        } else if ago_ms < 86_400_000 {
            format!("{} 小时前", ago_ms / 3_600_000)
        } else {
            format!("{} 天前", ago_ms / 86_400_000)
        };
        format!("{label}（{rel}）")
    }

    /// Render a drained user-mailbox batch as one `<system-reminder>` user
    /// message (RFC §3.5: `来自 @{nick} 的消息:{text}`, 注入即消费 — the
    /// caller drains from the registry, so this text is shown exactly once).
    pub(crate) fn render_user_mail_reminder(mails: &[UserMail]) -> String {
        let mut lines = Vec::new();
        for mail in mails {
            lines.push(format!(
                "- [来自 {} 的消息] {}",
                mail.sender_nickname, mail.text
            ));
        }
        format!(
            "<system-reminder>\n[收到 {} 条来自好友的消息，请阅读并处理。]\n{}\n如需回复，使用 send_message 工具（recipient=@昵称，如 recipient=@alice）。\n</system-reminder>",
            mails.len(),
            lines.join("\n\n")
        )
    }

    /// Render pending **inbound** friend requests as one `<system-reminder>`
    /// user message (RFC §4.3 每轮注入 — re-rendered every turn while
    /// requests remain, so the agent always has context when the user says
    /// "接受"/"拒绝"). `pending` comes from [`Self::pending_requests`].
    pub(crate) fn render_pending_requests_reminder(pending: &[(String, ContactEntry)]) -> String {
        let mut lines = Vec::new();
        for (_, entry) in pending {
            let when = crate::agents::commands::info::format_ts(entry.requested_at);
            lines.push(format!("你有 1 条待处理好友请求:{}，发送于 {}", entry.nickname, when));
        }
        format!(
            "<system-reminder>\n[共有 {} 条待处理好友请求，用户可能直接回复“接受/拒绝”。]\n{}\n</system-reminder>",
            pending.len(),
            lines.join("\n")
        )
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
        let contacts: HashMap<String, HashMap<String, ContactEntry>> = self
            .contacts
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        let user_mailbox: HashMap<String, Vec<UserMail>> = self
            .user_mailbox
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        let file = PersistedFile {
            version: 1,
            users,
            contacts,
            user_mailbox,
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

    // ── Contacts (RFC §4) ───────────────────────────────────────────────────

    fn alice() -> &'static str {
        "qqbot:xiaoer:alice"
    }
    fn bob() -> &'static str {
        "qqbot:xiaoer:bob"
    }

    #[test]
    fn request_accept_delivery_flow() {
        let reg = KnownUsersRegistry::in_memory();
        assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);

        // Bob sees one pending inbound request.
        let pending = reg.pending_requests(bob());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, alice());
        assert_eq!(pending[0].1.status, ContactStatus::Pending);
        assert_eq!(pending[0].1.direction, ContactDirection::In);

        // Not friends yet → delivery intercepted.
        assert_eq!(
            reg.delivery_verdict(alice(), bob()),
            DeliveryVerdict::NotFriends
        );

        // Accept → both sides mirror to Accepted.
        assert!(reg.accept_friend(bob(), alice()));
        assert_eq!(
            reg.delivery_verdict(alice(), bob()),
            DeliveryVerdict::Allowed
        );
        assert_eq!(
            reg.delivery_verdict(bob(), alice()),
            DeliveryVerdict::Allowed
        );

        // Accepting a non-pending pair fails.
        assert!(!reg.accept_friend(alice(), bob()));
    }

    #[test]
    fn request_declined_cooldown_blocks_repeat() {
        let reg = KnownUsersRegistry::in_memory();
        assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);
        assert!(reg.decline_friend(bob(), alice()));

        // Re-request within 24h → refused.
        assert_eq!(
            reg.request_friend(alice(), bob()),
            RequestOutcome::DeclinedTooSoon
        );
        assert_eq!(
            reg.delivery_verdict(alice(), bob()),
            DeliveryVerdict::NotFriends
        );
    }

    #[test]
    fn request_blocked_by_peer() {
        let reg = KnownUsersRegistry::in_memory();
        reg.block_friend(bob(), alice());

        assert_eq!(
            reg.request_friend(alice(), bob()),
            RequestOutcome::BlockedByPeer
        );
        assert_eq!(
            reg.delivery_verdict(alice(), bob()),
            DeliveryVerdict::Blocked
        );

        // Unblock returns to no-relationship → fresh request allowed.
        assert!(reg.unblock_friend(bob(), alice()));
        assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);
    }

    #[test]
    fn request_pending_is_idempotent() {
        let reg = KnownUsersRegistry::in_memory();
        assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);
        assert_eq!(
            reg.request_friend(alice(), bob()),
            RequestOutcome::AlreadyPending
        );
        assert_eq!(reg.pending_requests(bob()).len(), 1);
    }

    #[test]
    fn remove_friend_breaks_delivery_both_ways() {
        let reg = KnownUsersRegistry::in_memory();
        reg.request_friend(alice(), bob());
        reg.accept_friend(bob(), alice());

        assert!(reg.remove_friend(bob(), alice()));
        assert_eq!(
            reg.delivery_verdict(alice(), bob()),
            DeliveryVerdict::NotFriends
        );
        assert_eq!(
            reg.delivery_verdict(bob(), alice()),
            DeliveryVerdict::NotFriends
        );
        assert!(!reg.remove_friend(bob(), alice()));
    }

    #[test]
    fn user_mailbox_drains_once() {
        let reg = KnownUsersRegistry::in_memory();
        reg.push_user_mail(
            bob(),
            UserMail {
                msg_id: "m1".into(),
                sender_user_id: alice().into(),
                sender_nickname: "@alice".into(),
                text: "hello".into(),
                sent_at: 1,
            },
        );
        reg.push_user_mail(
            bob(),
            UserMail {
                msg_id: "m2".into(),
                sender_user_id: alice().into(),
                sender_nickname: "@alice".into(),
                text: "again".into(),
                sent_at: 2,
            },
        );

        let drained = reg.drain_user_mail(bob());
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "hello");
        // Inject-once: second drain is empty.
        assert!(reg.drain_user_mail(bob()).is_empty());
    }

    #[test]
    fn render_user_mail_reminder_lists_all_mails() {
        let mails = vec![
            UserMail {
                msg_id: "m1".into(),
                sender_user_id: alice().into(),
                sender_nickname: "@alice".into(),
                text: "你好".into(),
                sent_at: 1,
            },
            UserMail {
                msg_id: "m2".into(),
                sender_user_id: alice().into(),
                sender_nickname: "@alice".into(),
                text: "在吗".into(),
                sent_at: 2,
            },
        ];
        let rendered = KnownUsersRegistry::render_user_mail_reminder(&mails);
        assert!(rendered.contains("<system-reminder>"), "{rendered}");
        assert!(rendered.contains("2 条来自好友的消息"), "{rendered}");
        assert!(rendered.contains("来自 @alice 的消息"), "{rendered}");
        assert!(rendered.contains("你好"), "{rendered}");
        assert!(rendered.contains("在吗"), "{rendered}");
    }

    #[test]
    fn render_pending_requests_reminder_lists_each_request() {
        let reg = KnownUsersRegistry::in_memory();
        reg.request_friend(alice(), bob());
        let pending = reg.pending_requests(bob());
        assert_eq!(pending.len(), 1);
        let rendered = KnownUsersRegistry::render_pending_requests_reminder(&pending);
        assert!(rendered.contains("<system-reminder>"), "{rendered}");
        assert!(rendered.contains("待处理好友请求"), "{rendered}");
        assert!(rendered.contains("@alice"), "{rendered}");
        // No pending → empty render list, no reminder text.
        let rendered_empty = KnownUsersRegistry::render_pending_requests_reminder(&[]);
        assert!(rendered_empty.contains("共有 0 条"), "{rendered_empty}");
    }

    #[test]
    fn last_seen_ms_of_tracks_user_activity() {
        // RFC §6 P2 会话发现: last_seen 数据源。
        let reg = KnownUsersRegistry::in_memory();
        assert!(reg.last_seen_ms_of(alice()).is_none());
        reg.record("qqbot", "xiaoer", "alice", "c2c");
        assert!(reg.last_seen_ms_of(alice()).unwrap() > 0);
    }

    #[test]
    fn render_presence_labels_online_recent_offline() {
        let now = now_ms();
        // Fresh interaction → online.
        let online = KnownUsersRegistry::render_presence(now);
        assert!(online.contains("🟢"), "{online}");
        assert!(online.contains("在线"), "{online}");
        // ~10 minutes ago → recently active.
        let recent = KnownUsersRegistry::render_presence(now - 10 * 60_000);
        assert!(recent.contains("🟡"), "{recent}");
        assert!(recent.contains("最近活跃"), "{recent}");
        // 3 days ago → offline.
        let offline = KnownUsersRegistry::render_presence(now - 3 * 86_400_000);
        assert!(offline.contains("⚪"), "{offline}");
        assert!(offline.contains("离线"), "{offline}");
    }

    #[test]
    fn render_user_mail_reminder_includes_reply_guidance() {
        // RFC §6 P2 回复转发闭环: 注入文本带回复引导, 接收方 agent 知道如何回。
        let mails = vec![UserMail {
            msg_id: "m1".into(),
            sender_user_id: alice().into(),
            sender_nickname: "@alice".into(),
            text: "你好".into(),
            sent_at: 1,
        }];
        let rendered = KnownUsersRegistry::render_user_mail_reminder(&mails);
        assert!(rendered.contains("send_message"), "{rendered}");
        assert!(rendered.contains("recipient=@昵称"), "{rendered}");
    }

    #[test]
    fn resolve_contact_by_nick_within_relationship() {
        let reg = KnownUsersRegistry::in_memory();
        reg.request_friend(alice(), bob());
        reg.accept_friend(bob(), alice());

        let (peer, entry) = reg.resolve_contact_by_nick(alice(), "bob").unwrap();
        assert_eq!(peer, bob());
        assert_eq!(entry.status, ContactStatus::Accepted);
        assert!(reg.resolve_contact_by_nick(alice(), "stranger").is_none());
    }

    #[test]
    fn find_users_by_nick_disambiguates_duplicates() {
        let reg = KnownUsersRegistry::in_memory();
        reg.record("qqbot", "xiaoer", "alice", "c2c");
        reg.record("telegram", "default", "alice", "c2c"); // same nick, other channel

        let matches = reg.find_users_by_nick("alice");
        assert_eq!(matches.len(), 2, "same nick on two accounts is ambiguous");

        // No match → empty.
        assert!(reg.find_users_by_nick("nobody").is_empty());
    }

    #[test]
    fn contacts_and_mailbox_persist_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "myclaw_contacts_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let expected_path = dir.join("known_users.json");

        let reg = KnownUsersRegistry::new(&dir);
        reg.request_friend(alice(), bob());
        reg.accept_friend(bob(), alice());
        reg.push_user_mail(
            bob(),
            UserMail {
                msg_id: "m1".into(),
                sender_user_id: alice().into(),
                sender_nickname: "@alice".into(),
                text: "persisted".into(),
                sent_at: 1,
            },
        );
        reg.flush();

        let contents = std::fs::read_to_string(&expected_path).unwrap();
        let file: PersistedFile = serde_json::from_str(&contents).unwrap();
        assert!(file.contacts.contains_key(alice()));
        assert!(file.user_mailbox.contains_key(bob()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
