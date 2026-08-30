//! memory_list / memory_view tools and shared scope / user resolution helpers.

use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::format::{link_values, scan_merged};
use super::search::normalize_optional_filter;
use crate::api::tool::ToolContext;
use crate::identity::user_profile::UserResolver;
use crate::memory::build_backlinks;
use crate::providers::{Tool, ToolResult};

/// Common helper: resolve the user_id from a session via the resolver.
pub(super) fn user_id_for(ctx: &ToolContext, resolver: &UserResolver) -> String {
    resolver.resolve(&ctx.owner)
}

/// Resolve the target scope: "user" (default) or "agent".
pub(super) fn resolve_scope(args: &serde_json::Value) -> &'static str {
    match args["scope"].as_str() {
        Some("agent") => "agent",
        _ => "user",
    }
}

/// Directory for a scope's memory files.
/// P4 (RFC #101 §6): layered storage — `scope=="agent"` → the memory root
/// itself; user scope → `{base}/users/{bare uuid}/memory`. Pure computation;
/// creating the directory is the caller's job.
pub(super) fn scope_memory_dir(memory_root: &Path, scope: &str, user_id: &str) -> PathBuf {
    if scope == "agent" {
        memory_root.to_path_buf()
    } else {
        crate::memory::user_memory_dir(memory_root, user_id)
    }
}

/// Scan memory files from a single scope (not merged). Layered (P4):
/// agent scope → the agent layer; user scope → this user's user layer plus
/// pre-migration fallback entries still in the agent dir (frontmatter wins).
/// I/O errors other than NotFound propagate — never fake an empty layer.
pub(super) fn scan_scope(
    memory_root: &Path,
    scope: &str,
    user_id: &str,
) -> std::io::Result<Vec<crate::memory::MemoryFile>> {
    if scope == "agent" {
        crate::memory::scan_agent_layer(memory_root)
    } else {
        crate::memory::scan_user_layer(memory_root, user_id)
    }
}

/// Tool-level scan failure: surface the I/O error in the tool output
/// instead of pretending the layer is empty.
pub(super) fn scan_error_result(tool: &str, e: std::io::Error) -> ToolResult {
    tracing::error!(tool = tool, error = %e, "memory layer scan I/O error");
    let msg = format!("memory layer scan failed: {}", e);
    ToolResult {
        success: false,
        output: json!({ "success": false, "error": msg }).to_string(),
        error: Some(msg),
    }
}

// ══════════════════════════════════════════════════════════════════════════
// memory_list
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryListTool {
    memory_root: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemoryListTool {
    pub fn new(memory_root: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            memory_root,
            resolver,
        }
    }
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "memory_list"
    }

    fn description(&self) -> &str {
        "List all memory entries with metadata. Use this to browse what the agent remembers, \
         check if a specific fact is stored, or find entries to update/remove."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "memory_type": {
                    "type": "string",
                    "description": "Optional type filter. Omit, pass empty string, or whitespace to list all types."
                }
            }
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let type_filter = normalize_optional_filter(args["memory_type"].as_str());
        let user_id = user_id_for(ctx, &self.resolver);
        let files = match scan_merged(&self.memory_root, &user_id) {
            Ok(files) => files,
            Err(e) => return Ok(scan_error_result("memory_list", e)),
        };
        let entries: Vec<crate::memory::IndexEntry> = files
            .iter()
            .filter(|mf| {
                type_filter
                    .as_ref()
                    .is_none_or(|wanted| mf.mem_type.to_lowercase() == *wanted)
            })
            .map(crate::memory::IndexEntry::from)
            .collect();

        if entries.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "success": true,
                    "count": 0,
                    "entries": [],
                    "hint": "Use memory_manage(action='add') to create your first memory."
                })
                .to_string(),
                error: None,
            });
        }

        let backlinks = build_backlinks(&files);
        let json_entries: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                let backlinks_count = backlinks.get(&e.name).map(|b| b.len()).unwrap_or(0);
                let mut obj = json!({
                    "type": &e.mem_type,
                    "name": &e.name,
                    "description": &e.description,
                    "link_count": e.link_count,
                    "backlink_count": backlinks_count,
                });
                if !e.tags.is_empty() {
                    obj["tags"] = json!(e.tags);
                }
                obj
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: json!({
                "success": true,
                "count": entries.len(),
                "entries": json_entries,
                "hint": "Use memory_view(name) to read full content, or memory_search(query) to search."
            }).to_string(),
            error: None,
        })
    }
}

// ══════════════════════════════════════════════════════════════════════════
// memory_view
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryViewTool {
    memory_root: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemoryViewTool {
    pub fn new(memory_root: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            memory_root,
            resolver,
        }
    }
}

#[async_trait]
impl Tool for MemoryViewTool {
    fn name(&self) -> &str {
        "memory_view"
    }

    fn description(&self) -> &str {
        "Read a memory entry's full content. Provide the memory name (not filename)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the memory entry to read."
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: json!({"success": false, "error": "'name' is required."}).to_string(),
                    error: None,
                });
            }
        };

        let user_id = user_id_for(ctx, &self.resolver);
        let files = match scan_merged(&self.memory_root, &user_id) {
            Ok(files) => files,
            Err(e) => return Ok(scan_error_result("memory_view", e)),
        };
        let file = files.iter().find(|f| f.name == name);

        match file {
            Some(mf) => {
                let backlinks = build_backlinks(&files);
                let file_backlinks = backlinks.get(&mf.name).cloned().unwrap_or_default();
                let outgoing = link_values(&mf.links);

                let mut output = json!({
                    "success": true,
                    "name": &mf.name,
                    "type": &mf.mem_type,
                    "description": &mf.description,
                    "created_at": &mf.created_at,
                    "content": &mf.content,
                    "links": outgoing,
                    "backlinks": file_backlinks,
                });
                if !mf.tags.is_empty() {
                    output["tags"] = json!(mf.tags);
                }
                Ok(ToolResult {
                    success: true,
                    output: output.to_string(),
                    error: None,
                })
            }
            None => {
                let available: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                Ok(ToolResult {
                    success: false,
                    output: json!({
                        "success": false,
                        "error": format!("Memory '{}' not found.", name),
                        "available": available,
                        "hint": "Use memory_list() to see all entries."
                    })
                    .to_string(),
                    error: None,
                })
            }
        }
    }
}
