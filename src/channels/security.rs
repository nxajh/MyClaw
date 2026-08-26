//! Channel security policy types.
//!
//! `ChannelSecurityPolicy` / `AllowList` / `GroupAuthMode` live in
//! `crate::api::security` (L0 contract, moved in #151 Phase 3b); the
//! authorization helpers (`evaluate`, `MessageScope`, `AuthDecision`)
//! moved there too in Phase 3c. All are re-exported below.
//! `warn_if_locked_down` stays here — it takes `&dyn Channel` (runtime
//! side of the contract).
//!
//! See `docs/channel-model-rfc.md` §14. Phase 4 unifies per-channel
//! authorization data behind a single shape so admin tooling, tests, and
//! external code can reason about "who can talk to this channel" without
//! reaching into channel-specific internals.

pub use crate::api::security::{
    AllowList, AuthDecision, ChannelSecurityPolicy, GroupAuthMode, MessageScope, evaluate,
};

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
        // landed unchanged. Warn so operators upgrading from pre-Phase 4
        // know group messages are now silently dropped.
        tracing::warn!(
            channel = %channel.name(),
            "groups are rejected (Phase 4 default). \
             To accept groups, set allowed_groups = [\"*\"] (all) or [\"g1\", \"g2\"] (whitelist). \
             For Telegram, set mention_only = true to only respond to @mentions in allowed groups."
        );
    }
}
