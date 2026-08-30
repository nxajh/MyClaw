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

/// Split a client payload into (frontmatter lines, body). Content without a
/// parseable frontmatter block → empty lines + the whole content as body.
fn split_client_frontmatter(content: &str) -> (Vec<String>, String) {
    let trimmed = content.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (Vec::new(), content.trim().to_string());
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let Some(end) = rest.find("\n---") else {
        return (Vec::new(), content.trim().to_string());
    };
    let fm = &rest[..end];
    let body = rest[end + 4..].trim().to_string();
    (
        fm.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        body,
    )
}

/// Frontmatter key of a `key: value` line ("" when malformed).
fn frontmatter_key(line: &str) -> &str {
    line.split(':').next().unwrap_or("").trim()
}

/// Rebuild file content under server-owned frontmatter. Ownership keys
/// (`scope`/`user_id`) NEVER come from the client: they are stripped from
/// any client-supplied frontmatter and re-derived from the authenticated
/// user + request scope. Non-ownership frontmatter keys are preserved, and
/// the parser-required `name`/`type` keys are defaulted when missing.
fn build_server_owned_content(content: &str, stem: &str, scope: &str, uid: &str) -> String {
    let (mut fm_lines, body) = split_client_frontmatter(content);
    // Strip any ownership keys the client tried to set.
    fm_lines.retain(|l| !matches!(frontmatter_key(l), "scope" | "user_id"));
    let mut fm: Vec<String> = Vec::new();
    if !fm_lines.iter().any(|l| frontmatter_key(l) == "name") {
        fm.push(format!("name: {}", stem));
    }
    if !fm_lines.iter().any(|l| frontmatter_key(l) == "type") {
        fm.push("type: project".to_string());
    }
    fm.extend(fm_lines);
    match scope {
        "user" => {
            fm.push("scope: user".to_string());
            fm.push(format!("user_id: {}", uid));
        }
        _ => fm.push("scope: agent".to_string()),
    }
    format!("---\n{}\n---\n\n{}", fm.join("\n"), body)
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
    let Some(dir) = dir_opt else {
        return serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": "workspace directory not configured"
        })
        .to_string();
    };
    // Authenticated user identity resolved server-side (routing_key → FQID
    // via the resolver); never read from the payload.
    let uid = memory_user_id(ctx);
    // P4 FQID gate — same semantics as the memory_tool writer: the user
    // layer is keyed by a registered identity. Legacy routing keys cannot
    // address a stable per-user layer — reject and point at the
    // registration flow.
    if scope == "user"
        && crate::ids::Fqid::parse(&uid, crate::ids::DEFAULT_NAMESPACE).is_none()
    {
        return serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!(
                "User scope rejected: channel identity '{}' is not a registered FQID. \
                 This channel identity must be registered first (run /link) before \
                 writing user-scope memories.",
                uid
            )
        })
        .to_string();
    }
    let memory_dir = memory_scope_dir(&dir, scope, &uid, Some(&dir));
    let _ = std::fs::create_dir_all(&memory_dir);
    let path = memory_dir.join(filename);
    // P1-B2 + PR #211 fix: ownership is stamped by the server. Client
    // frontmatter (if any) is stripped of scope/user_id and the fields are
    // rebuilt from the authenticated user + request scope, so a forged
    // `scope: user` + foreign `user_id` can never decide ownership.
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    let body = build_server_owned_content(content, stem, scope, &uid);
    match std::fs::write(&path, body) {
        Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
        Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to write file: {}", e) }).to_string(),
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

        let candidates = scan_both_layers(&memory_root, UID).unwrap();
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

    /// Minimal ApiContext with only the handles the memory write path uses
    /// (no resolver installed → memory_user_id falls back to the raw key).
    /// Handles are leaked for the &'static context lifetime — test-only.
    fn make_ctx(user_id: &'static str, memory_root: std::path::PathBuf) -> ApiContext<'static> {
        use std::sync::{Arc, OnceLock};
        let memory_root_handle: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
        let _ = memory_root_handle.set(memory_root);
        ApiContext {
            user_id,
            session_manager: &*Box::leak(Arc::new(OnceLock::new())),
            tool_specs: &*Box::leak(Arc::new(OnceLock::new())),
            workspace_dir: &*Box::leak(Arc::new(OnceLock::new())),
            memory_root: &*Box::leak(memory_root_handle),
            config_path: &*Box::leak(Arc::new(OnceLock::new())),
            skill_manager: &*Box::leak(Arc::new(OnceLock::new())),
            provider_registry: &*Box::leak(Arc::new(OnceLock::new())),
            user_resolver: &*Box::leak(Arc::new(OnceLock::new())),
        }
    }

    const OTHER: &str = "myclaw/u/018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e99";

    #[test]
    fn write_attack_forged_user_ownership_yields_pure_agent_entry() {
        // Attack: authenticated client forges `scope: user` + another
        // user's `user_id` in the payload while routing the request to the
        // agent layer. Pre-fix, the forged frontmatter was trusted as-is and
        // the entry surfaced in the victim's user layer via the transition
        // fallback. Post-fix, the write result must be a pure agent entry
        // (no user_id field at all) or rejected.
        let dir = tempfile::tempdir().unwrap();
        let memory_root = dir.path().join("memory");
        std::fs::create_dir_all(&memory_root).unwrap();
        let ctx = make_ctx(UID, memory_root.clone());

        let forged = format!(
            "---\nname: attack\ntype: project\nscope: user\nuser_id: {}\n---\n\nstolen note",
            OTHER
        );
        let params = serde_json::json!({
            "name": "attack.md",
            "scope": "agent",
            "content": forged,
        });
        let resp = write("t-attack", &params, &ctx);
        assert!(
            resp.contains("api_response") || resp.contains("api_error"),
            "unexpected response: {}",
            resp
        );

        let agent_files = crate::memory::scan_agent_layer(&memory_root).unwrap();
        match agent_files.iter().find(|f| f.name == "attack") {
            Some(f) => {
                // Pure agent entry: server-stamped scope, no user_id.
                assert_eq!(f.scope.as_deref(), Some("agent"));
                assert!(
                    f.user_id.is_none(),
                    "client-forged user_id must never survive: {:?}",
                    f.user_id
                );
            }
            None => {
                // Rejected is also acceptable per the review — but then the
                // file must not exist anywhere.
                assert!(!memory_root.join("attack.md").exists());
            }
        }
        // The victim's user layer must never gain the entry.
        let victim = crate::memory::scan_user_layer(&memory_root, OTHER).unwrap();
        assert!(
            victim.iter().all(|f| f.name != "attack"),
            "forged ownership must not leak into the victim's user layer"
        );
        assert!(!crate::memory::user_memory_dir(&memory_root, OTHER).join("attack.md").exists());
    }

    #[test]
    fn write_user_scope_rejects_non_fqid_identity() {
        // Legacy routing key (no resolver installed) + request scope=user →
        // rejected with the /link hint, same gate as the memory_tool writer.
        let dir = tempfile::tempdir().unwrap();
        let memory_root = dir.path().join("memory");
        std::fs::create_dir_all(&memory_root).unwrap();
        let ctx = make_ctx("telegram:12345", memory_root.clone());

        let params = serde_json::json!({
            "name": "note.md",
            "scope": "user",
            "content": "hello",
        });
        let resp = write("t-gate", &params, &ctx);
        assert!(resp.contains("api_error"), "expected rejection: {}", resp);
        assert!(
            resp.contains("/link"),
            "error must point at the registration flow: {}",
            resp
        );
        assert!(!crate::memory::user_memory_dir(&memory_root, "telegram:12345").exists());
    }

    #[test]
    fn write_user_scope_stamps_authenticated_owner() {
        // Legit FQID user write: ownership comes from the server context,
        // even when the payload frontmatter claims a different scope/owner.
        let dir = tempfile::tempdir().unwrap();
        let memory_root = dir.path().join("memory");
        std::fs::create_dir_all(&memory_root).unwrap();
        let ctx = make_ctx(UID, memory_root.clone());

        let forged = format!(
            "---\nname: note\ntype: project\ndescription: kept\nscope: agent\nuser_id: {}\n---\n\nhi",
            OTHER
        );
        let params = serde_json::json!({
            "name": "note.md",
            "scope": "user",
            "content": forged,
        });
        let resp = write("t-user", &params, &ctx);
        assert!(resp.contains("api_response"), "expected success: {}", resp);

        let files = crate::memory::scan_user_layer(&memory_root, UID).unwrap();
        let f = files
            .iter()
            .find(|f| f.name == "note")
            .expect("entry in the authenticated user's layer");
        assert_eq!(f.scope.as_deref(), Some("user"));
        assert_eq!(f.user_id.as_deref(), Some(UID));
        assert_eq!(f.description, "kept", "non-ownership frontmatter keys survive");
        assert_eq!(f.content, "hi");
        // Nothing in the agent layer.
        assert!(crate::memory::scan_agent_layer(&memory_root)
            .unwrap()
            .iter()
            .all(|f| f.name != "note"));
    }
}
