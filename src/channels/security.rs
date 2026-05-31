//! Channel security policy types.
//!
//! See `docs/channel-model-rfc.md` §14. Phase 4 unifies per-channel
//! authorization data behind a single shape so admin tooling, tests, and
//! external code can reason about "who can talk to this channel" without
//! reaching into channel-specific internals.

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

/// Scope of an inbound message — what kind of authorization decision the
/// channel needs to make. Borrowed (`'a`) so callers in hot poll loops
/// don't allocate.
#[derive(Debug, Clone, Copy)]
pub enum MessageScope<'a> {
    Direct,
    Group { id: &'a str, has_mention: bool },
}

/// Three-state authorization outcome.
///
/// `Ignore` ≠ `Reject`: a group message without `@mention` under
/// `MentionOnly` is *expected* to be dropped silently; an unlisted user
/// trying to DM is a *security event* worth logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    Ignore,
    Reject { reason: &'static str },
}

impl AuthDecision {
    pub fn allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Default `check_authorization` body — channels with a `ChannelSecurityPolicy`
/// can delegate here instead of re-implementing the dispatch logic.
pub fn evaluate(policy: &ChannelSecurityPolicy, sender: &str, scope: MessageScope<'_>) -> AuthDecision {
    match scope {
        MessageScope::Direct => {
            if policy.allowed_users.allows(sender) {
                AuthDecision::Allow
            } else {
                AuthDecision::Reject { reason: "sender not in allowed_users" }
            }
        }
        MessageScope::Group { id, has_mention } => match policy.group_mode {
            GroupAuthMode::Reject => AuthDecision::Ignore,
            GroupAuthMode::MentionOnly if !has_mention => AuthDecision::Ignore,
            GroupAuthMode::Open | GroupAuthMode::MentionOnly => {
                if !policy.group_allowlist.allows(id) {
                    AuthDecision::Reject { reason: "group not in allowed_groups" }
                } else if policy.allowed_users.allows(sender) {
                    AuthDecision::Allow
                } else {
                    AuthDecision::Reject { reason: "sender not in allowed_users" }
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_from_config_canonical() {
        assert_eq!(AllowList::from_config(None), AllowList::All);
        assert_eq!(
            AllowList::from_config(Some(vec![])),
            AllowList::Whitelist(vec![])
        );
        assert_eq!(
            AllowList::from_config(Some(vec!["*".into()])),
            AllowList::All
        );
        assert_eq!(
            AllowList::from_config(Some(vec!["a".into(), "*".into()])),
            AllowList::All
        );
        assert_eq!(
            AllowList::from_config(Some(vec!["alice".into(), "bob".into()])),
            AllowList::Whitelist(vec!["alice".into(), "bob".into()])
        );
    }

    #[test]
    fn allow_list_allows() {
        assert!(AllowList::All.allows("anyone"));
        let wl = AllowList::Whitelist(vec!["alice".into()]);
        assert!(wl.allows("alice"));
        assert!(!wl.allows("bob"));
        assert!(!AllowList::Whitelist(vec![]).allows("anyone"));
    }

    #[test]
    fn allow_list_is_empty_whitelist() {
        assert!(AllowList::Whitelist(vec![]).is_empty_whitelist());
        assert!(!AllowList::Whitelist(vec!["a".into()]).is_empty_whitelist());
        assert!(!AllowList::All.is_empty_whitelist());
    }

    #[test]
    fn evaluate_direct_allowed() {
        let policy = ChannelSecurityPolicy {
            allowed_users: AllowList::Whitelist(vec!["alice".into()]),
            group_mode: GroupAuthMode::Reject,
            group_allowlist: AllowList::All,
        };
        assert_eq!(
            evaluate(&policy, "alice", MessageScope::Direct),
            AuthDecision::Allow
        );
        assert!(matches!(
            evaluate(&policy, "bob", MessageScope::Direct),
            AuthDecision::Reject { .. }
        ));
    }

    #[test]
    fn evaluate_group_default_reject_ignores_silently() {
        // Phase 4 "统一关": group_mode=Reject means Ignore (not Reject) so
        // we don't spam logs on every group message in a default-config channel.
        let policy = ChannelSecurityPolicy::dm_only();
        assert_eq!(
            evaluate(
                &policy,
                "alice",
                MessageScope::Group { id: "g1", has_mention: false }
            ),
            AuthDecision::Ignore
        );
    }

    #[test]
    fn evaluate_group_mention_only() {
        let policy = ChannelSecurityPolicy {
            allowed_users: AllowList::All,
            group_mode: GroupAuthMode::MentionOnly,
            group_allowlist: AllowList::All,
        };
        assert_eq!(
            evaluate(
                &policy,
                "alice",
                MessageScope::Group { id: "g1", has_mention: false }
            ),
            AuthDecision::Ignore
        );
        assert_eq!(
            evaluate(
                &policy,
                "alice",
                MessageScope::Group { id: "g1", has_mention: true }
            ),
            AuthDecision::Allow
        );
    }

    #[test]
    fn evaluate_group_allowlist_filters() {
        let policy = ChannelSecurityPolicy {
            allowed_users: AllowList::All,
            group_mode: GroupAuthMode::Open,
            group_allowlist: AllowList::Whitelist(vec!["g1".into()]),
        };
        assert_eq!(
            evaluate(
                &policy,
                "alice",
                MessageScope::Group { id: "g1", has_mention: false }
            ),
            AuthDecision::Allow
        );
        assert!(matches!(
            evaluate(
                &policy,
                "alice",
                MessageScope::Group { id: "g2", has_mention: false }
            ),
            AuthDecision::Reject { .. }
        ));
    }

    #[test]
    fn auth_decision_allowed() {
        assert!(AuthDecision::Allow.allowed());
        assert!(!AuthDecision::Ignore.allowed());
        assert!(!AuthDecision::Reject { reason: "x" }.allowed());
    }
}
