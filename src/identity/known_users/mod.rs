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

mod registry;
mod routing;
mod types;

pub use registry::KnownUsersRegistry;
pub use types::{
    ContactDirection, ContactEntry, ContactStatus, DeliveryVerdict, KnownUser, RequestOutcome,
    UserMail,
};
pub(crate) use routing::rk_for;

// 仅供本模块 tests 消费的绑定（tests 经 `use super::*` glob 引入）；
// cfg(test) 门控以免非测试构建触发 unused imports。
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use crate::identity::user_profile::UserResolver;
#[cfg(test)]
use types::{now_ms, PersistedFile};

#[cfg(test)]
mod tests;
