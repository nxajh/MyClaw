//! List Directory tool — lists directory contents.
//!
//! More intuitive than glob_search for browsing; no pattern required.
//! Supports recursive listing with max_depth control.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::providers::{Tool, ToolResult};

/// Resolve `p` to an absolute path before an `is_path_protected` check —
/// the protected-path patterns are tilde/absolute, so a relative `path`
/// arg needs resolving against cwd first to be checked correctly.
fn to_abs(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

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

#[allow(clippy::too_many_arguments)]
fn walk_dir_recursive(
    dir: &Path,
    entries: &mut Vec<Value>,
    show_hidden: bool,
    current_depth: u32,
    max_depth: u32,
    max_entries: usize,
    protected_skipped: &mut usize,
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

        let rel_path = entry.path();

        // A matched name under a protected path (~/.ssh/**, **/.env, ...)
        // still discloses its existence, so drop it — and never recurse
        // into a protected directory (everything under it matches too).
        if crate::config::is_path_protected(&to_abs(&rel_path)) {
            *protected_skipped += 1;
            continue;
        }

        let file_type = entry.file_type()?;
        let metadata = entry.metadata().ok();

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
                &rel_path,
                entries,
                show_hidden,
                current_depth + 1,
                max_depth,
                max_entries,
                protected_skipped,
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
        let mut protected_skipped = 0usize;
        if max_depth <= 1 {
            // Single-level: use the original flat listing
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();

                // Skip hidden files
                if !show_hidden && name.starts_with('.') {
                    continue;
                }

                // A matched name under a protected path (~/.ssh/**,
                // **/.env, ...) still discloses its existence, so drop it
                // rather than erroring the whole listing out.
                if crate::config::is_path_protected(&to_abs(&entry.path())) {
                    protected_skipped += 1;
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
            walk_dir_recursive(
                path,
                &mut entries,
                show_hidden,
                0,
                max_depth,
                500,
                &mut protected_skipped,
            )?;
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
                "total": entries.len(),
                "skipped_protected_files": protected_skipped
            }))?,
            error: None,
        })
    }
}

#[cfg(test)]
mod protected_path_tests {
    use super::*;

    #[tokio::test]
    async fn list_dir_skips_protected_files_even_with_show_hidden() {
        let dir = tempfile::tempdir().unwrap();
        // `**/.env` is a default protected pattern regardless of directory,
        // so this doesn't depend on the real $HOME the way `~/.ssh/**`
        // would. `show_hidden: true` isolates this from the pre-existing
        // dotfile filter — both `.env` and `notes.txt` would otherwise be
        // listed, so a leak here is specifically the protected-path gap.
        std::fs::write(dir.path().join(".env"), "SECRET_KEY=do-not-leak").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ok").unwrap();

        let tool = ListDirTool::new();
        let result = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap(), "show_hidden": true}),
                &crate::agents::session::Session::new("test".to_string()),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(
            !result.output.contains(".env"),
            "protected .env filename leaked into list_dir results: {}",
            result.output
        );
        assert!(result.output.contains("notes.txt"));
        assert!(result.output.contains("\"skipped_protected_files\":1"));
    }

    #[tokio::test]
    async fn list_dir_recursive_skips_protected_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join(".env"), "SECRET_KEY=do-not-leak").unwrap();
        std::fs::write(dir.path().join("sub").join("readme.md"), "ok").unwrap();

        let tool = ListDirTool::new();
        let result = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap(), "show_hidden": true, "max_depth": 3}),
                &crate::agents::session::Session::new("test".to_string()),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(
            !result.output.contains(".env"),
            "protected nested .env filename leaked into recursive list_dir results: {}",
            result.output
        );
        assert!(result.output.contains("readme.md"));
        assert!(result.output.contains("\"skipped_protected_files\":1"));
    }
}
