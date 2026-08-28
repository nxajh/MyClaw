//! SkillManageTool — CRUD management for skills (6 actions).

use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::api::skill_registry::SkillRegistry;
use crate::providers::{Tool, ToolResult};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const MAX_CONTENT_CHARS: usize = 100_000;
const MAX_FILE_BYTES: usize = 1_048_576;
const ALLOWED_SUBDIRS: &[&str] = &["references", "scripts", "templates", "assets"];

pub struct SkillManageTool {
    skills: Arc<dyn SkillRegistry>,
    users_root: PathBuf,
    skills_root: PathBuf,
    agents_skills_dir: Option<PathBuf>,
}

impl SkillManageTool {
    pub fn new<R: SkillRegistry + 'static>(
        skills: Arc<R>,
        users_root: PathBuf,
        skills_root: PathBuf,
        agents_skills_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            skills,
            users_root,
            skills_root,
            agents_skills_dir,
        }
    }
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Manage skills (create, edit, patch, delete). Skills are reusable approaches for \
         recurring task types.\n\nActions: create (full SKILL.md), patch (old_string/new_string \
         — preferred for fixes), edit (full rewrite — major overhauls), delete, write_file, \
         remove_file.\n\nWrites only ever touch the user layer: editing an agent-layer or \
         shared skill automatically forks it into your personal copy (the original stays \
         untouched); deleting a shared original is refused.\n\nCreate when: complex task \
         succeeded (5+ tool calls), errors overcome, user-corrected approach worked, or user \
         asks to remember a procedure.\nUpdate when: instructions stale/wrong, missing steps \
         or pitfalls found during use.\n\nGood skills include: trigger conditions, numbered \
         steps with exact commands, pitfalls section, verification steps. Use skill_view() to \
         see format examples.\n\nConfirm with user before creating or deleting skills. Skip \
         for simple one-off tasks."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "edit", "patch", "delete", "write_file", "remove_file"],
                    "description": "Action to perform."
                },
                "name": {
                    "type": "string",
                    "description": "Skill name."
                },
                "content": {
                    "type": "string",
                    "description": "[create/edit] Full SKILL.md content including YAML frontmatter."
                },
                "old_string": {
                    "type": "string",
                    "description": "[patch] Text to find."
                },
                "new_string": {
                    "type": "string",
                    "description": "[patch] Replacement text. Use empty string to delete."
                },
                "file_path": {
                    "type": "string",
                    "description": "Path within the skill directory. For 'patch': optional, defaults to SKILL.md. For 'write_file'/'remove_file': required, must be under references/, scripts/, templates/, or assets/."
                },
                "file_content": {
                    "type": "string",
                    "description": "[write_file] Content for the supporting file."
                }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'action' is required"))?;
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'name' is required"))?;

        let result = match action {
            "create" => self.action_create(name, &args, ctx),
            "edit" => self.action_edit(name, &args, ctx),
            "patch" => self.action_patch(name, &args, ctx),
            "delete" => self.action_delete(name, ctx),
            "write_file" => self.action_write_file(name, &args, ctx),
            "remove_file" => self.action_remove_file(name, &args, ctx),
            _ => Err(format!(
                "Unknown action '{}'. Valid: create, edit, patch, delete, write_file, remove_file",
                action
            )),
        };

        match result {
            Ok(v) => Ok(ToolResult {
                success: true,
                output: v.to_string(),
                error: None,
            }),
            Err(msg) => Ok(ToolResult {
                success: false,
                output: json!({ "success": false, "error": msg }).to_string(),
                error: None,
            }),
        }
    }
}

impl SkillManageTool {
    fn action_create(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &crate::api::tool::ToolContext,
    ) -> Result<serde_json::Value, String> {
        let content = args["content"]
            .as_str()
            .ok_or("'content' is required for create")?;
        validate_name(name)?;
        validate_frontmatter(content, name)?;
        validate_content_size(content)?;

        if name == "self" {
            return Err("Cannot create skill with reserved name 'self'".to_string());
        }
        if let Some(existing) = self.skills.find(name, Some(&ctx.owner)) {
            return Err(match existing
                .skill_dir
                .as_deref()
                .and_then(|dir| self.layer_of(dir, &self.user_root(ctx)))
            {
                // A read-only original with this name exists — steer the
                // caller to the mutating actions, which auto-fork it into
                // the user layer (RFC #101 §2.6).
                Some(layer @ ("agent" | "shared")) => format!(
                    "Skill '{name}' already exists in the {layer} layer. Use patch/edit instead — \
                     it will auto-fork the skill into your user layer."
                ),
                // User layer (or unclassifiable) — the original wording.
                _ => format!(
                    "Skill '{}' already exists. Use 'edit' or 'patch' to modify it.",
                    name
                ),
            });
        }

        // Normalize FQID owner (`myclaw/u/{uuid}`) to the bare uuid directory
        // name — the users/ tree layout is `users/{uuid}/skills`.
        let user_skills_dir = self
            .users_root
            .join(crate::ids::bare_dir_name(&ctx.owner))
            .join("skills");
        let skill_dir = user_skills_dir.join(name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| format!("Failed to create skill directory: {}", e))?;

        atomic_write(&skill_dir.join("SKILL.md"), content)
            .map_err(|e| format!("Failed to write SKILL.md: {}", e))?;

        self.refresh_skills();

        Ok(json!({
            "success": true,
            "message": format!("Skill '{}' created.", name),
            "path": format!("skills/{}/SKILL.md", name),
            "skill_dir": format!("{}", skill_dir.display()),
            "hint": format!(
                "To add reference files, use skill_manage(action='write_file', name='{}', file_path='references/...', file_content='...')",
                name
            )
        }))
    }

    fn action_edit(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &crate::api::tool::ToolContext,
    ) -> Result<serde_json::Value, String> {
        let content = args["content"]
            .as_str()
            .ok_or("'content' is required for edit")?;
        validate_frontmatter(content, name)?;
        validate_content_size(content)?;

        let resolved = self.get_skill_dir(name, ctx, true)?;
        atomic_write(&resolved.dir.join("SKILL.md"), content)
            .map_err(|e| format!("Failed to write SKILL.md: {}", e))?;

        self.refresh_skills();

        Ok(with_fork_note(
            json!({
                "success": true,
                "message": format!("Skill '{}' updated.", name)
            }),
            resolved.forked_from,
        ))
    }

    fn action_patch(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &crate::api::tool::ToolContext,
    ) -> Result<serde_json::Value, String> {
        let old_string = args["old_string"]
            .as_str()
            .ok_or("'old_string' is required for patch")?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or("'new_string' is required for patch")?;
        let file_path = args["file_path"].as_str();

        let resolved = self.get_skill_dir(name, ctx, true)?;
        let skill_dir = resolved.dir;

        let target = match file_path {
            None => skill_dir.join("SKILL.md"),
            Some(fp) => {
                validate_patch_file_path(fp, &skill_dir)?;
                skill_dir.join(fp)
            }
        };
        let file_label = file_path.unwrap_or("SKILL.md");

        let current = std::fs::read_to_string(&target)
            .map_err(|e| format!("Failed to read {}: {}", file_label, e))?;

        let count = current.matches(old_string).count();
        if count == 0 {
            let preview: String = current.chars().take(500).collect();
            return Err(format!(
                "old_string not found in {}. File preview:\n{}",
                file_label, preview
            ));
        }
        if count > 1 {
            return Err(format!(
                "old_string must be unique, found {} matches in {}",
                count, file_label
            ));
        }

        let new_content = current.replacen(old_string, new_string, 1);

        if file_path.is_none() {
            validate_frontmatter(&new_content, name)?;
        }
        validate_content_size(&new_content)?;

        atomic_write(&target, &new_content)
            .map_err(|e| format!("Failed to write {}: {}", file_label, e))?;

        self.refresh_skills();

        Ok(with_fork_note(
            json!({
                "success": true,
                "message": format!("Patched 1 replacement in {}.", file_label)
            }),
            resolved.forked_from,
        ))
    }

    fn action_delete(&self, name: &str, ctx: &crate::api::tool::ToolContext) -> Result<serde_json::Value, String> {
        if name == "self" {
            return Err("Cannot delete reserved skill 'self'".to_string());
        }
        // allow_fork=false: deleting a fork that was just created is
        // meaningless — non-user-layer targets are refused outright
        // (RFC #101 §2.6).
        let resolved = self.get_skill_dir(name, ctx, false)?;
        std::fs::remove_dir_all(&resolved.dir)
            .map_err(|e| format!("Failed to delete skill directory: {}", e))?;

        self.refresh_skills();

        Ok(json!({
            "success": true,
            "message": format!("Skill '{}' deleted.", name),
            "path": format!("skills/{}/", name)
        }))
    }

    fn action_write_file(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &crate::api::tool::ToolContext,
    ) -> Result<serde_json::Value, String> {
        let file_path = args["file_path"]
            .as_str()
            .ok_or("'file_path' is required for write_file")?;
        let file_content = args["file_content"]
            .as_str()
            .ok_or("'file_content' is required for write_file")?;

        validate_supporting_file_path(file_path)?;
        validate_content_size(file_content)?;
        if file_content.len() > MAX_FILE_BYTES {
            return Err(format!(
                "File content exceeds {} MiB limit",
                MAX_FILE_BYTES / 1_048_576
            ));
        }

        let resolved = self.get_skill_dir(name, ctx, true)?;
        let skill_dir = resolved.dir;
        let target = skill_dir.join(file_path);
        if !target.starts_with(&skill_dir) {
            return Err("Path traversal not allowed.".to_string());
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directories: {}", e))?;
        }

        atomic_write(&target, file_content).map_err(|e| format!("Failed to write file: {}", e))?;

        // Auxiliary files don't affect skill metadata — no refresh needed
        // (a fork refreshed already inside get_skill_dir).
        Ok(with_fork_note(
            json!({
                "success": true,
                "message": format!("File '{}' written.", file_path),
                "path": format!("skills/{}/{}", name, file_path)
            }),
            resolved.forked_from,
        ))
    }

    fn action_remove_file(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &crate::api::tool::ToolContext,
    ) -> Result<serde_json::Value, String> {
        let file_path = args["file_path"]
            .as_str()
            .ok_or("'file_path' is required for remove_file")?;
        validate_supporting_file_path(file_path)?;

        let resolved = self.get_skill_dir(name, ctx, true)?;
        let skill_dir = resolved.dir;
        let target = skill_dir.join(file_path);
        if !target.starts_with(&skill_dir) {
            return Err("Path traversal not allowed.".to_string());
        }

        if !target.exists() {
            let available = scan_skill_files(&skill_dir);
            return Err(format!(
                "File '{}' not found in skill '{}'. Available files: {}",
                file_path,
                name,
                serde_json::to_string(&available).unwrap_or_default()
            ));
        }

        std::fs::remove_file(&target).map_err(|e| format!("Failed to remove file: {}", e))?;

        // Clean up empty parent directory (best-effort)
        if let Some(parent) = target.parent() {
            if parent != skill_dir && parent.starts_with(&skill_dir) {
                let _ = std::fs::remove_dir(parent);
            }
        }

        Ok(with_fork_note(
            json!({
                "success": true,
                "message": format!("File '{}' removed from skill '{}'.", file_path, name)
            }),
            resolved.forked_from,
        ))
    }

    /// Resolve `name` to the on-disk skill directory a write operation
    /// (edit/patch/delete/write_file/remove_file — the only callers of this
    /// method) should act on, under the **fork model** (RFC #101 §2.6,
    /// decided 2026-08-29):
    ///
    /// - target in the caller's own user layer (`users/{owner}/skills`,
    ///   owner normalized via `bare_dir_name`) → returned as-is (written
    ///   in place);
    /// - target in the agent layer (`skills_root`) or the shared library
    ///   (`agents_skills_dir`, `~/.agents/skills` — issue #83/#99) → those
    ///   originals are read-only: with `allow_fork` the skill is **lazily
    ///   forked** — fully copied into the caller's user layer (shadowing
    ///   precedence user > agent > shared makes the copy take effect) and
    ///   the copy's path is returned; `forked_from` records the source
    ///   layer so the caller can flag the fork in its response. Without
    ///   `allow_fork` (delete — forking just to delete the copy would be
    ///   meaningless) the request is refused;
    /// - registry dir outside every known root → refused (nothing writes
    ///   there by design).
    ///
    /// The leading `validate_name` also keeps the path-injection hole
    /// closed: edit/patch/delete must never reach directory resolution
    /// with unvalidated names (`../`, `/`, …).
    fn get_skill_dir(
        &self,
        name: &str,
        ctx: &crate::api::tool::ToolContext,
        allow_fork: bool,
    ) -> Result<ResolvedSkillDir, String> {
        validate_name(name)?;

        let user_root = self.user_root(ctx);

        let dir = self
            .skills
            .skill_dir(name, Some(&ctx.owner))
            .ok_or_else(|| format!("Skill '{}' not found.", name))?;

        match self.layer_of(&dir, &user_root) {
            // User layer (users/{owner}/skills) — the only layer written
            // in place. Checked before `skills_root`: the roots are not
            // guaranteed to be disjoint, and a user skill nested under
            // skills_root must still resolve as user-owned.
            Some("user") => Ok(ResolvedSkillDir {
                dir,
                forked_from: None,
            }),
            // Agent layer / shared library — read-only originals, fork
            // channel for mutating actions (RFC #101 §2.6).
            Some(layer) => {
                if !allow_fork {
                    return Err(format!(
                        "Skill '{name}' is a shared original ({layer} layer: {path}) — \
                         originals are read-only and can't be deleted via skill_manage. \
                         Delete your user-layer fork instead; the original is managed via \
                         the workspace filesystem/git.",
                        path = dir.display()
                    ));
                }
                let forked_dir = self.fork_to_user_layer(&dir, name, layer, ctx)?;
                // The copy takes over resolution (user > agent > shared):
                // refresh so subsequent operations on the same name hit
                // the user layer instead of trying to fork again.
                self.refresh_skills();
                Ok(ResolvedSkillDir {
                    dir: forked_dir,
                    forked_from: Some(layer),
                })
            }
            // Registry returned a directory outside every known layer
            // root. Nothing writes there by design — refuse rather than
            // fall through.
            None => Err(format!(
                "Skill '{name}' resolves outside your user skills directory \
                 ({}) and outside every known skills layer — refusing to write.",
                user_root.display()
            )),
        }
    }

    /// The caller's user-layer root (`users/{owner}`), with the FQID owner
    /// (`myclaw/u/{uuid}`) normalized to the bare uuid directory name —
    /// the users/ tree layout is `users/{uuid}/skills`.
    fn user_root(&self, ctx: &crate::api::tool::ToolContext) -> PathBuf {
        self.users_root.join(crate::ids::bare_dir_name(&ctx.owner))
    }

    /// Classify an on-disk skill directory by layer (RFC #101 §2.1/§2.5).
    /// User layer is checked first: the roots are not guaranteed to be
    /// disjoint, and a user skill nested under another root must still
    /// count as user-owned. Returns `None` for directories outside every
    /// known layer root.
    fn layer_of(&self, dir: &Path, user_root: &Path) -> Option<&'static str> {
        if dir.starts_with(user_root) {
            Some("user")
        } else if dir.starts_with(&self.skills_root) {
            Some("agent")
        } else if self
            .agents_skills_dir
            .as_deref()
            .is_some_and(|shared| dir.starts_with(shared))
        {
            Some("shared")
        } else {
            None
        }
    }

    /// Lazily fork a read-only original (agent layer / shared library)
    /// into the caller's user layer (RFC #101 §2.6): full recursive copy
    /// of the skill directory plus a `.fork-origin` provenance sidecar
    /// (kept out of SKILL.md — layering stays directory-authoritative).
    ///
    /// Atomic on success: the copy is built under a dot-prefixed temporary
    /// sibling (with SKILL.md written last, so the loader — which requires
    /// a SKILL.md per directory — can never pick up a half-fork even if a
    /// watcher races the copy) and renamed into place. On any failure the
    /// temporary directory is removed and the original is untouched.
    fn fork_to_user_layer(
        &self,
        src: &Path,
        name: &str,
        source_layer: &str,
        ctx: &crate::api::tool::ToolContext,
    ) -> Result<PathBuf, String> {
        let user_skills = self.user_root(ctx).join("skills");
        let dest = user_skills.join(name);
        let tmp = user_skills.join(format!(".{}.fork-tmp", name));

        std::fs::create_dir_all(&user_skills)
            .map_err(|e| format!("Failed to create user skills directory: {}", e))?;
        // Leftover from an aborted fork — never loaded (dot-prefixed, no
        // loader contract on it), safe to discard.
        let _ = std::fs::remove_dir_all(&tmp);
        if dest.exists() {
            return Err(format!(
                "A user-layer copy of skill '{name}' already exists at {dest} — \
                 the registry should have resolved it; remove it first if it is \
                 broken and you want to re-fork from the {source_layer} layer.",
                dest = dest.display()
            ));
        }

        let build = copy_dir_recursive(src, &tmp)
            .and_then(|()| write_fork_origin(&tmp, source_layer, src));
        match build {
            Ok(()) => std::fs::rename(&tmp, &dest).map_err(|e| {
                let _ = std::fs::remove_dir_all(&tmp);
                format!("Failed to move the forked skill into place: {}", e)
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(e)
            }
        }
        .map(|()| dest)
    }

    fn refresh_skills(&self) {
        // #174 收敛点：分层加载 + def→Skill 转换 + 整体替换都收在
        // SkillRegistry::reload_layered 的 agents 层实现里。
        self.skills.reload_layered(
            Some(self.users_root.as_path()),
            &self.skills_root,
            self.agents_skills_dir.as_deref(),
        );
    }
}

// ── Fork model (RFC #101 §2.6) ────────────────────────────────────────────────

/// A write operation's resolved target directory plus — when the lazy
/// fork fired — the layer the personal copy was forked from.
struct ResolvedSkillDir {
    dir: PathBuf,
    forked_from: Option<&'static str>,
}

/// Annotate a successful action's response when its target was forked
/// from a read-only original (RFC #101 §2.6): the caller must be able to
/// see the write landed on a personal copy, not the shared original.
fn with_fork_note(
    mut value: serde_json::Value,
    forked_from: Option<&'static str>,
) -> serde_json::Value {
    if let Some(layer) = forked_from {
        value["forked"] = json!(true);
        value["note"] = json!(format!(
            "Created your personal copy; the {layer}-layer original is untouched."
        ));
    }
    value
}

/// Recursively copy a skill directory (SKILL.md + every subdirectory)
/// into `dst`, which must not exist yet. SKILL.md is written last so a
/// concurrent loader/watcher scan of the destination can never see a
/// half-copied skill (the loader only considers directories with a
/// SKILL.md). Refuses sources without a SKILL.md.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("Failed to create {}: {}", dst.display(), e))?;
    let entries =
        std::fs::read_dir(src).map_err(|e| format!("Failed to read {}: {}", src.display(), e))?;
    let mut skill_md: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "SKILL.md" {
            skill_md = Some(path);
            continue;
        }
        let target = dst.join(&name);
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if path.is_file() {
            std::fs::copy(&path, &target)
                .map_err(|e| format!("Failed to copy {}: {}", path.display(), e))?;
        }
    }
    match skill_md {
        Some(md) => std::fs::copy(&md, dst.join("SKILL.md"))
            .map(|_| ())
            .map_err(|e| format!("Failed to copy {}: {}", md.display(), e)),
        None => Err(format!(
            "Source skill {} has no SKILL.md — refusing to fork a malformed skill.",
            src.display()
        )),
    }
}

/// Write the `.fork-origin` provenance sidecar for a forked copy (RFC
/// #101 §2.6): source layer, source path and fork timestamp, as JSON.
/// Lives beside SKILL.md — never inside it (directory-authoritative
/// layering, no frontmatter scope fields).
fn write_fork_origin(
    forked_dir: &Path,
    source_layer: &str,
    source_path: &Path,
) -> Result<(), String> {
    let origin = json!({
        "source_layer": source_layer,
        "source_path": source_path.display().to_string(),
        "forked_at": chrono::Local::now().to_rfc3339(),
    });
    let body = serde_json::to_string_pretty(&origin)
        .map_err(|e| format!("Failed to serialize .fork-origin: {}", e))?;
    std::fs::write(forked_dir.join(".fork-origin"), body)
        .map_err(|e| format!("Failed to write .fork-origin: {}", e))
}

// ── Validation helpers ────────────────────────────────────────────────────────

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Skill name cannot be empty.".to_string());
    }
    if name.len() > MAX_NAME_LENGTH {
        return Err(format!(
            "Skill name too long (max {} chars).",
            MAX_NAME_LENGTH
        ));
    }
    let valid = name.chars().enumerate().all(|(i, c)| {
        if i == 0 {
            c.is_ascii_alphanumeric()
        } else {
            c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'
        }
    });
    if !valid {
        return Err(format!(
            "Invalid skill name '{}'. Must match ^[a-z0-9][a-z0-9._-]*$",
            name
        ));
    }
    Ok(())
}

fn validate_frontmatter(content: &str, expected_name: &str) -> Result<(), String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err("Content must start with YAML frontmatter (---)".to_string());
    }
    let after_open = &trimmed[3..];
    let close_pos = after_open
        .find("\n---")
        .ok_or("Frontmatter is not closed with ---")?;

    let front_matter = after_open[..close_pos].trim();
    let body = after_open[close_pos + 4..].trim();

    let yaml_name = crate::str_utils::extract_yaml_string(front_matter, "name")
        .ok_or("Frontmatter must include a 'name' field")?;
    if yaml_name != expected_name {
        return Err(format!(
            "Frontmatter name '{}' does not match skill name '{}'",
            yaml_name, expected_name
        ));
    }

    let description = crate::str_utils::extract_yaml_string(front_matter, "description")
        .ok_or("Frontmatter must include a 'description' field")?;
    if description.len() > MAX_DESCRIPTION_LENGTH {
        return Err(format!(
            "description exceeds {} character limit.",
            MAX_DESCRIPTION_LENGTH
        ));
    }

    if body.is_empty() {
        return Err("Skill body cannot be empty.".to_string());
    }

    Ok(())
}

fn validate_content_size(content: &str) -> Result<(), String> {
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(format!(
            "Content exceeds {} character limit.",
            MAX_CONTENT_CHARS
        ));
    }
    Ok(())
}

fn validate_supporting_file_path(file_path: &str) -> Result<(), String> {
    if file_path.is_empty() {
        return Err("file_path cannot be empty.".to_string());
    }
    if file_path.contains("..") {
        return Err("file_path cannot contain '..'".to_string());
    }
    let first_segment = Path::new(file_path)
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or("");
    if !ALLOWED_SUBDIRS.contains(&first_segment) {
        return Err(format!(
            "file_path must start with one of: {}",
            ALLOWED_SUBDIRS.join(", ")
        ));
    }
    if Path::new(file_path).components().count() < 2 {
        return Err("file_path must include a filename (e.g. 'references/api.md')".to_string());
    }
    Ok(())
}

fn validate_patch_file_path(file_path: &str, skill_dir: &Path) -> Result<(), String> {
    if file_path.contains("..") {
        return Err("file_path cannot contain '..'".to_string());
    }
    if !skill_dir.join(file_path).starts_with(skill_dir) {
        return Err("Path traversal not allowed.".to_string());
    }
    Ok(())
}

fn atomic_write(target: &Path, content: &str) -> std::io::Result<()> {
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

fn scan_skill_files(dir: &Path) -> HashMap<String, Vec<String>> {
    let mut files = HashMap::new();
    for subdir in ALLOWED_SUBDIRS {
        let d = dir.join(subdir);
        if d.exists() {
            let mut sub_files: Vec<String> = collect_files_recursive(&d)
                .into_iter()
                .filter_map(|p| {
                    p.strip_prefix(dir)
                        .ok()
                        .map(|rel| rel.to_string_lossy().to_string())
                })
                .collect();
            if !sub_files.is_empty() {
                sub_files.sort();
                files.insert(subdir.to_string(), sub_files);
            }
        }
    }
    files
}

fn collect_files_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            result.extend(collect_files_recursive(&path));
        } else if path.is_file() {
            result.push(path);
        }
    }
    result
}
