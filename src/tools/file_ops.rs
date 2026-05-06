//! File operation tools: read, write, edit.

use async_trait::async_trait;
use crate::providers::{Tool, ToolResult};
use crate::str_utils;
use serde_json::json;
use std::path::Path;

/// Resolve `path` to an absolute path and check it stays within the user's
/// home directory (or current working directory for relative paths).
/// Returns `Err` with a descriptive message if the resolved path would escape.
fn validate_path(path: &str) -> anyhow::Result<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };

    // Normalize without requiring the path to exist yet (for writes).
    let mut normalized = std::path::PathBuf::new();
    for component in abs.components() {
        match component {
            std::path::Component::ParentDir => { normalized.pop(); }
            std::path::Component::CurDir => {}
            c => normalized.push(c),
        }
    }

    // Disallow paths outside home or cwd — catches ../../etc/passwd patterns.
    let home = std::env::var("HOME").map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/home"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if !normalized.starts_with(&home) && !normalized.starts_with(&cwd) {
        anyhow::bail!(
            "path '{}' resolves outside allowed directories (home: {}, cwd: {})",
            path, home.display(), cwd.display()
        );
    }
    Ok(normalized)
}

// ── FileReadTool ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FileReadTool;

impl FileReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read file contents. Supports partial reading via offset and limit."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file."
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number, 1-based (default: 1)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default: all)."
                }
            },
            "required": ["path"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        10_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;

        let resolved = validate_path(path)?;
        let path = resolved.to_str().unwrap_or(path);

        let offset = args["offset"].as_u64().unwrap_or(0) as usize; // 0 means from start
        let limit = args["limit"].as_u64().map(|l| l as usize);

        let content = tokio::fs::read_to_string(path).await.map_err(|e| {
            anyhow::anyhow!("failed to read '{}': {}", path, e)
        })?;

        let lines: Vec<&str> = content.lines().collect();
        let start = if offset > 0 { (offset - 1).min(lines.len()) } else { 0 };

        let selected: Vec<String> = if let Some(limit) = limit {
            lines[start..].iter().take(limit)
                .enumerate()
                .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
                .collect()
        } else {
            lines[start..].iter()
                .enumerate()
                .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
                .collect()
        };

        let output = selected.join("\n");

        Ok(ToolResult {
            success: true,
            output: if output.is_empty() {
                "(empty file or offset beyond end)".to_string()
            } else {
                format!("{} ({} lines)\n{}", path, lines.len(), output)
            },
            error: None,
        })
    }
}

// ── FileWriteTool ────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FileWriteTool;

impl FileWriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates parent directories if needed. Overwrites existing content."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file."
                },
                "content": {
                    "type": "string",
                    "description": "Content to write."
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'content' is required"))?;

        let resolved = validate_path(path)?;
        let path = resolved.to_str().unwrap_or(path);

        // Create parent directories if needed.
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    anyhow::anyhow!("failed to create parent dirs for '{}': {}", path, e)
                })?;
            }
        }

        tokio::fs::write(path, content).await.map_err(|e| {
            anyhow::anyhow!("failed to write '{}': {}", path, e)
        })?;

        let line_count = content.lines().count();
        let byte_count = content.len();

        Ok(ToolResult {
            success: true,
            output: format!(
                "wrote {} bytes ({} lines) to {}\n  first: {}\n  last: {}",
                byte_count,
                line_count,
                path,
                content.lines().next().map(|l| str_utils::truncate_line(l, 80)).unwrap_or_else(|| "(empty)".to_string()),
                content.lines().last().map(|l| str_utils::truncate_line(l, 80)).unwrap_or_else(|| "(empty)".to_string()),
            ),
            error: None,
        })
    }
}

// ── FileEditTool ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FileEditTool;

impl FileEditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string match with new content. The old_string must appear exactly once in the file."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file."
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to find (must appear exactly once)."
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text (use empty string to delete)."
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;
        let resolved = validate_path(path)?;
        let path = resolved.to_str().unwrap_or(path);
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'old_string' is required"))?;
        let new_string = args["new_string"]
            .as_str()
            .unwrap_or("");

        let content = tokio::fs::read_to_string(path).await.map_err(|e| {
            anyhow::anyhow!("failed to read '{}': {}", path, e)
        })?;

        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("old_string not found in file".to_string()),
            });
        }
        if count > 1 {
            return Ok(ToolResult {
                success: false,
                output: format!("old_string found {} times, must be unique", count),
                error: Some(format!("old_string matched {} times, expected exactly 1", count)),
            });
        }

        let new_content = content.replacen(old_string, new_string, 1);

        tokio::fs::write(path, &new_content).await.map_err(|e| {
            anyhow::anyhow!("failed to write '{}': {}", path, e)
        })?;

        Ok(ToolResult {
            success: true,
            output: format!(
                "replaced 1 occurrence in {} (line {}):\n  - {}\n  + {}",
                path,
                find_line_number(&content, old_string),
                str_utils::truncate_line(old_string, 80),
                str_utils::truncate_line(new_string, 80),
            ),
            error: None,
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Find the 1-based line number where `needle` first occurs in `haystack`.
fn find_line_number(haystack: &str, needle: &str) -> usize {
    if let Some(pos) = haystack.find(needle) {
        haystack[..pos].lines().count() + 1
    } else {
        0
    }
}
