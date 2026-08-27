//! tools — Tool implementations for the MyClaw runtime.
//!
//! Concrete implementations of `crate::providers::Tool` (Domain trait).
//!
//! **Core:** ShellTool, ShellPollTool, FileReadTool, FileWriteTool, FileEditTool, GlobSearchTool, ContentSearchTool
//! **Web:** HttpRequestTool (subsumes web_fetch via `strip_html` param), WebSearchTool
//! **Utility:** CalculatorTool, AskUserTool
//! **Multi-Agent:** AgentDelegateTool, AgentListTool, AgentKillTool, AgentResumeTool, SessionsYieldTool
//! **Planning:** TaskCreateTool, TaskListTool, TaskUpdateTool, TaskDeleteTool
//! **Discovery:** ToolSearchTool, ListDirTool

mod agent_kill;
mod agent_list;
mod agent_resume;
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
pub mod shell_env;
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
pub use agent_resume::AgentResumeTool;
pub use ask_user::AskUserTool;
pub use calculator::CalculatorTool;
pub use cronjob_tool::CronJobTool;
#[cfg(test)]
pub(crate) use cronjob_tool::{
    format_unknown_job_listing, parse_webhook_channel, resolve_delivery_for_create,
    resolve_delivery_for_update,
};
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
pub use shell::{ShellKillTool, ShellPollTool, ShellTool};
pub use skill_manage_tool::SkillManageTool;
pub use skill_tool::SkillTool;
pub use skills_list_tool::SkillsListTool;
pub use symbol_check::SymbolCheckTool;
pub use task::{new_tools as new_task_tools, TaskBoards, TaskState};
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
///
/// Also returns the shell process registry so the daemon can run
/// `shell::adopt_after_restart` on it at startup before any tool call can
/// reach it — bare CLI callers (no persistent session dir, no daemon to
/// restart) can ignore it.
///
/// `shell_notice_tx` (issue #129): where `background: true` shell commands
/// report completion, so the orchestrator can wake the spawning session.
/// `None` for bare CLI usage / tests — no orchestrator to wake.
pub fn builtin_tools(
    sessions_dir: Option<std::path::PathBuf>,
    shell_notice_tx: Option<tokio::sync::mpsc::Sender<shell::ShellCompletion>>,
) -> (Vec<Arc<dyn Tool>>, shell::ShellRegistry, Arc<ShellTool>) {
    let shell = match shell_notice_tx {
        Some(tx) => ShellTool::new_with_notice_sender(sessions_dir, tx),
        None => ShellTool::new(sessions_dir),
    };
    let shell_registry = shell.registry();
    let shell_poll = ShellPollTool::new(shell.registry(), shell.sessions_dir());
    let shell_kill = ShellKillTool::new(shell.registry());
    // issue #140: keep a concrete handle so the caller can wire in the
    // SessionManager once it exists (`ShellTool::set_session_manager`) —
    // built before SessionManager in daemon.rs's composition order, same
    // reason `ClientChannel`/`DelegationCoordinator` use a setter instead of
    // a constructor param.
    let shell = Arc::new(shell);
    let tools = vec![
        // Core tools
        Arc::clone(&shell) as Arc<dyn Tool>,
        Arc::new(shell_poll),
        Arc::new(shell_kill),
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
    ];
    (tools, shell_registry, shell)
}
