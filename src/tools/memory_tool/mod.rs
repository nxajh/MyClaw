//! Memory tools — persistent memory CRUD with validation.
//!
//! Four tools:
//! - `memory_list`: list all memory entries with metadata
//! - `memory_view`: read a specific memory file's full content
//! - `memory_search`: keyword search across all memory files
//! - `memory_manage`: add / replace / remove entries with validation

mod audit;
mod format;
mod reader;
mod search;
mod writer;

pub use reader::{MemoryListTool, MemoryViewTool};
pub use writer::{MemoryManageTool, MemorySearchTool};
