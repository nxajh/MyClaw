//! Re-export Tool trait and types from the domain crate.
//!
//! The canonical definitions live in `capability::tool`.  This module
//! re-exports them so existing `use mcp::{Tool, ToolResult, ToolSpec}`
//! imports continue to compile without changes.

pub use capability::tool::{Tool, ToolResult, ToolSpec};
