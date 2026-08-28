//! memory_search / memory_manage tools.

use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::audit::{
    append_memory_audit, archive_memory_version, scan_agent_pii_opt, scan_memory_content_opt,
    short_sha256, MemoryAudit,
};
use super::format::{
    atomic_write, build_frontmatter, link_values, lint_memory_content, scan_merged,
    validate_body_only, validate_name,
};
use super::reader::{resolve_scope, scan_scope, scope_memory_dir, user_id_for};
use super::search::{best_snippet, memory_search_score, normalize_optional_filter, query_tokens};
use crate::api::tool::ToolContext;
use crate::identity::user_profile::UserResolver;
use crate::memory::build_backlinks;
use crate::providers::{Tool, ToolResult};

const MAX_CONTENT_CHARS: usize = 10_000;
const MAX_DESCRIPTION_CHARS: usize = 500;

// ══════════════════════════════════════════════════════════════════════════
// memory_search
// ══════════════════════════════════════════════════════════════════════════

pub struct MemorySearchTool {
    memory_root: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemorySearchTool {
    pub fn new(memory_root: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            memory_root,
            resolver,
        }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search memory entries by keyword. Searches across name, description, tags, content, link labels, and link targets. Returns matching entries with relevance info."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. Matches against name, description, tags, content, link labels, and link targets."
                },
                "memory_type": {
                    "type": "string",
                    "description": "Optional type filter. Omit, pass empty string, or whitespace to search all types."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return (default 20, max 100)."
                },
                "include_related": {
                    "type": "boolean",
                    "description": "If true, include outgoing links and backlinks for each direct result."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let query = match args["query"].as_str() {
            Some(q) => q.trim().to_lowercase(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: json!({"success": false, "error": "'query' is required."}).to_string(),
                    error: None,
                });
            }
        };
        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": "'query' cannot be empty."}).to_string(),
                error: None,
            });
        }
        let tokens = query_tokens(&query);
        let type_filter = normalize_optional_filter(args["memory_type"].as_str());
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        let include_related = args["include_related"].as_bool().unwrap_or(false);

        let user_id = user_id_for(ctx, &self.resolver);
        let files = scan_merged(&self.memory_root, &user_id);

        let mut results: Vec<(i32, &crate::memory::MemoryFile)> = Vec::new();
        for mf in &files {
            if let Some(ref wanted) = type_filter {
                if mf.mem_type.to_lowercase() != *wanted {
                    continue;
                }
            }
            let score = memory_search_score(mf, &query, &tokens);
            if score > 0 {
                results.push((score, mf));
            }
        }

        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "success": true,
                    "count": 0,
                    "results": [],
                    "hint": "No matching memories. Try different keywords or use memory_list() to browse all."
                }).to_string(),
                error: None,
            });
        }

        results.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        results.truncate(limit);

        let backlinks = build_backlinks(&files);
        let json_results: Vec<serde_json::Value> = results
            .iter()
            .map(|(score, mf)| {
                let snippet = best_snippet(mf, &query, &tokens);

                let outgoing = link_values(&mf.links);
                let file_backlinks = backlinks.get(&mf.name).cloned().unwrap_or_default();
                let mut related = Vec::new();
                if include_related {
                    for link in &mf.links {
                        related.push(
                            json!({ "direction": "out", "name": link.target, "label": link.label }),
                        );
                    }
                    for backlink in &file_backlinks {
                        related.push(json!({ "direction": "in", "name": backlink }));
                    }
                }

                let mut result = json!({
                    "name": &mf.name,
                    "type": &mf.mem_type,
                    "description": &mf.description,
                    "snippet": snippet,
                    "relevance": score,
                    "links": outgoing,
                    "backlinks": file_backlinks,
                });
                if include_related {
                    result["related"] = json!(related);
                }
                if !mf.tags.is_empty() {
                    result["tags"] = json!(mf.tags);
                }
                result
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: json!({
                "success": true,
                "count": json_results.len(),
                "results": json_results,
            })
            .to_string(),
            error: None,
        })
    }
}

// ══════════════════════════════════════════════════════════════════════════
// memory_manage
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryManageTool {
    memory_root: PathBuf,
    base_dir: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemoryManageTool {
    pub fn new(memory_root: PathBuf, base_dir: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            memory_root,
            base_dir,
            resolver,
        }
    }
}

#[async_trait]
impl Tool for MemoryManageTool {
    fn name(&self) -> &str {
        "memory_manage"
    }

    fn description(&self) -> &str {
        "Manage persistent memories. Actions: add (create new entry), replace (update existing), \
         remove (delete entry).\n\nUse add when: user asks to remember something, you discover a \
         stable preference or fact.\nUse replace when: existing memory needs updating.\n\
         Use remove when: memory is stale or wrong.\n\nConfirm with user before removing memories.\n\n\
         Scopes: 'user' (default) stores the memory in the current user's private layer; 'agent' \
         stores it in the shared agent layer (cross-user methodology/processes/rules, must be \
         de-identified). Use 'agent' only for generalizable knowledge without user-specific \
         identifiers."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "replace", "remove"],
                    "description": "Action to perform."
                },
                "scope": {
                    "type": "string",
                    "enum": ["user", "agent"],
                    "description": "Target layer: 'user' (default) = this user's private memory; \
                     'agent' = shared cross-user layer (de-identified only)."
                },
                "name": {
                    "type": "string",
                    "description": "Memory entry name (used as identifier). Lowercase, hyphens allowed."
                },
                "content": {
                    "type": "string",
                    "description": "Memory content. Required for add and replace. BODY ONLY: plain markdown content, no YAML frontmatter and no `---` blocks. When applicable, end with `## See Also` and 1-3 links in canonical form: `[Related: other_memory_name](other_memory_name.md)` — href must be `<name>.md` (not bare name, not a path)."
                },
                "memory_type": {
                    "type": "string",
                    "description": "Semantic category (user, feedback, rule, project, reference, or custom). \
                     Does NOT control injection — use the `inject` field for that. Default: project."
                },
                "inject": {
                    "type": "string",
                    "enum": ["always", "search"],
                    "description": "Injection policy. `always`: description injected into every conversation's \
                     system-reminder (use for behavioral rules, personality traits, communication preferences \
                     that affect every interaction). `search`: available via memory_search only, never \
                     auto-injected (use for technical gotchas, situational facts, project context). \
                     Default: search."
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of the key content (1-2 sentences). \
                     Required for inject=always (injected into system prompt). \
                     Auto-generated from content if omitted. Formerly known as 'summary'."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tags for filtering and categorization, e.g. [\"rust\", \"qqbot\", \"bug\"]."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Required and must be true for action='remove'. Only remove after explicit user confirmation."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional short reason for audit logging."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model identifier for audit logging when a background agent writes memory."
                }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        let user_id = user_id_for(ctx, &self.resolver);

        let result = match action {
            "add" => self.action_add(name, &args, &user_id, ctx),
            "replace" => self.action_replace(name, &args, &user_id, ctx),
            "remove" => self.action_remove(name, &args, &user_id, ctx),
            _ => Err(format!(
                "Unknown action '{}'. Use: add, replace, remove",
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
                output: json!({"success": false, "error": msg}).to_string(),
                error: None,
            }),
        }
    }
}

impl MemoryManageTool {
    fn action_add(
        &self,
        name: &str,
        args: &serde_json::Value,
        user_id: &str,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        validate_name(name)?;
        let scope = resolve_scope(args);
        // P1-B2: user-scope entries carry ownership in frontmatter — without a
        // resolvable user_id they would be unreadable after the write.
        if scope == "user" && user_id.trim().is_empty() {
            return Err("User scope requires a resolvable user_id.".to_string());
        }

        let content = args["content"]
            .as_str()
            .ok_or("'content' is required for add.")?;
        if content.trim().is_empty() {
            return Err("Content cannot be empty.".to_string());
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!(
                "Content exceeds {} character limit.",
                MAX_CONTENT_CHARS
            ));
        }

        validate_body_only(content)?;
        scan_memory_content_opt(content)?;
        if scope == "agent" {
            scan_agent_pii_opt(content)?;
        }

        // P1-B2: flat dir — names are unique across scopes; an add that
        // collides with an existing name owned by another scope must be
        // rejected instead of silently overwriting the file.
        let all_files = crate::memory::scan_memory_files(&self.memory_root);
        if let Some(existing) = all_files.iter().find(|f| f.name == name) {
            let existing_scope = existing.scope.as_deref().unwrap_or("agent");
            return Err(format!(
                "Memory '{}' already exists in the {} scope. Use 'replace' to update it.",
                name, existing_scope
            ));
        }

        let mem_type = self.resolve_type(args);
        let inject = self.resolve_inject(args);
        let description = self.resolve_description(args, content);
        let tags = self.resolve_tags(args);
        let filename = format!("{}.md", name);
        let now = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let warnings = lint_memory_content(name, content, &all_files);
        let frontmatter = build_frontmatter(
            name,
            &description,
            &tags,
            &mem_type,
            &inject,
            &now,
            None,
            scope,
            Some(user_id),
        );
        let file_content = format!("{}{}", frontmatter, content);

        let target = scope_memory_dir(&self.memory_root, scope, user_id).join(&filename);
        // Ensure the memory dir exists
        let _ = std::fs::create_dir_all(target.parent().unwrap_or(&target));
        atomic_write(&target, &file_content)
            .map_err(|e| format!("Failed to write memory file: {}", e))?;
        append_memory_audit(
            &self.base_dir,
            ctx,
            MemoryAudit {
                user_id,
                scope,
                action: "add",
                name,
                old_hash: None,
                new_hash: Some(short_sha256(&file_content)),
                args,
            },
        );

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' added.", name),
            "name": name,
            "type": &mem_type,
            "description": &description,
            "tags": tags,
            "warnings": warnings,
        }))
    }

    fn action_replace(
        &self,
        name: &str,
        args: &serde_json::Value,
        user_id: &str,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        validate_name(name)?;
        let scope = resolve_scope(args);

        let files = scan_scope(&self.memory_root, scope, user_id);
        let existing = files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| format!("Memory '{}' not found in the {} scope.", name, scope))?;
        let old_content = std::fs::read_to_string(&existing.path).ok();
        let old_hash = old_content.as_deref().map(short_sha256);

        let content = args["content"]
            .as_str()
            .ok_or("'content' is required for replace.")?;
        if content.trim().is_empty() {
            return Err("Content cannot be empty.".to_string());
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!(
                "Content exceeds {} character limit.",
                MAX_CONTENT_CHARS
            ));
        }

        validate_body_only(content)?;
        scan_memory_content_opt(content)?;
        if scope == "agent" {
            scan_agent_pii_opt(content)?;
        }

        // Preserve existing metadata unless overridden
        let mem_type = normalize_optional_filter(args["memory_type"].as_str())
            .unwrap_or_else(|| existing.mem_type.clone());
        let inject = match args["inject"].as_str() {
            Some("always") => "always".to_string(),
            Some("search") => "search".to_string(),
            _ => existing.inject.clone(),
        };
        let description =
            if args["description"].as_str().is_some() || args["summary"].as_str().is_some() {
                self.resolve_description(args, content)
            } else {
                existing.description.clone()
            };
        let tags = if args["tags"].is_array() {
            self.resolve_tags(args)
        } else {
            existing.tags.clone()
        };
        let now = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let warnings = lint_memory_content(name, content, &files);
        let frontmatter = build_frontmatter(
            name,
            &description,
            &tags,
            &mem_type,
            &inject,
            &existing.created_at,
            Some(&now),
            scope,
            Some(user_id),
        );
        let file_content = format!("{}{}", frontmatter, content);

        // Write to the same location as the existing file.
        let target = existing.path.clone();

        // Archive the previous version before overwriting.
        if let Some(ref old) = old_content {
            let memory_dir = target.parent().unwrap_or(Path::new("."));
            archive_memory_version(memory_dir, name, old);
        }

        atomic_write(&target, &file_content)
            .map_err(|e| format!("Failed to write memory file: {}", e))?;
        append_memory_audit(
            &self.base_dir,
            ctx,
            MemoryAudit {
                user_id,
                scope,
                action: "replace",
                name,
                old_hash,
                new_hash: Some(short_sha256(&file_content)),
                args,
            },
        );

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' updated.", name),
            "name": name,
            "warnings": warnings,
        }))
    }

    fn action_remove(
        &self,
        name: &str,
        args: &serde_json::Value,
        user_id: &str,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        validate_name(name)?;
        let scope = resolve_scope(args);
        if args["confirm"].as_bool() != Some(true) {
            return Err(
                "Removing memory requires confirm=true after explicit user confirmation."
                    .to_string(),
            );
        }

        let files = scan_scope(&self.memory_root, scope, user_id);
        let existing = files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| format!("Memory '{}' not found in the {} scope.", name, scope))?;
        let old_hash = std::fs::read_to_string(&existing.path)
            .ok()
            .map(|content| short_sha256(&content));

        std::fs::remove_file(&existing.path)
            .map_err(|e| format!("Failed to remove memory file: {}", e))?;
        append_memory_audit(
            &self.base_dir,
            ctx,
            MemoryAudit {
                user_id,
                scope,
                action: "remove",
                name,
                old_hash,
                new_hash: None,
                args,
            },
        );

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' removed.", name),
        }))
    }

    fn resolve_type(&self, args: &serde_json::Value) -> String {
        normalize_optional_filter(args["memory_type"].as_str())
            .unwrap_or_else(|| "project".to_string())
    }

    fn resolve_inject(&self, args: &serde_json::Value) -> String {
        match args["inject"].as_str() {
            Some("always") => "always".to_string(),
            _ => "search".to_string(),
        }
    }

    /// Resolve description: explicit parameter, or auto-generate from content.
    fn resolve_description(&self, args: &serde_json::Value, content: &str) -> String {
        if let Some(desc) = args["description"]
            .as_str()
            .or_else(|| args["summary"].as_str())
        {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                if trimmed.chars().count() > MAX_DESCRIPTION_CHARS {
                    let truncated: String = trimmed.chars().take(MAX_DESCRIPTION_CHARS).collect();
                    return truncated;
                }
                return trimmed.to_string();
            }
        }
        // Auto-generate from first non-empty line of content
        let first_line = content
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(content);
        let truncated: String = first_line.chars().take(200).collect();
        truncated
    }

    fn resolve_tags(&self, args: &serde_json::Value) -> Vec<String> {
        match args["tags"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => Vec::new(),
        }
    }
}
