//! channel_registry — live-channel lookup contracts, L0.
//!
//! [`ChannelRegistry`] (a thin typed seam over a
//! `DashMap<(channel_type, account_id), Arc<dyn Channel>>`) plus the
//! [`SessionKey`] value type it keys on. Sunk from
//! `agents::orchestrator::ctx` / `agents::orchestrator::key` in #151
//! phase8+ so the tools layer can hold the registry without reaching into
//! L4; the old orchestrator paths stay alive via re-exports (compat).

use std::fmt;
use std::sync::Arc;

use dashmap::DashMap;

use crate::api::message::Channel;

/// The set of live channels, keyed by `(channel_type, account_id)`.
///
/// A thin newtype over the underlying map so lookups go through a typed seam
/// (`get` / `get_by_key`) instead of raw `DashMap` access scattered across the
/// codebase. Cheap to clone (the map is behind an `Arc`).
#[derive(Clone, Default)]
pub struct ChannelRegistry {
    inner: Arc<DashMap<(String, String), Arc<dyn Channel>>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, account: (String, String), channel: Arc<dyn Channel>) {
        self.inner.insert(account, channel);
    }

    /// Look up a channel by its `(channel_type, account_id)` pair.
    pub fn get(&self, account: &(String, String)) -> Option<Arc<dyn Channel>> {
        self.inner.get(account).map(|r| r.clone())
    }

    /// Look up the channel that owns `key`'s session.
    pub fn get_by_key(&self, key: &SessionKey) -> Option<Arc<dyn Channel>> {
        self.get(&key.account_key())
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

use std::fmt;

/// A user session key: `channel_type:account_id:sender`.
///
/// The (channel_type, account_id) pair selects the channel instance;
/// `sender` distinguishes per-user sessions on that channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub channel: String,
    pub account: String,
    pub sender: String,
}

impl SessionKey {
    pub fn new(
        channel: impl Into<String>,
        account: impl Into<String>,
        sender: impl Into<String>,
    ) -> Self {
        Self {
            channel: channel.into(),
            account: account.into(),
            sender: sender.into(),
        }
    }

    /// Parse `"telegram:ops:12345"` into its three parts. Returns `None` if
    /// any of the three segments is missing or empty.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.splitn(3, ':');
        let channel = parts.next()?;
        let account = parts.next()?;
        let sender = parts.next()?;
        if channel.is_empty() || account.is_empty() || sender.is_empty() {
            return None;
        }
        Some(Self::new(channel, account, sender))
    }

    /// The `(channel_type, account_id)` pair used to look up a channel in the
    /// [`ChannelRegistry`].
    pub fn account_key(&self) -> (String, String) {
        (self.channel.clone(), self.account.clone())
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.channel, self.account, self.sender)
    }
}

/// A sub-agent session key: `agent_name:sub_session_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubAgentKey {
    pub agent: String,
    pub sub_session: String,
}

impl SubAgentKey {
    #[allow(dead_code)]
    pub fn new(agent: impl Into<String>, sub_session: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            sub_session: sub_session.into(),
        }
    }
}

impl fmt::Display for SubAgentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.agent, self.sub_session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_roundtrips() {
        let k = SessionKey::parse("telegram:ops:12345").unwrap();
        assert_eq!(k.channel, "telegram");
        assert_eq!(k.account, "ops");
        assert_eq!(k.sender, "12345");
        assert_eq!(k.to_string(), "telegram:ops:12345");
        assert_eq!(k.account_key(), ("telegram".into(), "ops".into()));
    }

    #[test]
    fn session_key_sender_may_contain_colons() {
        // splitn(3) keeps the remainder intact in `sender`.
        let k = SessionKey::parse("slack:team:U123:thread:9").unwrap();
        assert_eq!(k.channel, "slack");
        assert_eq!(k.account, "team");
        assert_eq!(k.sender, "U123:thread:9");
    }

    #[test]
    fn session_key_rejects_incomplete_or_empty() {
        assert!(SessionKey::parse("telegram:ops").is_none());
        assert!(SessionKey::parse("telegram").is_none());
        assert!(SessionKey::parse("").is_none());
        assert!(SessionKey::parse("telegram::12345").is_none());
        assert!(SessionKey::parse(":ops:12345").is_none());
        assert!(SessionKey::parse("telegram:ops:").is_none());
    }

    #[test]
    fn sub_agent_key_displays() {
        let k = SubAgentKey::new("researcher", "abc-123");
        assert_eq!(k.to_string(), "researcher:abc-123");
    }
}
