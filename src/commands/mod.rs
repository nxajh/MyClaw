//! Slash command system — intercepts `/command` messages in the orchestrator layer.
//!
//! Commands are parsed and dispatched before reaching the agent loop.
//! Each command returns a text response sent directly through the channel.

use crate::agents::AgentRuntime;
use crate::agents::session::{SessionManager, SessionOverride};
use std::sync::Arc;

mod config;
pub(crate) mod friends;
pub(crate) mod info;
mod link;
mod model;
pub(crate) mod register;
mod reload;
mod session;

pub use config::{cmd_autonomy, cmd_config, cmd_settings};
pub use friends::{
    cmd_friend_accept, cmd_friend_block, cmd_friend_decline, cmd_friend_request,
    cmd_friend_remove, cmd_friend_unblock, cmd_friends,
};
pub use info::{
    cmd_btw, cmd_context, cmd_export, cmd_groups, cmd_help, cmd_mcp, cmd_ping, cmd_skill,
    cmd_status, cmd_tools, cmd_users, cmd_whoami,
};
pub use link::{cmd_link, cmd_link_confirm};
pub use model::{cmd_model, cmd_models, cmd_think};
pub use register::{cmd_email, cmd_register, cmd_username};
pub use reload::{cmd_reload, cmd_stop};
pub use session::{
    cmd_compact, cmd_delete, cmd_history, cmd_new, cmd_rename, cmd_sessions, cmd_switch,
};

/// Context available to all command handlers.
pub struct CommandContext<'a> {
    /// The routing key (`channel:account:sender`) — used as the session key
    /// by SessionManager. Existing command handlers rely on this being the
    /// full routing key.
    pub user_id: &'a str,
    pub channel_type: &'a str,
    pub account_id: &'a str,
    pub registry: &'a Arc<dyn crate::providers::ProviderRegistry>,
    pub session_manager: &'a SessionManager,
    pub runtime: &'a AgentRuntime,
    /// Active SessionContext for this user (the canonical Arc<Mutex<Session>>
    /// the inbound Agent dispatch is using). Commands acquire
    /// `session_ctx.session.lock().await` for read/write access to live
    /// state — same Mutex used by Agent.run, so reads see the latest
    /// turn's state without stale-cache surprises.
    pub session_ctx: Option<&'a Arc<crate::agents::SessionContext>>,
    /// Global user registry for `/users`, `/whoami`, `/ping` queries.
    pub known_users: &'a Arc<crate::agents::KnownUsersRegistry>,
    /// P4 用户实体注册表（uid/email/username）——`/register`、`/email`、
    /// `/username` 与好友命令的 user.id / email 解析共用。
    pub user_registry: &'a Arc<crate::agents::UserRegistry>,
    /// Live channel registry — used by `/friend_*` to notify the peer
    /// (framework template push, RFC §4.3; never routed through the LLM).
    pub channels: &'a crate::agents::orchestrator::ChannelRegistry,
    /// The channel that received this command (for `/groups`, etc.).
    pub channel: Option<&'a Arc<dyn crate::api::message::Channel>>,
}

/// Parse a slash command from message content.
/// Returns `(command_name, args)` if the content starts with `/`.
///
/// Skips a leading `@bot_username` mention so that
/// `@bot /ping` is treated the same as `/ping`.
pub fn parse_command(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim();
    // Strip leading @mention (e.g. "@oci_my_claw_bot /ping" → "/ping").
    let trimmed = if trimmed.starts_with('@') {
        if let Some(space) = trimmed.find(' ') {
            trimmed[space..].trim_start()
        } else {
            return None; // "@bot" with nothing after
        }
    } else {
        trimmed
    };
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
        ("help", "Show available commands"),
        ("status", "Show current session status"),
        ("ping", "Bot health check"),
        ("whoami", "Show your user identity"),
        ("users", "List known users"),
        ("groups", "Show group statistics"),
        ("new", "Start a new session"),
        ("reset", "Reset the current session"),
        ("compact", "Compact conversation history"),
        ("model", "Set the active model"),
        ("models", "List available models"),
        ("stop", "Stop the daemon"),
        ("tools", "List available tools"),
        ("config", "View or set config options"),
        ("think", "Set thinking budget"),
        ("autonomy", "Set autonomy level"),
        ("settings", "Show current settings"),
        ("mcp", "MCP server status"),
        ("context", "Show context window usage"),
        ("btw", "Add a background note"),
        ("export", "Export conversation history"),
        ("history", "Show conversation history"),
        ("skills", "List loaded skills"),
        ("skill", "List loaded skills"),
        ("reload", "Reload skills and config"),
        ("sessions", "List all sessions"),
        ("ss", "List all sessions"),
        ("switch", "Switch to a session"),
        ("sw", "Switch to a session"),
        ("rename", "Rename a session"),
        ("rn", "Rename a session"),
        ("delete", "Delete a session"),
        ("del", "Delete a session"),
        ("friends", "List friend requests and contacts"),
        ("friend_request", "Send a friend request (u/username or email)"),
        ("friend_accept", "Accept a friend request (u/username or email)"),
        ("friend_decline", "Decline a friend request (u/username or email)"),
        ("friend_block", "Block a user (u/username or email)"),
        ("friend_unblock", "Unblock a user (u/username or email)"),
        ("friend_remove", "Remove a friend relationship (u/username or email)"),
        ("link", "Link this account to an existing user (u/username)"),
        ("link_confirm", "Confirm an identity link with the 6-digit code"),
        ("register", "Create your identity: /register <email> <username>"),
        ("email", "Set your email: /email set <email>"),
        ("username", "Set your username: /username set <username>"),
    ]
}

/// Return true if `cmd` is a recognized slash command name.
///
/// Used by the orchestrator to decide whether to spawn dispatch in a background
/// task (known commands) or fall through to the agent loop (unknown commands).
pub fn is_known_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "help"
            | "h"
            | "?"
            | "status"
            | "ping"
            | "whoami"
            | "users"
            | "groups"
            | "new"
            | "reset"
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
            | "skills"
            | "skill"
            | "reload"
            | "sessions"
            | "ss"
            | "switch"
            | "sw"
            | "rename"
            | "rn"
            | "delete"
            | "del"
            | "friends"
            | "friend_request"
            | "friend_accept"
            | "friend_decline"
            | "friend_block"
            | "friend_unblock"
            | "friend_remove"
            | "link"
            | "link_confirm"
            | "register"
            | "email"
            | "username"
    )
}

/// Dispatch a slash command. Returns the response text, or None if unrecognized.
pub async fn dispatch(cmd: &str, args: &str, ctx: CommandContext<'_>) -> Option<String> {
    match cmd {
        // ── Batch 1: core ──
        "help" | "h" | "?" => Some(info::cmd_help()),
        "status" => Some(info::cmd_status(ctx).await),
        "ping" => Some(info::cmd_ping(ctx)),
        "whoami" => Some(info::cmd_whoami(ctx)),
        "users" => Some(info::cmd_users(ctx)),
        "groups" => Some(info::cmd_groups(ctx)),
        "new" | "reset" => Some(session::cmd_new(args, ctx).await),
        "compact" => Some(session::cmd_compact(ctx).await),
        "model" => Some(model::cmd_model(args, ctx).await),
        "models" => Some(model::cmd_models(ctx)),
        "stop" => Some(reload::cmd_stop(ctx).await),
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
        // ── Batch 5: friends (RFC §4.2) ──
        "friends" => Some(friends::cmd_friends(ctx)),
        "friend_request" => Some(friends::cmd_friend_request(args, ctx).await),
        "friend_accept" => Some(friends::cmd_friend_accept(args, ctx).await),
        "friend_decline" => Some(friends::cmd_friend_decline(args, ctx).await),
        "friend_block" => Some(friends::cmd_friend_block(args, ctx)),
        "friend_unblock" => Some(friends::cmd_friend_unblock(args, ctx)),
        "friend_remove" => Some(friends::cmd_friend_remove(args, ctx)),
        // ── Batch 6: identity linking (RFC §2/P3) ──
        "link" => Some(link::cmd_link(args, ctx).await),
        "link_confirm" => Some(link::cmd_link_confirm(args, ctx).await),
        // ── Batch 7: user self-service (RFC §2.2/P4) ──
        "register" => Some(register::cmd_register(args, ctx)),
        "email" => Some(register::cmd_email(args, ctx)),
        "username" => Some(register::cmd_username(args, ctx)),
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
    ctx.session_manager
        .save_session_override(ctx.user_id, ov.clone());

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
pub(super) async fn get_history(
    ctx: &CommandContext<'_>,
) -> Option<Vec<crate::providers::ChatMessage>> {
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
