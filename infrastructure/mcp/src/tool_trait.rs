//! Re-export Tool trait and types from the domain crate.
//!
//! The canonical definitions live in `myclaw_capability::tool`.  This module
//! re-exports them so existing `use myclaw_mcp::{Tool, ToolResult, ToolSpec}`
//! imports continue to compile without changes.

pub use myclaw_capability::tool::{Tool, ToolResult, ToolSpec};
