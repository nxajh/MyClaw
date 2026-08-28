use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub(super) fn now_ms() -> u64 {
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
    /// RFC §2.2: 昵称不落快照——显示/比对一律实时取（UserRegistry）。
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
pub(super) struct PersistedFile {
    pub(super) version: u32,
    pub(super) users: HashMap<String, KnownUser>,
    /// RFC §4.1: owner user_id → (peer user_id → contact entry).
    #[serde(default)]
    pub(super) contacts: HashMap<String, HashMap<String, ContactEntry>>,
    /// RFC §3.5: recipient user_id → queued cross-user messages.
    #[serde(default)]
    pub(super) user_mailbox: HashMap<String, Vec<UserMail>>,
}

// ── Legacy migration format (old qqbot_known_users_{account}.json) ──────────

#[derive(Serialize, Deserialize)]
pub(super) struct LegacyUserEntry {
    pub(super) message_count: u32,
    pub(super) first_seen_ms: u64,
    pub(super) last_seen_ms: u64,
    pub(super) scope: String,
}
