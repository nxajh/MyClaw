//! Slash command system — intercepts `/command` messages in the orchestrator layer.
//!
//! Commands are parsed and dispatched before reaching the agent loop.
//! Each command returns a text response sent directly through the channel.

use crate::agents::agent_impl::{Agent, AgentLoop};
use crate::agents::mcp_manager::McpManager;
use crate::agents::session::{SessionManager, SessionOverride};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

mod config;
mod info;
mod model;
mod reload;
mod session;

pub use config::{cmd_config, cmd_settings, cmd_autonomy};
pub use info::{cmd_help, cmd_status, cmd_tools, cmd_context, cmd_mcp, cmd_skill, cmd_btw, cmd_export};
pub use model::{cmd_model, cmd_models, cmd_think};
pub use reload::{cmd_stop, cmd_reload};
pub use session::{cmd_new, cmd_compact, cmd_history, cmd_sessions, cmd_switch, cmd_rename, cmd_delete};

/// Context available to all command handlers.
pub struct CommandContext<'a> {
    pub user_id: &'a str,
    pub registry: &'a Arc<dyn crate::providers::ServiceRegistry>,
    pub session_manager: &'a SessionManager,
    pub agent: &'a Agent,
    /// Access to the current session's agent loop (if it exists).
    pub agent_loop: Option<&'a Arc<TokioMutex<AgentLoop>>>,
    /// MCP manager (for /mcp command).
    pub mcp_manager: Option<&'a Arc<McpManager>>,
    /// Sessions cache — needed by /new to evict stale agent loops.
    pub sessions: &'a DashMap<String, Arc<crate::agents::SessionHandle>>,
    /// Search provider cooldown tracker (for /status command).
    pub search_cooldown: Option<&'a Arc<crate::tools::search_cooldown::SearchProviderCooldown>>,
}

/// Parse a slash command from message content.
/// Returns `(command_name, args)` if the content starts with `/`.
pub fn parse_command(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let rest = &trimmed[1..];
    if rest.is_empty() {
        return None;
    }
    let (cmd, args) = match rest.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    // Reject obviously non-command input (e.g. URLs, file paths).
    if cmd.contains('/') || cmd.contains('\\') || cmd.contains('.') {
        return None;
    }
    Some((cmd, args))
}

/// Return true if `cmd` is a recognized slash command name.
///
/// Used by the orchestrator to decide whether to spawn dispatch in a background
/// task (known commands) or fall through to the agent loop (unknown commands).
pub fn is_known_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "help" | "h" | "?"
        | "status"
        | "new" | "reset"
        | "compact"
        | "model"
        | "models"
        | "stop"
        | "tools"
        | "config"
        | "think"
        | "autonomy"
        | "settings"
        | "mcp"
        | "context"
        | "btw"
        | "export"
        | "history"
        | "skills" | "skill"
        | "reload"
        | "sessions" | "ss"
        | "switch" | "sw"
        | "rename" | "rn"
        | "delete" | "del"
    )
}

/// Dispatch a slash command. Returns the response text, or None if unrecognized.
pub async fn dispatch(cmd: &str, args: &str, ctx: CommandContext<'_>) -> Option<String> {
    match cmd {
        // ── Batch 1: core ──
        "help" | "h" | "?" => Some(info::cmd_help()),
        "status" => Some(info::cmd_status(ctx).await),
        "new" | "reset" => Some(session::cmd_new(args, ctx).await),
        "compact" => Some(session::cmd_compact(ctx).await),
        "model" => Some(model::cmd_model(args, ctx).await),
        "models" => Some(model::cmd_models(ctx)),
        "stop" => Some(reload::cmd_stop()),
        // ── Batch 2: enhanced ──
        "tools" => Some(info::cmd_tools(ctx)),
        "config" => Some(config::cmd_config(args, ctx)),
        "think" => Some(model::cmd_think(args, ctx).await),
        "autonomy" => Some(config::cmd_autonomy(args, ctx).await),
        "settings" => Some(config::cmd_settings(ctx).await),
        "mcp" => Some(info::cmd_mcp(ctx).await),
        "context" => Some(info::cmd_context(ctx).await),
        "btw" => Some(info::cmd_btw(args, ctx).await),
        "export" => Some(info::cmd_export(ctx).await),
        "history" => Some(session::cmd_history(ctx).await),
        // ── Batch 3 ──
        "skills" | "skill" => Some(info::cmd_skill(ctx)),
        "reload" => Some(reload::cmd_reload(ctx).await),
        // ── Batch 4: session management ──
        "sessions" | "ss" => Some(session::cmd_sessions(ctx)),
        "switch" | "sw" => Some(session::cmd_switch(args, ctx).await),
        "rename" | "rn" => Some(session::cmd_rename(args, ctx)),
        "delete" | "del" => Some(session::cmd_delete(args, ctx).await),
        _ => None,
    }
}

/// Persist a session override through both the session manager and the live agent loop.
///
/// Calling this ensures the override takes effect immediately (live loop) AND
/// survives a restart (persisted to meta.json via session_manager).
pub(super) async fn apply_and_persist_override(ov: SessionOverride, ctx: &CommandContext<'_>) {
    // Persist via session_manager (updates cache + disk).
    ctx.session_manager.save_session_override(ctx.user_id, ov.clone());

    // Also update the live agent loop if one exists.
    if let Some(loop_arc) = ctx.agent_loop {
        let mut guard = loop_arc.lock().await;
        guard.apply_session_override(ov);
    }
}

/// Get session history: from active agent loop if available, otherwise from session_manager.
pub(super) async fn get_history(ctx: &CommandContext<'_>) -> Option<Vec<crate::providers::ChatMessage>> {
    if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        if !guard.session().history.is_empty() {
            return Some(guard.session().history.clone());
        }
    }
    let session = ctx.session_manager.get_or_create(ctx.user_id);
    if session.history.is_empty() {
        None
    } else {
        Some(session.history)
    }
}
