//! QQ Bot channel adapter.
//!
//! Implements the [`Channel`] trait for the QQ Bot API (WebSocket gateway + REST).

pub mod channel;
pub mod keyboard;
pub mod markdown_sanitize;
pub mod token;
pub mod types;

// Re-export the message module from the parent for use within submodules.
use super::message;

pub use channel::QQBotChannel;
pub use markdown_sanitize::{sanitize_qq_markdown, sanitize_qq_markdown_dollars};
