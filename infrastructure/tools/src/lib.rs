//! tools — Tool implementations for the MyClaw runtime.
//!
//! **Core:** ShellTool, FileReadTool, FileWriteTool, FileEditTool, GlobSearchTool, ContentSearchTool
//! **Memory:** MemoryStoreTool, MemoryRecallTool, MemoryForgetTool
//! **Web:** WebFetchTool

mod file_ops;
mod memory;
mod search;
mod shell;
mod web;

// Re-export tools.
pub use file_ops::{FileEditTool, FileReadTool, FileWriteTool};
pub use memory::{MemoryForgetTool, MemoryRecallTool, MemoryStore, MemoryStoreTool};
pub use search::{ContentSearchTool, GlobSearchTool};
pub use shell::ShellTool;
pub use web::WebFetchTool;

use mcp::Tool;
use std::sync::Arc;

/// Create all built-in tools with a shared memory store.
/// Returns (tools_vec, memory_store).
pub fn builtin_tools() -> (Vec<Arc<dyn Tool>>, MemoryStore) {
    let mem = MemoryStore::new();
    let tools = builtin_tools_with_memory(mem.clone());
    (tools, mem)
}

/// Create all built-in tools sharing the given MemoryStore.
pub fn builtin_tools_with_memory(mem: MemoryStore) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ShellTool::new()),
        Arc::new(FileReadTool::new()),
        Arc::new(FileWriteTool::new()),
        Arc::new(FileEditTool::new()),
        Arc::new(GlobSearchTool::new()),
        Arc::new(ContentSearchTool::new()),
        Arc::new(MemoryStoreTool::new(mem.clone())),
        Arc::new(MemoryRecallTool::new(mem.clone())),
        Arc::new(MemoryForgetTool::new(mem)),
        Arc::new(WebFetchTool::new()),
    ]
}
