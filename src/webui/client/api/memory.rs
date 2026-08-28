//! `memory.*` management-API route handlers, extracted verbatim from
//! `client.rs::handle_api_request` (RFC docs/webui-client-split-rfc.md,
//! batch 2: pure move — each function body is the original match-arm
//! body, unchanged; only the wrapper signature was added.)
//!
//! The `memory_scope_dir` / `memory_file_in_scope` / `memory_user_id`
//! helpers moved alongside, unchanged.

use super::ApiContext;

/// Resolve the memory directory for a scope.
/// P1-B2: single flat memory root for both scopes — ownership is a
/// frontmatter attribute (`scope` + `user_id`), not a path segment.
/// Falls back to the legacy `{workspace}/memory` when the memory root
/// handle was not installed (older embedders / tests).
pub(super) fn memory_scope_dir(
    workspace: &std::path::Path,
    scope: &str,
    uid: &str,
    memory_root: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let _ = (scope, uid);
    match memory_root {
        Some(kd) => kd.to_path_buf(),
        None => workspace.join(crate::memory::MEMORY_DIR_NAME),
    }
}

/// Whether a memory file belongs to the given scope (frontmatter-based).
/// Missing `scope` is treated as the agent layer; `scope=user` requires an
/// exact `user_id` match.
pub(super) fn memory_file_in_scope(f: &crate::memory::MemoryFile, scope: &str, uid: &str) -> bool {
    let f_scope = f.scope.as_deref().unwrap_or("agent");
    if scope == "agent" {
        f_scope == "agent"
    } else {
        f_scope == "user" && f.user_id.as_deref() == Some(uid)
    }
}

/// Resolve the request's user_id via the shared resolver (routing_key → uid).
/// Falls back to the raw routing key when no resolver is installed.
pub(super) fn memory_user_id(ctx: &ApiContext<'_>) -> String {
    ctx.user_resolver
        .get()
        .map(|r| r.resolve(ctx.user_id))
        .unwrap_or_else(|| ctx.user_id.to_string())
}

pub(super) fn list(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let scope = params["scope"].as_str().unwrap_or("all");
    match ctx.memory_root.get().cloned().or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME))) {
        Some(dir) => {
            let dir = dir.as_path();
            let uid = memory_user_id(ctx);
            // Collect (scope, file) rows; agent layer first so a
            // same-named agent entry wins dedup (matches scan_merged).
            let mut rows: Vec<(&str, crate::memory::MemoryFile)> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            if scope == "all" || scope == "agent" {
                for f in crate::memory::scan_memory_files(&memory_scope_dir(dir, "agent", &uid, Some(dir)))
                    .into_iter()
                    .filter(|f| memory_file_in_scope(f, "agent", &uid))
                {
                    if seen.insert(f.name.clone()) {
                        rows.push(("agent", f));
                    }
                }
            }
            if scope == "all" || scope == "user" {
                for f in crate::memory::scan_memory_files(&memory_scope_dir(dir, "user", &uid, Some(dir)))
                    .into_iter()
                    .filter(|f| memory_file_in_scope(f, "user", &uid))
                {
                    if seen.insert(f.name.clone()) {
                        rows.push(("user", f));
                    }
                }
            }
            let all_files: Vec<crate::memory::MemoryFile> =
                rows.iter().map(|(_, f)| f.clone()).collect();
            let backlinks = crate::memory::build_backlinks(&all_files);
            let result: Vec<serde_json::Value> = rows.iter().map(|(scope_name, f)| {
                let bl_count = backlinks.get(&f.name).map(|b| b.len()).unwrap_or(0);
                serde_json::json!({
                    "name": f.path.file_name().and_then(|n| n.to_str()).unwrap_or(&f.name).to_string(),
                    "size": std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0),
                    "mem_name": f.name,
                    "description": f.description,
                    "tags": f.tags,
                    "type": f.mem_type,
                    "inject": f.inject,
                    "scope": scope_name,
                    "link_count": f.links.len(),
                    "backlink_count": bl_count,
                    "created_at": f.created_at,
                    "updated_at": f.updated_at,
                    "content": f.content,
                })
            }).collect();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": result,
            }).to_string()
        }
        None => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": "workspace directory not configured"
        }).to_string(),
    }
}

pub(super) fn write(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let filename = match params["name"].as_str() {
        Some(s) if !s.is_empty() => s,
        _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
    };
    if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
        return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid filename" }).to_string();
    }
    let scope = match params["scope"].as_str() {
        Some("user") => "user",
        _ => "agent",
    };
    let content = params["content"].as_str().unwrap_or("");
    let dir_opt = ctx
        .memory_root
        .get()
        .cloned()
        .or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME)));
    match dir_opt {
        Some(dir) => {
            let uid = memory_user_id(ctx);
            let memory_dir = memory_scope_dir(&dir, scope, &uid, Some(&dir));
            let _ = std::fs::create_dir_all(&memory_dir);
            // P1-B2: write ownership into frontmatter. If the payload
            // already starts with frontmatter containing scope, trust
            // it as-is (raw mgmt-API write); otherwise prepend a
            // minimal ownership header before any existing frontmatter
            // keys would break parsing — simplest correct form: write
            // body-only content under a generated frontmatter.
            let path = memory_dir.join(filename);
            // Minimal header must satisfy parse_memory_file: name and
            // type are required, otherwise the file is invisible to scans.
            let stem = filename.strip_suffix(".md").unwrap_or(filename);
            let body = if content.trim_start().starts_with("---") {
                // Caller-supplied frontmatter: inject/patch scope keys.
                let trimmed = content.trim_start();
                let rest = &trimmed[3..];
                let rest = rest.trim_start_matches(['\r', '\n']);
                if let Some(end) = rest.find("\n---") {
                    let fm = &rest[..end];
                    let body = &rest[end + 4..];
                    if fm.lines().any(|l| l.trim().starts_with("scope:")) {
                        content.to_string()
                    } else {
                        let extra = if scope == "user" {
                            format!("scope: user\nuser_id: {}", uid)
                        } else {
                            "scope: agent".to_string()
                        };
                        format!("---\n{}\n{}\n---{}", fm, extra, body)
                    }
                } else {
                    content.to_string()
                }
            } else {
                let extra = if scope == "user" {
                    format!(
                        "name: {}\ntype: project\nscope: user\nuser_id: {}\n",
                        stem, uid
                    )
                } else {
                    format!("name: {}\ntype: project\nscope: agent\n", stem)
                };
                format!("---\n{}---\n\n{}", extra, content)
            };
            match std::fs::write(&path, body) {
                Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
                Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to write file: {}", e) }).to_string(),
            }
        }
        None => serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string(),
    }
}

pub(super) fn delete(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let filename = match params["name"].as_str() {
        Some(s) if !s.is_empty() => s,
        _ => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing name parameter" }).to_string(),
    };
    if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
        return serde_json::json!({ "type": "api_error", "id": id, "error": "invalid filename" }).to_string();
    }
    let scope = params["scope"].as_str();
    let dir_opt = ctx
        .memory_root
        .get()
        .cloned()
        .or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME)));
    match dir_opt {
        Some(dir) => {
            let uid = memory_user_id(ctx);
            // P1-B2: single flat dir — scope matching is done on
            // parsed frontmatter, not by path.
            let candidates = crate::memory::scan_memory_files(&memory_scope_dir(
                &dir,
                "all",
                &uid,
                Some(&dir),
            ));
            let stem = filename.strip_suffix(".md").unwrap_or(filename);
            let matches_scope = |f: &crate::memory::MemoryFile, scope_name: &str| {
                f.name == stem && memory_file_in_scope(f, scope_name, &uid)
            };
            let result = match scope {
                Some("user") => candidates
                    .iter()
                    .find(|f| matches_scope(f, "user"))
                    .map(|f| std::fs::remove_file(&f.path))
                    .unwrap_or_else(|| {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "file not found in user scope",
                        ))
                    }),
                Some("agent") => candidates
                    .iter()
                    .find(|f| matches_scope(f, "agent"))
                    .map(|f| std::fs::remove_file(&f.path))
                    .unwrap_or_else(|| {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "file not found in agent scope",
                        ))
                    }),
                _ => candidates
                    .iter()
                    .find(|f| matches_scope(f, "agent"))
                    .or_else(|| candidates.iter().find(|f| matches_scope(f, "user")))
                    .map(|f| std::fs::remove_file(&f.path))
                    .unwrap_or_else(|| {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "file not found",
                        ))
                    }),
            };
            match result {
                Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
                Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to delete file: {}", e) }).to_string(),
            }
        }
        None => serde_json::json!({ "type": "api_error", "id": id, "error": "workspace directory not configured" }).to_string(),
    }
}

pub(super) fn read(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let filename = match params["name"].as_str() {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing name parameter"
            }).to_string();
        }
    };
    // Reject path traversal attempts.
    if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
        return serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": "invalid filename"
        }).to_string();
    }
    let dir_opt = ctx
        .memory_root
        .get()
        .cloned()
        .or_else(|| ctx.workspace_dir.get().map(|ws| ws.join(crate::memory::MEMORY_DIR_NAME)));
    match dir_opt {
        Some(dir) => {
            let uid = memory_user_id(ctx);
            let scope = params["scope"].as_str();
            // P1-B2: single flat dir — scope routing via frontmatter.
            let candidates = crate::memory::scan_memory_files(&memory_scope_dir(
                &dir,
                "all",
                &uid,
                Some(&dir),
            ));
            let stem = filename.strip_suffix(".md").unwrap_or(filename);
            let matches_scope = |f: &crate::memory::MemoryFile, scope_name: &str| {
                f.name == stem && memory_file_in_scope(f, scope_name, &uid)
            };
            let found = match scope {
                Some("user") => candidates
                    .iter()
                    .find(|f| matches_scope(f, "user"))
                    .map(|f| ("user".to_string(), std::fs::read_to_string(&f.path).ok())),
                Some("agent") => candidates
                    .iter()
                    .find(|f| matches_scope(f, "agent"))
                    .map(|f| ("agent".to_string(), std::fs::read_to_string(&f.path).ok())),
                // Backwards-compatible default: agent layer first,
                // fall back to the per-user layer.
                _ => candidates
                    .iter()
                    .find(|f| matches_scope(f, "agent"))
                    .or_else(|| candidates.iter().find(|f| matches_scope(f, "user")))
                    .map(|f| {
                        let s = if memory_file_in_scope(f, "agent", &uid) {
                            "agent"
                        } else {
                            "user"
                        };
                        (s.to_string(), std::fs::read_to_string(&f.path).ok())
                    }),
            }
            .and_then(|(s, content)| content.map(|c| (s, c)));
            match found {
                Some((s, content)) => serde_json::json!({
                    "type": "api_response",
                    "id": id,
                    "result": { "name": filename, "content": content, "scope": s }
                }).to_string(),
                None => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to read file: {}", filename)
                }).to_string(),
            }
        }
        None => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": "workspace directory not configured"
        }).to_string(),
    }
}
