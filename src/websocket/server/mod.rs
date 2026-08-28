//! WebSocketChannel — WebSocket-based channel for TUI and Web UI clients.
//!
//! Unlike other channels (Telegram, QQBot) where MyClaw is a *client* connecting
//! to an external platform, WebSocketChannel runs a WebSocket *server* that TUI and
//! Web UI clients connect to.
//!
//! ## Shared state & lock ordering
//!
//! Deferred-init handles (`session_manager`, `workspace_dir`, `config_path`,
//! `skill_manager`, `provider_registry`) are wired by the daemon after
//! construction and never change afterwards, so they use `OnceLock` —
//! lock-free reads, no `RwLock<Option<_>>` nesting. The remaining mutable
//! maps use `parking_lot::RwLock`.
//!
//! Lock-ordering rule (to avoid deadlocks): never hold a connection-map
//! guard across an `.await`. The `skill_manager` inner `RwLock` is a leaf —
//! take it last and release it before touching the connection maps.
mod api;
mod bus;
mod channel;
mod turn;

pub use channel::WebSocketChannel;

// Test-import forwarding: `mod tests` at the bottom consumes these via
// `use super::*`. cfg(test)-gated because after batch 3 mod.rs itself holds
// no non-test code — an unconditional `use` would be an unused import in
// non-test builds (clippy runs with -D warnings). Batch 4 moved `mod tests`
// out to tests.rs but kept this block — `use super::*` there still receives
// the imports from the parent module.
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use parking_lot::{Mutex as SyncMutex, RwLock};
#[cfg(test)]
use std::sync::OnceLock;

#[cfg(test)]
use crate::channels::message::{Channel, ChannelOutboundMessage};
#[cfg(test)]
use crate::config::channel::WebSocketConfig;
#[cfg(test)]
use api::{ApiContext, handle_api_request};
#[cfg(test)]
use bus::{SessionOutputBus, bus_key_candidates};
#[cfg(test)] mod tests;
