//! Slash command system — intercepts `/command` messages in the orchestrator layer.
//!
//! Commands are parsed and dispatched before reaching the agent loop.
//! Each command returns a text response sent directly through the channel.

use crate::agents::agent_impl::AgentBuilder;
use crate::agents::mcp_manager::McpManager;
use crate::agents::session::{SessionManager, SessionOverride};
use dashmap::DashMap;
use std::sync::Arc;

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
    pub registry: &'a Arc<dyn crate::providers::ProviderRegistry>,
    pub session_manager: &'a SessionManager,
    pub agent: &'a AgentBuilder,
    /// Active SessionContext for this user (the canonical Arc<Mutex<Session>>
    /// the inbound Agent dispatch is using). Commands acquire
    /// `session_ctx.session.lock().await` for read/write access to live
    /// state — same Mutex used by Agent.run, so reads see the latest
    /// turn's state without stale-cache surprises.
    pub session_ctx: Option<&'a Arc<crate::agents::SessionContext>>,
    /// MCP manager (for /mcp command).
    pub mcp_manager: Option<&'a Arc<McpManager>>,
    /// SessionContext cache — needed by /new and /switch to evict the
    /// cached SessionContext when the active session changes.
    pub session_contexts: &'a DashMap<String, Arc<crate::agents::SessionContext>>,
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

/// Return a catalog of all available slash commands as `(name, description)` pairs.
/// Used by the WebUI to power slash-command autocomplete.
pub fn command_catalog() -> Vec<(&'static str, &'static str)> {
    vec![
        ("help",      "Show available commands"),
        ("status",    "Show current session status"),
        ("new",       "Start a new session"),
        ("reset",     "Reset the current session"),
        ("compact",   "Compact conversation history"),
        ("model",     "Set the active model"),
        ("models",    "List available models"),
        ("stop",      "Stop the daemon"),
        ("tools",     "List available tools"),
        ("config",    "View or set config options"),
        ("think",     "Set thinking budget"),
        ("autonomy",  "Set autonomy level"),
        ("settings",  "Show current settings"),
        ("mcp",       "MCP server status"),
        ("context",   "Show context window usage"),
        ("btw",       "Add a background note"),
        ("export",    "Export conversation history"),
        ("history",   "Show conversation history"),
        ("skills",    "List loaded skills"),
        ("skill",     "List loaded skills"),
        ("reload",    "Reload skills and config"),
        ("sessions",  "List all sessions"),
        ("ss",        "List all sessions"),
        ("switch",    "Switch to a session"),
        ("sw",        "Switch to a session"),
        ("rename",    "Rename a session"),
        ("rn",        "Rename a session"),
        ("delete",    "Delete a session"),
        ("del",       "Delete a session"),
    ]
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

/// Persist a session override through both the session manager and the
/// live SessionContext (so the next turn picks up the change immediately).
///
/// If the session is locked by a running turn, the in-memory update is
/// queued in a background task instead of blocking the command — the
/// SessionManager.save_session_override above has already written to
/// the backend, and the next turn (after the in-flight one finishes)
/// reads the updated override from `session.session_override` once the
/// queued task acquires the lock.
pub(super) async fn apply_and_persist_override(ov: SessionOverride, ctx: &CommandContext<'_>) {
    // Persist via session_manager (updates cache + disk).
    ctx.session_manager.save_session_override(ctx.user_id, ov.clone());

    // Also update the live SessionContext if one is active.
    if let Some(session_ctx) = ctx.session_ctx {
        if let Ok(mut session) = session_ctx.session.try_lock() {
            session.session_override = ov;
        } else {
            // Session is locked by a running turn — queue the update so
            // it lands once the lock releases.
            let session_ctx = session_ctx.clone();
            tokio::spawn(async move {
                let mut session = session_ctx.session.lock().await;
                session.session_override = ov;
            });
        }
    }
}

/// Get session history. Tries the active SessionContext first (canonical
/// live state); if the session lock is held by a running turn, falls
/// through to the SessionManager cache (slightly stale, but never
/// blocks the command for the LLM-call duration).
pub(super) async fn get_history(ctx: &CommandContext<'_>) -> Option<Vec<crate::providers::ChatMessage>> {
    if let Some(session_ctx) = ctx.session_ctx {
        if let Ok(session) = session_ctx.session.try_lock() {
            if !session.history.is_empty() {
                return Some(session.history.clone());
            }
        }
        // try_lock failed → session busy; fall through to cache snapshot.
    }
    let session = ctx.session_manager.get_or_create(ctx.user_id);
    if session.history.is_empty() {
        None
    } else {
        Some(session.history)
    }
}
