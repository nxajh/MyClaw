//! Tool execution trait — core domain concept.
//!
//! Canonical definitions live in `crate::api::tool`. This module re-exports
//! them for backward compatibility.

pub use crate::api::tool::{Tool, ToolResult, ToolSource, ToolSpec};
