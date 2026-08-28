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
         remove_file.\n\nCreate when: complex task succeeded (5+ tool calls), errors overcome, \
         user-corrected approach worked, or user asks to remember a procedure.\nUpdate when: \
         instructions stale/wrong, missing steps or pitfalls found during use.\n\nGood skills \
         include: trigger conditions, numbered steps with exact commands, pitfalls section, \
         verification steps. Use skill_view() to see format examples.\n\nConfirm with user \
         before creating or deleting skills. Skip for simple one-off tasks."
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
        if self.skills.find(name, Some(&ctx.owner)).is_some() {
            return Err(format!(
                "Skill '{}' already exists. Use 'edit' or 'patch' to modify it.",
                name
            ));
        }

        let user_skills_dir = self.users_root.join(&ctx.owner).join("skills");
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

        let skill_dir = self.get_skill_dir(name, ctx)?;
        atomic_write(&skill_dir.join("SKILL.md"), content)
            .map_err(|e| format!("Failed to write SKILL.md: {}", e))?;

        self.refresh_skills();

        Ok(json!({
            "success": true,
            "message": format!("Skill '{}' updated.", name)
        }))
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

        let skill_dir = self.get_skill_dir(name, ctx)?;

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

        Ok(json!({
            "success": true,
            "message": format!("Patched 1 replacement in {}.", file_label)
        }))
    }

    fn action_delete(&self, name: &str, ctx: &crate::api::tool::ToolContext) -> Result<serde_json::Value, String> {
        if name == "self" {
            return Err("Cannot delete reserved skill 'self'".to_string());
        }
        let skill_dir = self.get_skill_dir(name, ctx)?;
        std::fs::remove_dir_all(&skill_dir)
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

        let skill_dir = self.get_skill_dir(name, ctx)?;
        let target = skill_dir.join(file_path);
        if !target.starts_with(&skill_dir) {
            return Err("Path traversal not allowed.".to_string());
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directories: {}", e))?;
        }

        atomic_write(&target, file_content).map_err(|e| format!("Failed to write file: {}", e))?;

        // Auxiliary files don't affect skill metadata — no refresh needed.
        Ok(json!({
            "success": true,
            "message": format!("File '{}' written.", file_path),
            "path": format!("skills/{}/{}", name, file_path)
        }))
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

        let skill_dir = self.get_skill_dir(name, ctx)?;
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

        Ok(json!({
            "success": true,
            "message": format!("File '{}' removed from skill '{}'.", file_path, name)
        }))
    }

    /// Resolve `name` to its on-disk skill directory for a write operation
    /// (edit/patch/delete/write_file/remove_file — the only callers of this
    /// method). Rejects a skill sourced from `agents_skills_dir` (issue #83's
    /// cross-agent shared library, `~/.agents/skills`): that library is
    /// read-only by design (`skill_manage` only ever writes under
    /// `skills_root`), but before this check `get_skill_dir` returned the
    /// shared path anyway and every caller happily wrote/deleted through it
    /// (issue #93 — filing this as "should say read-only instead of 'not
    /// found'" undersold it: nothing ever produced "not found" for a
    /// shared-only skill, the writes/deletes silently succeeded against the
    /// shared library).
    fn get_skill_dir(&self, name: &str, ctx: &crate::api::tool::ToolContext) -> Result<PathBuf, String> {
        let dir = self.skills.skill_dir(name, Some(&ctx.owner))
            .ok_or_else(|| format!("Skill '{}' not found.", name))?;

        if let Some(shared) = &self.agents_skills_dir {
            if dir.starts_with(shared) {
                return Err(format!(
                    "Skill '{name}' comes from the shared library ({}) and is read-only \
                     here — it can't be edited, patched, deleted, or have files added/removed \
                     via skill_manage.",
                    shared.display()
                ));
            }
        }

        Ok(dir)
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
