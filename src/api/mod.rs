//! api — L0 contract layer.
//!
//! Types defined here are the shared contracts between all layers:
//! - Message types (MessageSender, MessageReceiver, PersistedChannelMessage)
//! - Tool trait and ToolContext (replacing direct Session dependency)
//!
//! No layer may depend on another layer's internals for these contracts —
//! everything goes through `api`.
pub mod agent_lifecycle;
pub mod agent_mail;
pub mod ask_fulfillment;
pub mod capability;
pub mod channel_registry;
pub mod delegation;
pub mod loop_breaker;
pub mod message;
pub mod run_mode;
pub mod security;
pub mod session_store;
pub mod skill_registry;
pub mod tool_listing;
pub mod tool;
pub mod turn_event;
pub mod turn_stream;

