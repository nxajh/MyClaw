//! Re-export Tool trait and types from the domain crate.
//!
//! The canonical definitions live in `crate::providers::tool`.  This module
//! re-exports them so existing `use myclaw_mcp::{Tool, ToolResult, ToolSpec}`
//! imports continue to compile without changes.

pub use crate::providers::capability_tool::{Tool, ToolResult, ToolSpec};
