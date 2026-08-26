//! api — L0 contract layer.
//!
//! Types defined here are the shared contracts between all layers:
//! - Message types (MessageSender, MessageReceiver, PersistedChannelMessage)
//! - Tool trait and ToolContext (replacing direct Session dependency)
//!
//! No layer may depend on another layer's internals for these contracts —
//! everything goes through `api`.
pub mod loop_breaker;
pub mod message;
pub mod run_mode;
pub mod security;
pub mod tool;
pub mod turn_event;
pub mod turn_stream;

