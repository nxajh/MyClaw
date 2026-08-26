//! Channel security policy — canonical security-policy contract types (L0).
//!
//! Moved from `channels/security.rs` (#151 Phase 3b): the policy snapshot
//! and its field types are cross-layer contracts (agents read them via
//! `Channel::security_policy()` / `check_authorization`), while evaluation
//! helpers (`evaluate`, `warn_if_locked_down`, `MessageScope`,
//! `AuthDecision`) stay in `channels`.

/// Which senders are permitted.
///
/// Cross-channel canonical semantic:
/// - `All` — allow everyone (config: `["*"]` or omitted)
/// - `Whitelist(vec)` — only listed IDs; **empty list rejects all**
///
/// `from_config` centralizes the `Option<Vec<String>>` → `AllowList`
/// mapping so each channel's config parser stays one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowList {
    All,
    Whitelist(Vec<String>),
}

impl AllowList {
    /// Parse from a TOML-style `Option<Vec<String>>` field.
    ///
    /// | Input                       | Output                  |
    /// |-----------------------------|-------------------------|
    /// | `None` (field omitted)      | `All`                   |
    /// | `Some(vec ["*", ...])`      | `All`                   |
    /// | `Some(vec![])`              | `Whitelist(vec![])`  ⇒ reject all |
    /// | `Some(vec)`                 | `Whitelist(vec)`        |
    pub fn from_config(opt: Option<Vec<String>>) -> Self {
        match opt {
            None => Self::All,
            Some(v) if v.iter().any(|s| s == "*") => Self::All,
            Some(v) => Self::Whitelist(v),
        }
    }

    pub fn allows(&self, sender: &str) -> bool {
        match self {
            Self::All => true,
            Self::Whitelist(v) => v.iter().any(|u| u == sender),
        }
    }

    /// True iff this list is `Whitelist(empty)` — i.e. configured to reject
    /// everyone. Used by `warn_if_locked_down` to flag likely-misconfigured
    /// channels at startup.
    pub fn is_empty_whitelist(&self) -> bool {
        matches!(self, Self::Whitelist(v) if v.is_empty())
    }
}

/// How a channel handles group messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupAuthMode {
    /// Reject all group messages (Phase 4 default — "统一关").
    Reject,
    /// Accept all group messages that pass `group_allowlist`.
    Open,
    /// Only accept group messages that @mention the bot (Telegram
    /// `mention_only` carryover).
    MentionOnly,
}

/// Full security policy snapshot for one channel. Cloneable so
/// `Channel::security_policy()` can return a snapshot pulled from a
/// hot-reload RwLock without exposing the lock.
#[derive(Debug, Clone)]
pub struct ChannelSecurityPolicy {
    pub allowed_users: AllowList,
    pub group_mode: GroupAuthMode,
    pub group_allowlist: AllowList,
}

impl ChannelSecurityPolicy {
    /// "Open" default — allow all DMs, accept all groups. Used by Client
    /// (where connection-level token authn already gates senders).
    pub fn open() -> Self {
        Self {
            allowed_users: AllowList::All,
            group_mode: GroupAuthMode::Open,
            group_allowlist: AllowList::All,
        }
    }

    /// Phase 4 conservative default — DMs allowed, groups rejected. Used
    /// when a channel has no explicit group config.
    pub fn dm_only() -> Self {
        Self {
            allowed_users: AllowList::All,
            group_mode: GroupAuthMode::Reject,
            group_allowlist: AllowList::All,
        }
    }
}
