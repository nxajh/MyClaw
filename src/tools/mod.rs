//! tools — Tool implementations for the MyClaw runtime.
//!
//! Concrete implementations of `crate::providers::Tool` (Domain trait).
//!
//! **Core:** ShellTool, FileReadTool, FileWriteTool, FileEditTool, GlobSearchTool, ContentSearchTool
//! **Web:** WebFetchTool, HttpRequestTool, WebSearchTool
//! **Utility:** CalculatorTool, AskUserTool
//! **Multi-Agent:** DelegateTaskTool
//! **Memory:** MemoryStoreTool, MemoryRecallTool, MemoryForgetTool
//! **Planning:** TaskManagerTool
//! **Discovery:** ToolSearchTool, ListDirTool

mod ask_user;
mod calculator;
mod delegate;
mod file_ops;
mod http;
mod list_dir;
mod memory;
mod search;
mod shell;
mod skill_tool;
mod task;
pub mod tool_search;
pub mod truncation;
mod web;
mod web_search;

// Re-export tools.
pub use ask_user::AskUserTool;
pub use calculator::CalculatorTool;
pub use delegate::{DelegateTaskTool, TaskDelegator};
pub use file_ops::{FileEditTool, FileReadTool, FileWriteTool};
pub use http::HttpRequestTool;
pub use list_dir::ListDirTool;
pub use memory::{MemoryForgetTool, MemoryRecallTool, MemoryStore, MemoryStoreTool};
pub use search::{ContentSearchTool, GlobSearchTool};
pub use shell::ShellTool;
pub use skill_tool::SkillTool;
pub use task::{TaskManagerTool, TaskState};
pub use tool_search::ToolSearchTool;
pub use truncation::{truncate_output, truncate_tool_result};
pub use web::WebFetchTool;
pub use web_search::WebSearchTool;

use crate::providers::Tool;
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
        // Core tools
        Arc::new(ShellTool::new()),
        Arc::new(FileReadTool::new()),
        Arc::new(FileWriteTool::new()),
        Arc::new(FileEditTool::new()),
        Arc::new(GlobSearchTool::new()),
        Arc::new(ContentSearchTool::new()),
        // Web tools
        Arc::new(WebFetchTool::new()),
        Arc::new(HttpRequestTool::new()),
        // WebSearchTool requires a ServiceRegistry — registered separately in daemon.rs
        // Utility tools
        Arc::new(CalculatorTool::new()),
        Arc::new(AskUserTool::new()),
        // Memory tools
        Arc::new(MemoryStoreTool::new(mem.clone())),
        Arc::new(MemoryRecallTool::new(mem.clone())),
        Arc::new(MemoryForgetTool::new(mem)),
    ]
}
