//! File operation tools: read, write, edit.

use async_trait::async_trait;
use capability::tool::{Tool, ToolResult};
use serde_json::json;
use std::path::Path;

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

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;

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
            output: format!("wrote {} bytes ({} lines) to {}", byte_count, line_count, path),
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
            output: format!("replaced 1 occurrence in {}", path),
            error: None,
        })
    }
}
