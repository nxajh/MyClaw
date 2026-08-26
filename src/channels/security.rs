//! Channel security policy types.
//!
//! `ChannelSecurityPolicy` itself lives in `crate::api::security` (L0
//! contract, moved in #151 Phase 3b); it is re-exported below.
//!
//! See `docs/channel-model-rfc.md` §14. Phase 4 unifies per-channel
//! authorization data behind a single shape so admin tooling, tests, and
//! external code can reason about "who can talk to this channel" without
//! reaching into channel-specific internals.

pub use crate::api::security::{AllowList, ChannelSecurityPolicy, GroupAuthMode};

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

/// Emit `warn!` log lines if the channel's security policy is configured
/// in a way that's likely a mis-configuration (empty whitelists, default
/// group reject). Call from `Channel::new()` so daemon startup surfaces
/// the warning before any messages arrive.
pub fn warn_if_locked_down(channel: &dyn super::Channel) {
    let policy = channel.security_policy();
    if policy.allowed_users.is_empty_whitelist() {
        tracing::warn!(
            channel = %channel.name(),
            "allowed_users is empty — channel will reject all DMs. \
             To allow all senders, set allowed_users = [\"*\"]."
        );
    }
    if matches!(policy.group_mode, GroupAuthMode::Reject)
        && matches!(policy.group_allowlist, AllowList::All)
    {
        // Default-reject groups + no explicit group config = Phase 4 default
        // landed unchanged. Warn so operators upgrading from pre-Phase-4
        // know group messages are now silently dropped.
        tracing::warn!(
            channel = %channel.name(),
            "groups are rejected (Phase 4 default). \
             To accept groups, set allowed_groups = [\"*\"] (all) or [\"g1\", \"g2\"] (whitelist). \
             For Telegram, set mention_only = true to only respond to @mentions in allowed groups."
        );
    }
}

/// Default `check_authorization` body — channels with a `ChannelSecurityPolicy`
/// can delegate here instead of re-implementing the dispatch logic.
pub fn evaluate(
    policy: &ChannelSecurityPolicy,
    sender: &str,
    scope: MessageScope<'_>,
) -> AuthDecision {
    match scope {
        MessageScope::Direct => {
            if policy.allowed_users.allows(sender) {
                AuthDecision::Allow
            } else {
                AuthDecision::Reject {
                    reason: "sender not in allowed_users",
                }
            }
        }
        MessageScope::Group { id, has_mention } => match policy.group_mode {
            GroupAuthMode::Reject => AuthDecision::Ignore,
            GroupAuthMode::MentionOnly if !has_mention => AuthDecision::Ignore,
            GroupAuthMode::Open | GroupAuthMode::MentionOnly => {
                if !policy.group_allowlist.allows(id) {
                    AuthDecision::Reject {
                        reason: "group not in allowed_groups",
                    }
                } else if policy.allowed_users.allows(sender) {
                    AuthDecision::Allow
                } else {
                    AuthDecision::Reject {
                        reason: "sender not in allowed_users",
                    }
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
                MessageScope::Group {
                    id: "g1",
                    has_mention: false
                }
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
                MessageScope::Group {
                    id: "g1",
                    has_mention: false
                }
            ),
            AuthDecision::Ignore
        );
        assert_eq!(
            evaluate(
                &policy,
                "alice",
                MessageScope::Group {
                    id: "g1",
                    has_mention: true
                }
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
                MessageScope::Group {
                    id: "g1",
                    has_mention: false
                }
            ),
            AuthDecision::Allow
        );
        assert!(matches!(
            evaluate(
                &policy,
                "alice",
                MessageScope::Group {
                    id: "g2",
                    has_mention: false
                }
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
