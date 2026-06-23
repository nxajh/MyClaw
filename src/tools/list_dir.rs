//! List Directory tool — lists directory contents.
//!
//! More intuitive than glob_search for browsing; no pattern required.
//! Supports recursive listing with max_depth control.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::Path;

use crate::providers::{Tool, ToolResult};

pub struct ListDirTool;

impl ListDirTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ListDirTool {
    fn default() -> Self {
        Self::new()
    }
}

fn walk_dir_recursive(
    dir: &Path,
    entries: &mut Vec<Value>,
    show_hidden: bool,
    current_depth: u32,
    max_depth: u32,
    max_entries: usize,
) -> std::io::Result<()> {
    if entries.len() >= max_entries {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        let file_type = entry.file_type()?;
        let metadata = entry.metadata().ok();
        let rel_path = entry.path();

        entries.push(json!({
            "name": name,
            "path": rel_path.to_string_lossy(),
            "type": if file_type.is_dir() { "dir" } else if file_type.is_symlink() { "symlink" } else { "file" },
            "size_bytes": metadata.map(|m| m.len()).unwrap_or(0),
            "depth": current_depth,
        }));

        if entries.len() >= max_entries {
            break;
        }

        // Recurse into subdirectories if we haven't reached max depth
        if file_type.is_dir() && current_depth < max_depth {
            walk_dir_recursive(
                &entry.path(),
                entries,
                show_hidden,
                current_depth + 1,
                max_depth,
                max_entries,
            )?;
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List files and directories in a given path. \
         Returns file names, types (file/dir), sizes, and relative paths. \
         Set max_depth > 1 for recursive listing (default: 1 = single level)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list (default: current directory)."
                },
                "show_hidden": {
                    "type": "boolean",
                    "description": "Whether to show hidden files (default: false)."
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum recursion depth (default: 1 = single level). Set to 2+ for recursive listing."
                }
            }
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let path_str = args["path"].as_str().unwrap_or(".");
        let show_hidden = args["show_hidden"].as_bool().unwrap_or(false);
        let max_depth = args["max_depth"].as_u64().unwrap_or(1).min(10) as u32;

        let path = Path::new(path_str);
        if !path.exists() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("path does not exist: {}", path_str)),
            });
        }
        if !path.is_dir() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("not a directory: {}", path_str)),
            });
        }

        let mut entries: Vec<Value> = Vec::new();
        if max_depth <= 1 {
            // Single-level: use the original flat listing
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();

                // Skip hidden files
                if !show_hidden && name.starts_with('.') {
                    continue;
                }

                let file_type = entry.file_type()?;
                let metadata = entry.metadata().ok();

                entries.push(json!({
                    "name": name,
                    "type": if file_type.is_dir() { "dir" } else if file_type.is_symlink() { "symlink" } else { "file" },
                    "size_bytes": metadata.map(|m| m.len()).unwrap_or(0),
                }));
            }
        } else {
            walk_dir_recursive(path, &mut entries, show_hidden, 0, max_depth, 500)?;
        }

        // Sort: directories first, then files, alphabetically
        entries.sort_by(|a, b| {
            let a_is_dir = a["type"] == "dir";
            let b_is_dir = b["type"] == "dir";
            b_is_dir.cmp(&a_is_dir).then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            })
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({
                "ok": true,
                "path": path_str,
                "max_depth": max_depth,
                "entries": entries,
                "total": entries.len()
            }))?,
            error: None,
        })
    }
}
