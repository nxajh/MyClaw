//! QQ Bot channel adapter.
//!
//! Implements the [`Channel`] trait for the QQ Bot API (WebSocket gateway + REST).

pub mod types;
pub mod keyboard;
pub mod token;
pub mod channel;

// Re-export the message module from the parent for use within submodules.
use super::message;

pub use channel::QQBotChannel;
