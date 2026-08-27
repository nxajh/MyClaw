//! Management-API router, extracted verbatim from `client.rs` (RFC
//! docs/webui-client-split-rfc.md, batch 2: pure move).
//!
//! [`ApiContext`] and the [`handle_api_request`] string-exact match
//! skeleton live here; each route arm's body moved unchanged into its
//! domain file and the arm forwards with an explicit argument list.
//! The match must stay exact-match — the `_` fallback behaviour for
//! unknown methods depends on it (no prefix dispatch).

use std::sync::Arc;
use std::sync::OnceLock;

use parking_lot::RwLock;

mod memory;
mod sessions;
mod skills;
mod system;

/// Shared handles passed to every API request handler.
pub(super) struct ApiContext<'a> {
    /// Session-manager scope key (channel:account:sender), stable across reconnects.
    pub(super) user_id: &'a str,
    pub(super) session_manager: &'a Arc<OnceLock<Arc<crate::agents::SessionManager>>>,
    pub(super) tool_specs: &'a Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
    pub(super) workspace_dir: &'a Arc<OnceLock<std::path::PathBuf>>,
    pub(super) memory_root: &'a Arc<OnceLock<std::path::PathBuf>>,
    pub(super) config_path: &'a Arc<OnceLock<std::path::PathBuf>>,
    pub(super) skill_manager: &'a Arc<OnceLock<Arc<RwLock<crate::agents::SkillManager>>>>,
    pub(super) provider_registry: &'a Arc<OnceLock<Arc<dyn crate::providers::ProviderRegistry>>>,
    pub(super) user_resolver: &'a Arc<OnceLock<Arc<crate::agents::UserResolver>>>,
}

/// Route a management API request and return a JSON response string.
pub(super) fn handle_api_request(
    id: &str,
    method: &str,
    params: &serde_json::Value,
    ctx: &ApiContext<'_>,
) -> String {
    let sm = match ctx.session_manager.get() {
        Some(sm) => sm,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "session manager not available"
            })
            .to_string();
        }
    };

    let user_id = ctx.user_id;

    match method {
        "sessions.list" => sessions::list(id, ctx, sm, user_id),

        "sessions.create" => sessions::create(id, params, sm, user_id),

        "sessions.switch" => sessions::switch(id, params, sm, user_id),

        "sessions.delete" => sessions::delete(id, params, sm, user_id),

        "sessions.delete_message" => sessions::delete_message(id, params, sm),

        "sessions.rename" => sessions::rename(id, params, sm),

        "tools.list" => {
            let specs = ctx.tool_specs.read();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": &*specs,
            }).to_string()
        }

        "memory.list" => memory::list(id, params, ctx),

        "memory.write" => memory::write(id, params, ctx),

        "memory.delete" => memory::delete(id, params, ctx),

        "memory.read" => memory::read(id, params, ctx),

        // ── file.read: read a workspace-relative file and return base64 ──
        "file.read" => system::file_read(id, params, ctx),

        "config.get" => system::config_get(id, ctx),

        "config.get_raw" => system::config_get_raw(id, ctx),

        "config.save" => system::config_save(id, params, ctx),

        "daemon.restart" => system::daemon_restart(id),

        "sessions.history" => sessions::history(id, sm, user_id),

        "skills.list" => skills::list(id, ctx),

        "skills.read" => skills::read(id, params, ctx),

        "skills.write" => skills::write(id, params, ctx),

        "skills.delete" => skills::delete(id, params, ctx),

        "commands.list" => system::commands_list(id),

        "models.list" => system::models_list(id, ctx, sm, user_id),

        "models.set" => system::models_set(id, params, sm, user_id),

        _ => {
            serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": format!("unknown method: {}", method)
            }).to_string()
        }
    }
}
