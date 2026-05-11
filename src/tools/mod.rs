//! tools — Tool implementations for the MyClaw runtime.
//!
//! Concrete implementations of `crate::providers::Tool` (Domain trait).
//!
//! **Core:** ShellTool, FileReadTool, FileWriteTool, FileEditTool, GlobSearchTool, ContentSearchTool
//! **Web:** WebFetchTool, HttpRequestTool, WebSearchTool
//! **Utility:** CalculatorTool, AskUserTool
//! **Multi-Agent:** AgentDelegateTool
//! **Planning:** TaskManagerTool
//! **Discovery:** ToolSearchTool, ListDirTool

mod ask_user;
mod calculator;
mod delegate;
mod file_ops;
mod http;
mod list_dir;
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
pub use delegate::{AgentDelegateTool, TaskDelegator};
pub use file_ops::{FileEditTool, FileReadTool, FileWriteTool};
pub use http::HttpRequestTool;
pub use list_dir::ListDirTool;
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

/// Create all built-in tools.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
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
    ]
}
