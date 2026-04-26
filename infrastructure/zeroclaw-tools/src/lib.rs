//! zeroclaw-tools — Infrastructure layer for built-in agent tools.
//!
//! Re-exports the full tool implementations from the existing `zeroclaw-tools`
//! crate. MCP-related modules (mcp_protocol, mcp_transport, mcp_client,
//! mcp_tool, mcp_deferred) are NOT re-exported here — they live in
//! `infrastructure/zeroclaw-mcp` instead.
//!
//! ## Architecture
//!
//! ```text
//! application/zeroclaw-runtime
//!         |
//!         +-- zeroclaw-tools-infra  ← tool implementations (browser, file, git, …)
//!         +-- zeroclaw-mcp          ← MCP client infrastructure
//!         |
//!         v
//! interface/zeroclaw-channels
//! ```

// Re-export all non-MCP modules from the original zeroclaw-tools crate.
// MCP modules are excluded because they have been migrated to zeroclaw-mcp.
pub use zeroclaw_tools::ask_user;
pub use zeroclaw_tools::backup_tool;
pub use zeroclaw_tools::browser;
pub use zeroclaw_tools::browser_delegate;
pub use zeroclaw_tools::browser_open;
pub use zeroclaw_tools::calculator;
pub use zeroclaw_tools::canvas;
pub use zeroclaw_tools::claude_code;
pub use zeroclaw_tools::claude_code_runner;
pub use zeroclaw_tools::cli_discovery;
pub use zeroclaw_tools::cloud_ops;
pub use zeroclaw_tools::cloud_patterns;
pub use zeroclaw_tools::codex_cli;
pub use zeroclaw_tools::composio;
pub use zeroclaw_tools::content_search;
pub use zeroclaw_tools::data_management;
pub use zeroclaw_tools::discord_search;
pub use zeroclaw_tools::escalate;
pub use zeroclaw_tools::file_edit;
pub use zeroclaw_tools::file_write;
pub use zeroclaw_tools::gemini_cli;
pub use zeroclaw_tools::git_operations;
pub use zeroclaw_tools::glob_search;
pub use zeroclaw_tools::google_workspace;
pub use zeroclaw_tools::hardware_board_info;
pub use zeroclaw_tools::hardware_memory_map;
pub use zeroclaw_tools::hardware_memory_read;
pub use zeroclaw_tools::http_request;
pub use zeroclaw_tools::image_gen;
pub use zeroclaw_tools::image_info;
pub use zeroclaw_tools::jira_tool;
pub use zeroclaw_tools::knowledge_tool;
pub use zeroclaw_tools::linkedin;
pub use zeroclaw_tools::linkedin_client;
pub use zeroclaw_tools::llm_task;
pub use zeroclaw_tools::memory_export;
pub use zeroclaw_tools::memory_forget;
pub use zeroclaw_tools::memory_purge;
pub use zeroclaw_tools::memory_recall;
pub use zeroclaw_tools::memory_store;
pub use zeroclaw_tools::microsoft365;
pub use zeroclaw_tools::model_routing_config;
pub use zeroclaw_tools::node_capabilities;
pub use zeroclaw_tools::notion_tool;
pub use zeroclaw_tools::opencode_cli;
pub use zeroclaw_tools::pdf_read;
pub use zeroclaw_tools::pipeline;
pub use zeroclaw_tools::poll;
pub use zeroclaw_tools::proxy_config;
pub use zeroclaw_tools::pushover;
pub use zeroclaw_tools::reaction;
pub use zeroclaw_tools::report_template_tool;
pub use zeroclaw_tools::report_templates;
pub use zeroclaw_tools::screenshot;
pub use zeroclaw_tools::sessions;
pub use zeroclaw_tools::swarm;
pub use zeroclaw_tools::text_browser;
pub use zeroclaw_tools::tool_search;
pub use zeroclaw_tools::util_helpers;
pub use zeroclaw_tools::weather_tool;
pub use zeroclaw_tools::web_fetch;
pub use zeroclaw_tools::web_search_provider_routing;
pub use zeroclaw_tools::web_search_tool;
pub use zeroclaw_tools::workspace_tool;
pub use zeroclaw_tools::wrappers;