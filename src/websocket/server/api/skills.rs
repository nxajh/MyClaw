//! `skills.*` management-API route handlers, extracted verbatim from
//! `client.rs::handle_api_request` (RFC docs/websocket-client-split-rfc.md,
//! batch 2: pure move — each function body is the original match-arm
//! body, unchanged; only the wrapper signature was added.)
//!
//! The `is_safe_skill_name` / `resolve_skill_dir` /
//! `reload_skills_from_workspace` helpers moved alongside, unchanged.

use crate::agents::workspace::skill_loader;

use super::ApiContext;

pub(super) fn is_safe_skill_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.starts_with('.')
}

/// Resolve the on-disk skill directory for a skill name.
///
/// Prefer the path recorded on `Skill.skill_dir` (from SKILL.md source_path),
/// so frontmatter `name` may differ from the directory name. Fall back to
/// `workspace/skills/{name}` only when the manager has no entry.
pub(super) fn resolve_skill_dir(ctx: &ApiContext<'_>, name: &str) -> Option<std::path::PathBuf> {
    if let Some(mgr_arc) = ctx.skill_manager.get() {
        let owner = session_owner(ctx);
        if let Some(dir) = mgr_arc.read().skill_dir(name, owner.as_deref()) {
            return Some(dir.to_path_buf());
        }
    }
    ctx.workspace_dir
        .get()
        .map(|ws| ws.join("skills").join(name))
        .filter(|p| p.exists())
}

/// RFC #101: the API's owner view — resolve the connection's routing key
/// (`user_id`) to the bound FQID via UserResolver. Unbound keys resolve to
/// their fallback identity, which simply has no user-layer skills.
fn session_owner(ctx: &ApiContext<'_>) -> Option<String> {
    ctx.user_resolver.get().map(|r| r.resolve(ctx.user_id))
}

pub(super) fn reload_skills_from_workspace(ctx: &ApiContext<'_>, workspace: &std::path::Path) {
    if let Some(mgr_arc) = ctx.skill_manager.get() {
        // users root lives beside the workspace dir ({base_dir}/users);
        // shared library is left untouched (config/watcher own its lifecycle).
        let users_root = workspace.parent().map(|p| p.join("users"));
        let user_map = match users_root {
            Some(u) => skill_loader::load_all_users_skills(&u),
            None => std::collections::HashMap::new(),
        };
        let agent_defs = skill_loader::load_skills_from_dir(&workspace.join("skills"));
        mgr_arc.write().reload_user_agent_layers(user_map, agent_defs);
    }
}

pub(super) fn list(id: &str, ctx: &ApiContext<'_>) -> String {
    let owner = session_owner(ctx);
    let result: Vec<serde_json::Value> = match ctx.skill_manager.get() {
        Some(mgr_arc) => {
            let mgr = mgr_arc.read();
            mgr.skills_iter(owner.as_deref())
                .map(|(name, s)| serde_json::json!({
                    "name": name,
                    "description": s.description,
                    "keywords": s.keywords,
                }))
                .collect()
        }
        None => Vec::new(),
    };
    serde_json::json!({
        "type": "api_response",
        "id": id,
        "result": result,
    }).to_string()
}

pub(super) fn read(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let name = match params["name"].as_str() {
        Some(s) if !s.is_empty() => s,
        _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
    };
    if !is_safe_skill_name(name) {
        return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid skill name" }).to_string();
    }
    // Prefer SkillManager.skill_dir so frontmatter name may differ from directory name.
    let path = match resolve_skill_dir(ctx, name) {
        Some(dir) => dir.join("SKILL.md"),
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": format!("skill '{}' not found", name)
            })
            .to_string();
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::json!({
            "type": "api_response",
            "id": id,
            "result": { "name": name, "content": content }
        })
        .to_string(),
        Err(e) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to read skill file: {}", e)
        })
        .to_string(),
    }
}

pub(super) fn write(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let name = match params["name"].as_str() {
        Some(s) if !s.is_empty() => s,
        _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
    };
    if !is_safe_skill_name(name) {
        return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid skill name" }).to_string();
    }
    let content = params["content"].as_str().unwrap_or("");
    let Some(workspace) = ctx.workspace_dir.get() else {
        return serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string();
    };
    // Existing skills keep their real directory (name may != dir name).
    // New skills fall back to workspace/skills/{name}.
    let skill_dir = resolve_skill_dir(ctx, name)
        .unwrap_or_else(|| workspace.join("skills").join(name));
    let path = skill_dir.join("SKILL.md");
    if let Err(e) = std::fs::create_dir_all(&skill_dir) {
        return serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to create skill directory: {}", e)
        })
        .to_string();
    }
    match std::fs::write(&path, content) {
        Ok(()) => {
            reload_skills_from_workspace(ctx, workspace);
            serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string()
        }
        Err(e) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to write skill file: {}", e)
        })
        .to_string(),
    }
}

pub(super) fn delete(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let name = match params["name"].as_str() {
        Some(s) if !s.is_empty() => s,
        _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
    };
    if !is_safe_skill_name(name) {
        return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid skill name" }).to_string();
    }
    let Some(workspace) = ctx.workspace_dir.get() else {
        return serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string();
    };
    let Some(skill_dir) = resolve_skill_dir(ctx, name) else {
        return serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("skill '{}' not found", name)
        })
        .to_string();
    };
    match std::fs::remove_dir_all(&skill_dir) {
        Ok(()) => {
            reload_skills_from_workspace(ctx, workspace);
            serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string()
        }
        Err(e) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to delete skill: {}", e)
        })
        .to_string(),
    }
}
