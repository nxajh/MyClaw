use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tracing::{info, warn};

use super::routing::routing_key;
use super::types::{
    now_ms, ContactDirection, ContactEntry, ContactStatus, DeliveryVerdict, KnownUser,
    LegacyUserEntry, PersistedFile, RequestOutcome, UserMail,
};
use crate::identity::user_profile::UserResolver;

// ── Rate limit constants ────────────────────────────────────────────────────

const SENDER_LIMIT_PER_MIN: u32 = 30;
const GLOBAL_LIMIT_PER_MIN: u32 = 300;
const WINDOW_MS: u64 = 60_000;

/// Maximum entries before oldest-eviction kicks in.
const MAX_USERS: usize = 10_000;

/// Re-request cooldown after a decline (RFC §4.1: same pair blocked for 24h).
const FRIEND_REQUEST_COOLDOWN_MS: u64 = 24 * 60 * 60 * 1000;

// ── Registry ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct KnownUsersRegistry {
    /// routing_key → KnownUser
    pub(super) users: DashMap<String, KnownUser>,
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
    /// RFC §2/P3: shared identity resolver (routing_key → user_id). When
    /// installed, contacts and mailbox keys are resolved through it so a
    /// linked user (one human on several channels) shares friends and
    /// messages across channels. `users` stays keyed by routing_key (the
    /// per-channel registry); only relationship/mailbox keys are folded.
    resolver: Option<Arc<UserResolver>>,
}

impl KnownUsersRegistry {
    /// Create a persistent registry backed by `{base_dir}/known_users.json`.
    pub fn new(base_dir: &Path) -> Self {
        let data_path = crate::config::known_users_path(base_dir);
        let reg = Self {
            users: DashMap::new(),
            rate_buckets: DashMap::new(),
            global_count: AtomicU32::new(0),
            global_window_start: AtomicU64::new(0),
            contacts: DashMap::new(),
            user_mailbox: DashMap::new(),
            data_path: data_path.clone(),
            dirty: AtomicBool::new(false),
            resolver: None,
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
            resolver: None,
        }
    }

    /// Install the shared identity resolver (P3 身份绑定). Contacts and
    /// mailbox keys are then resolved through it: a linked user sees the
    /// same friends and messages from every channel.
    pub fn with_resolver(mut self, resolver: Arc<UserResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// The shared identity resolver, when installed (used by `/link`).
    pub fn resolver(&self) -> Option<&Arc<UserResolver>> {
        self.resolver.as_ref()
    }

    /// Resolve a key through the identity resolver; identity when unset.
    /// Public so callers (send_message, friend commands) fold sender ids
    /// the same way the registry folds mailbox/contact keys.
    pub fn resolve_uid(&self, key: &str) -> String {
        match &self.resolver {
            Some(r) => r.resolve(key),
            None => key.to_string(),
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
    pub fn migrate_legacy(&self, base_dir: &Path) {
        if !self.users.is_empty() {
            return; // unified file already has data
        }
        let mut migrated = 0;
        let read_dir = match std::fs::read_dir(base_dir) {
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
        let owner = self.resolve_uid(owner);
        let peer = self.resolve_uid(peer);

        // Peer-side view decides (RFC §4.1: "发送校验查接收方名下条目").
        if let Some(peer_entry) = self.contact_entry(&peer, &owner) {
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
        if let Some(entry) = self.contact_entry(&owner, &peer) {
            match entry.status {
                ContactStatus::Accepted => return RequestOutcome::AlreadyAccepted,
                ContactStatus::Pending => return RequestOutcome::AlreadyPending,
                ContactStatus::Blocked | ContactStatus::Declined => {}
            }
        }

        self.dirty.store(true, Ordering::Relaxed);
        self.contacts
            .entry(owner.clone())
            .or_default()
            .insert(
                peer.clone(),
                ContactEntry {
                    status: ContactStatus::Pending,
                    direction: ContactDirection::Out,
                    requested_at: now,
                    accepted_at: 0,
                    last_declined_at: 0,
                },
            );
        self.contacts
            .entry(peer.clone())
            .or_default()
            .insert(
                owner.clone(),
                ContactEntry {
                    status: ContactStatus::Pending,
                    direction: ContactDirection::In,
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
        let owner = self.resolve_uid(owner);
        let peer = self.resolve_uid(peer);
        let mut updated = false;
        if let Some(mut map) = self.contacts.get_mut(&owner) {
            if let Some(entry) = map.get_mut(&peer) {
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
            if let Some(mut map) = self.contacts.get_mut(&peer) {
                if let Some(entry) = map.get_mut(&owner) {
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
        let owner = self.resolve_uid(owner);
        let peer = self.resolve_uid(peer);
        let mut updated = false;
        if let Some(mut map) = self.contacts.get_mut(&owner) {
            if let Some(entry) = map.get_mut(&peer) {
                if entry.status == ContactStatus::Pending {
                    entry.status = ContactStatus::Declined;
                    entry.last_declined_at = now;
                    updated = true;
                }
            }
        }
        if updated {
            if let Some(mut map) = self.contacts.get_mut(&peer) {
                if let Some(entry) = map.get_mut(&owner) {
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
        let owner = self.resolve_uid(owner);
        let peer = self.resolve_uid(peer);
        self.dirty.store(true, Ordering::Relaxed);
        let mut map = self.contacts.entry(owner.clone()).or_default();
        let entry = map.entry(peer.clone()).or_insert_with(|| ContactEntry {
            status: ContactStatus::Pending,
            direction: ContactDirection::In,
            requested_at: now,
            accepted_at: 0,
            last_declined_at: 0,
        });
        entry.status = ContactStatus::Blocked;
    }

    /// Unblock a user — returns to no-relationship (a fresh request is
    /// required to re-establish, RFC §4.1 state machine).
    pub fn unblock_friend(&self, owner: &str, peer: &str) -> bool {
        let owner = self.resolve_uid(owner);
        let peer = self.resolve_uid(peer);
        let mut removed = false;
        if let Some(mut map) = self.contacts.get_mut(&owner) {
            if let Some(entry) = map.get(&peer) {
                if entry.status == ContactStatus::Blocked {
                    map.remove(&peer);
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
        let owner = self.resolve_uid(owner);
        let peer = self.resolve_uid(peer);
        let mut removed = false;
        if let Some(mut map) = self.contacts.get_mut(&owner) {
            if let Some(entry) = map.get(&peer) {
                if entry.status == ContactStatus::Accepted {
                    map.remove(&peer);
                    removed = true;
                }
            }
        }
        if removed {
            if let Some(mut map) = self.contacts.get_mut(&peer) {
                if let Some(entry) = map.get(&owner) {
                    if entry.status == ContactStatus::Accepted {
                        map.remove(&owner);
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
            .get(&self.resolve_uid(owner))
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
            .get(&self.resolve_uid(owner))
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

    /// 全部登记簿 routing_key（P4 迁移遍历用）。
    pub fn rk_keys(&self) -> Vec<String> {
        self.users.iter().map(|e| e.key().clone()).collect()
    }

    /// P4 迁移收尾：把 contacts / user_mailbox 中非 routing_key 形态的键
    /// （P3 /link 折叠产生的短 id 键，如 "alice"）统一重指 `target`
    /// （= root 的 user.id）。含 `:` 的键是 routing_key，保持原样（调用方
    /// 已通过 `migrate_identity` 并入折叠身份）。自环条目丢弃。
    pub fn rekey_legacy_to(&self, target: &str) {
        let mut changed = false;
        // contacts: 收集 (owner, peer, entry) 后重写。
        let mut rekeyed_contacts: Vec<(String, String, ContactEntry)> = Vec::new();
        for entry in self.contacts.iter() {
            let owner = entry.key();
            for (peer, contact) in entry.value() {
                let new_owner = if owner.contains(':') {
                    owner.clone()
                } else {
                    target.to_string()
                };
                let new_peer = if peer.contains(':') {
                    peer.clone()
                } else {
                    target.to_string()
                };
                if new_owner == new_peer {
                    continue; // 自环（同一存量用户在旧数据中互为联系人）
                }
                rekeyed_contacts.push((new_owner, new_peer, contact.clone()));
            }
        }
        if !rekeyed_contacts.is_empty() {
            self.contacts.clear();
            for (owner, peer, contact) in rekeyed_contacts {
                self.contacts
                    .entry(owner)
                    .or_default()
                    .entry(peer)
                    .or_insert(contact);
            }
            changed = true;
        }
        // user_mailbox: 非 rk 键 → target。
        let mut rekeyed_mail: Vec<(String, Vec<UserMail>)> = Vec::new();
        for entry in self.user_mailbox.iter() {
            let owner = entry.key();
            let new_owner = if owner.contains(':') {
                owner.clone()
            } else {
                target.to_string()
            };
            rekeyed_mail.push((new_owner, entry.value().clone()));
        }
        if !rekeyed_mail.is_empty() {
            self.user_mailbox.clear();
            for (owner, mails) in rekeyed_mail {
                self.user_mailbox
                    .entry(owner)
                    .or_default()
                    .extend(mails);
            }
            changed = true;
        }
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
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
            .entry(self.resolve_uid(to))
            .or_default()
            .push(mail);
    }

    /// Drain the user-level mailbox (inject-once semantics — the caller
    /// renders the mails into the current turn, then they are gone).
    pub fn drain_user_mail(&self, user_id: &str) -> Vec<UserMail> {
        match self.user_mailbox.remove(&self.resolve_uid(user_id)) {
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
            .get(&self.resolve_uid(owner))
            .and_then(|map| map.get(&self.resolve_uid(peer)).cloned())
    }

    // ── Per-turn injection rendering (RFC §3.5 / §4.3) ───────────────────

    /// Last-seen timestamp (unix ms) of a known user, if registered (RFC §6
    /// P2 会话发现: presence of friends). Updated on every user interaction
    /// via [`Self::record`]. `None` when the peer has never interacted.
    ///
    /// P3 身份绑定: for a folded user_id the most recent activity across all
    /// its routing_keys wins (a linked user is "online" if any channel is).
    pub fn last_seen_ms_of(&self, user_id: &str) -> Option<u64> {
        let uid = self.resolve_uid(user_id);
        let mut keys: Vec<String> = match &self.resolver {
            Some(r) => r.routing_keys_for(&uid),
            None => Vec::new(),
        };
        if keys.is_empty() {
            keys.push(uid);
        }
        keys.into_iter()
            .filter_map(|k| self.users.get(&k).map(|e| e.value().last_seen_ms))
            .max()
    }

    /// P3 身份绑定: fold `old_rk`'s relationship data into `new_uid` after a
    /// successful `/link_confirm`. Moves:
    /// - the user-level mailbox under `old_rk` → appended under `new_uid`;
    /// - `old_rk`'s own contacts (as owner) → merged under `new_uid`
    ///   (peer conflicts keep the existing entry);
    /// - other owners' entries pointing at `old_rk` → re-pointed to `new_uid`
    ///   (conflicts keep the existing entry).
    ///
    /// No-op when `old_rk == new_uid` (unlinked identity). The `users` table
    /// is untouched — per-routing-key activity stays per-channel.
    pub fn migrate_identity(&self, old_rk: &str, new_uid: &str) {
        if old_rk == new_uid {
            return;
        }
        // Mailbox: move the queue under the folded identity.
        let mut moved_mailbox = false;
        if let Some((_, mut mails)) = self.user_mailbox.remove(old_rk) {
            if !mails.is_empty() {
                moved_mailbox = true;
            }
            self.user_mailbox
                .entry(new_uid.to_string())
                .or_default()
                .append(&mut mails);
        }
        // Contacts: old_rk as owner → merge under new_uid.
        let mut moved_owner = false;
        if let Some((_, own_contacts)) = self.contacts.remove(old_rk) {
            if !own_contacts.is_empty() {
                moved_owner = true;
            }
            let mut target = self.contacts.entry(new_uid.to_string()).or_default();
            for (peer, entry) in own_contacts {
                target.entry(peer).or_insert(entry);
            }
        }
        // Contacts: other owners' entries pointing at old_rk → re-point.
        // (Collect keys first — DashMap `iter_mut` holds the global write
        // lock, so we must not mutate inside the iteration.)
        let mut affected: Vec<String> = Vec::new();
        for entry in self.contacts.iter() {
            if entry.value().contains_key(old_rk) {
                affected.push(entry.key().clone());
            }
        }
        let mut moved_peer = false;
        for owner in affected {
            let removed = {
                let mut map = match self.contacts.get_mut(&owner) {
                    Some(m) => m,
                    None => continue,
                };
                map.remove(old_rk)
            };
            if let Some(entry) = removed {
                moved_peer = true;
                // The relationship key follows the folded identity.
                let mut map = self.contacts.get_mut(&owner).unwrap();
                map.entry(new_uid.to_string()).or_insert(entry);
            }
        }
        if moved_mailbox || moved_owner || moved_peer {
            self.dirty.store(true, Ordering::Relaxed);
        }
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
            "<system-reminder>\n[收到 {} 条来自好友的消息，请阅读并处理。]\n{}\n如需回复，使用 send_message 工具（recipient=u/uid 或邮箱，如 recipient=u/alice）。\n</system-reminder>",
            mails.len(),
            lines.join("\n\n")
        )
    }

    /// Render pending **inbound** friend requests as one `<system-reminder>`
    /// user message (RFC §4.3 每轮注入 — re-rendered every turn while
    /// requests remain, so the agent always has context when the user says
    /// "接受"/"拒绝"). `pending` comes from [`Self::pending_requests`];
    /// `display` 实时渲染对方显示名（RFC §2.2: 昵称不落快照）。
    pub(crate) fn render_pending_requests_reminder(
        pending: &[(String, ContactEntry)],
        display: impl Fn(&str) -> String,
    ) -> String {
        let mut lines = Vec::new();
        for (peer, entry) in pending {
            let when = crate::str_utils::format_ts(entry.requested_at);
            lines.push(format!("你有 1 条待处理好友请求:{}，发送于 {}", display(peer), when));
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
