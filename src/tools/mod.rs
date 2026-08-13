//! tools — Tool implementations for the MyClaw runtime.
//!
//! Concrete implementations of `crate::providers::Tool` (Domain trait).
//!
//! **Core:** ShellTool, ShellPollTool, FileReadTool, FileWriteTool, FileEditTool, GlobSearchTool, ContentSearchTool
//! **Web:** HttpRequestTool (subsumes web_fetch via `strip_html` param), WebSearchTool
//! **Utility:** CalculatorTool, AskUserTool
//! **Multi-Agent:** AgentDelegateTool, AgentListTool, AgentKillTool, SessionsYieldTool
//! **Planning:** TaskCreateTool, TaskListTool, TaskUpdateTool, TaskDeleteTool
//! **Discovery:** ToolSearchTool, ListDirTool

mod agent_kill;
mod agent_list;
mod ask_user;
mod calculator;
mod cronjob_tool;
mod delegate;
mod file_ops;
mod friends;
mod hear_audio;
mod http;
mod list_dir;
mod media_download;
mod memory_tool;
#[cfg(test)]
mod memory_tool_tests;
mod search;
mod search_cooldown;
mod send_message;
pub mod shell;
mod session_query;
mod sessions_yield;
mod skill_manage_tool;
mod skill_tool;
mod skills_list_tool;
mod symbol_check;
mod task;
pub mod tool_search;
pub mod truncation;
mod view_image;
mod view_video;
mod web_search;

// Re-export tools.
pub use agent_kill::AgentKillTool;
pub use agent_list::AgentListTool;
pub use ask_user::AskUserTool;
pub use calculator::CalculatorTool;
pub use cronjob_tool::CronJobTool;
pub use delegate::AgentDelegateTool;
pub use file_ops::{FileEditTool, FileReadTool, FileWriteTool};
pub use friends::{
    FriendAcceptTool, FriendDeclineTool, FriendListTool, FriendRequestTool, FriendToolsCtx,
};
pub use hear_audio::HearAudioTool;
pub use http::HttpRequestTool;
pub use list_dir::ListDirTool;
pub use memory_tool::{MemoryListTool, MemoryManageTool, MemorySearchTool, MemoryViewTool};
pub use search::{ContentSearchTool, GlobSearchTool};
pub use search_cooldown::SearchProviderCooldown;
pub use send_message::SendMessageTool;
pub use session_query::SessionQueryTool;
pub use sessions_yield::SessionsYieldTool;
pub use shell::{ShellPollTool, ShellTool};
pub use skill_manage_tool::SkillManageTool;
pub use skill_tool::SkillTool;
pub use skills_list_tool::SkillsListTool;
pub use symbol_check::SymbolCheckTool;
pub use task::{TaskCreateTool, TaskDeleteTool, TaskListTool, TaskState, TaskUpdateTool, SharedTaskState, new_tools as new_task_tools, shared_state as shared_task_state, shared_state_persisted as shared_task_state_persisted};
pub use tool_search::ToolSearchTool;
pub use truncation::{truncate_output, truncate_tool_result};
pub use view_image::ViewImageTool;
pub use view_video::ViewVideoTool;
pub use web_search::WebSearchTool;

use crate::providers::Tool;
use std::sync::Arc;

/// Create all built-in tools that don't depend on shared state managed by
/// the daemon (router / channels / scheduler / etc). Tools requiring such
/// state — `ask_user`, `web_search`, `agent_delegate`, `agent_list`,
/// `agent_kill`, `sessions_yield`, `tool_search` — are registered by daemon.rs::build_tools.
pub fn builtin_tools(sessions_dir: Option<std::path::PathBuf>) -> Vec<Arc<dyn Tool>> {
    let shell = ShellTool::new(sessions_dir);
    let shell_poll = ShellPollTool::new(shell.bg_registry());
    vec![
        // Core tools
        Arc::new(shell),
        Arc::new(shell_poll),
        Arc::new(FileReadTool::new()),
        Arc::new(FileWriteTool::new()),
        Arc::new(FileEditTool::new()),
        Arc::new(GlobSearchTool::new()),
        Arc::new(ContentSearchTool::new()),
        Arc::new(SymbolCheckTool::new()),
        // Web tools — http_request subsumes web_fetch (use strip_html=true)
        Arc::new(HttpRequestTool::new()),
        // Utility tools
        Arc::new(CalculatorTool::new()),
    ]
}
