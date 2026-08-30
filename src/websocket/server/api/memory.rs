//! `memory.*` management-API route handlers, extracted verbatim from
//! `client.rs::handle_api_request` (RFC docs/websocket-client-split-rfc.md,
//! batch 2: pure move — each function body is the original match-arm
//! body, unchanged; only the wrapper signature was added.)
//!
//! The `memory_scope_dir` / `memory_file_in_scope` / `memory_user_id`
//! helpers moved alongside. P4 (RFC #101 §6): directory layout is layered
//! ({base}/memory agent, {base}/users/{uuid}/memory user), mirroring the
//! memory-tool reader implementation.

use super::ApiContext;

/// Resolve the memory directory for a scope.
/// P4: agent scope → the memory root itself; user scope →
/// `{base}/users/{bare uuid}/memory` (mirrors memory_tool::reader).
/// Falls back to the legacy `{workspace}/memory` when the memory root
/// handle was not installed (older embedders / tests).
pub(super) fn memory_scope_dir(
    workspace: &std::path::Path,
    scope: &str,
    uid: &str,
    memory_root: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let base = match memory_root {
        Some(kd) => kd.to_path_buf(),
        None => workspace.join(crate::memory::MEMORY_DIR_NAME),
    };
    if scope == "agent" {
        base
    } else {
        crate::memory::user_memory_dir(&base, uid)
    }
}

/// Whether a memory file belongs to the given scope. The layered scan
/// helpers normalize `scope`/`user_id` (incl. the agent-dir frontmatter
/// fallback for pre-migration single-pool entries), so membership is
/// decided from the frontmatter fields alone.
pub(super) fn memory_file_in_scope(f: &crate::memory::MemoryFile, scope: &str, uid: &str) -> bool {
    let f_scope = f.scope.as_deref().unwrap_or("agent");
    if scope == "agent" {
        f_scope == "agent"
    } else {
        f_scope == "user" && f.user_id.as_deref() == Some(uid)
    }
}

/// Both layers for scope-agnostic lookups: agent layer + this user's user
/// layer (which itself includes pre-migration fallback entries still in
/// the agent dir). User layer first — same-name shadowing is user>agent
/// (RFC §6.2), matching what the memory tools display.
fn scan_both_layers(
    base: &std::path::Path,
    uid: &str,
) -> std::io::Result<Vec<crate::memory::MemoryFile>> {
    let mut candidates = crate::memory::scan_user_layer(base, uid)?;
    candidates.extend(crate::memory::scan_agent_layer(base)?);
    Ok(candidates)
}

/// Layer-scan I/O failure: surface an api_error instead of pretending the
/// layer is empty.
fn scan_api_error(id: &str, e: std::io::Error) -> String {
    serde_json::json!({
        "type": "api_error",
        "id": id,
        "error": format!("memory scan failed: {}", e)
    })
    .to_string()
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
            // Collect (scope, file) rows; user layer first so a same-named
            // user entry wins dedup (user>agent shadowing, matches
            // scan_merged_for_user).
            let mut rows: Vec<(&str, crate::memory::MemoryFile)> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            if scope == "all" || scope == "user" {
                let files = match crate::memory::scan_user_layer(dir, &uid) {
                    Ok(files) => files,
                    Err(e) => return scan_api_error(id, e),
                };
                for f in files
                    .into_iter()
                    .filter(|f| memory_file_in_scope(f, "user", &uid))
                {
                    if seen.insert(f.name.clone()) {
                        rows.push(("user", f));
                    }
                }
            }
            if scope == "all" || scope == "agent" {
                let files = match crate::memory::scan_agent_layer(dir) {
                    Ok(files) => files,
                    Err(e) => return scan_api_error(id, e),
                };
                for f in files
                    .into_iter()
                    .filter(|f| memory_file_in_scope(f, "agent", &uid))
                {
                    if seen.insert(f.name.clone()) {
                        rows.push(("agent", f));
                    }
                }
            }
            let all_files: Vec<crate::memory::MemoryFile> =
                rows.iter().map(|(_, f)| f.clone()).collect();
            let backlinks = crate::memory::build_backlinks(&all_files);
            let result: Vec<serde_json::Value> = rows.iter().map(|(scope_name, f)| {
                let bl_count = backlinks
                    .get(&crate::memory::layer_qualified_name(f))
                    .map(|b| b.len())
                    .unwrap_or(0);
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
            // P4: layered dirs — candidates span both layers, scope
            // matching on the normalized frontmatter.
            let candidates = match scan_both_layers(&dir, &uid) {
                Ok(c) => c,
                Err(e) => return scan_api_error(id, e),
            };
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
                // Default: user layer first (user>agent shadowing).
                _ => candidates
                    .iter()
                    .find(|f| matches_scope(f, "user"))
                    .or_else(|| candidates.iter().find(|f| matches_scope(f, "agent")))
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
            // P4: layered dirs — candidates span both layers, scope
            // routing via the normalized frontmatter.
            let candidates = match scan_both_layers(&dir, &uid) {
                Ok(c) => c,
                Err(e) => return scan_api_error(id, e),
            };
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
                // Default: user layer first (user>agent shadowing —
                // matches what the memory tools display), agent fallback.
                // Default: user layer first (user>agent shadowing).
                _ => candidates
                    .iter()
                    .find(|f| matches_scope(f, "user"))
                    .or_else(|| candidates.iter().find(|f| matches_scope(f, "agent")))
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

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "myclaw/u/018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e7f";

    fn write_file(dir: &std::path::Path, name: &str, fm: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.md", name)),
            format!(
                "---\nname: {}\n{}\ntype: project\ninject: search\ncreated_at: 2026-08-30\n---\n\nbody",
                name, fm
            ),
        )
        .unwrap();
    }

    #[test]
    fn memory_scope_dir_layers_by_scope() {
        let ws = std::path::Path::new("/tmp/myclaw_ws_test_scope_dir");
        let memory_root = ws.join("memory");
        assert_eq!(
            memory_scope_dir(ws, "agent", UID, Some(&memory_root)),
            memory_root
        );
        assert_eq!(
            memory_scope_dir(ws, "user", UID, Some(&memory_root)),
            ws.join("users").join("018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e7f").join("memory")
        );
        // Legacy fallback without an installed memory root handle.
        assert_eq!(
            memory_scope_dir(ws, "user", UID, None),
            ws.join("users").join("018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e7f").join("memory")
        );
    }

    #[test]
    fn list_and_scope_filtering_across_layers() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let memory_root = base.join("memory");
        std::fs::create_dir_all(&memory_root).unwrap();

        // Agent-layer entry + a pooled legacy user entry still in the agent
        // dir (transition) + a real user-layer entry.
        write_file(&memory_root, "shared-rule", "scope: agent");
        write_file(&memory_root, "pooled-user", &format!("scope: user\nuser_id: {}", UID));
        let user_dir = crate::memory::user_memory_dir(&memory_root, UID);
        write_file(&user_dir, "user-note", &format!("scope: user\nuser_id: {}", UID));

        let candidates = scan_both_layers(&memory_root, UID);
        let in_scope = |scope: &str| {
            candidates
                .iter()
                .filter(|f| memory_file_in_scope(f, scope, UID))
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(in_scope("agent"), vec!["shared-rule"]);
        let user_names = in_scope("user");
        assert!(user_names.contains(&"user-note"));
        assert!(user_names.contains(&"pooled-user"), "frontmatter fallback must keep pooled user entries user-layer");
        assert_eq!(user_names.len(), 2);
    }
}
