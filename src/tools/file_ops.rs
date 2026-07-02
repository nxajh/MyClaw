//! File operation tools: read, write, edit.

use crate::providers::{Tool, ToolResult};
use crate::str_utils;
use async_trait::async_trait;
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
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            c => normalized.push(c),
        }
    }

    // Disallow paths outside home or cwd — catches ../../etc/passwd patterns.
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/home"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if !normalized.starts_with(&home) && !normalized.starts_with(&cwd) {
        anyhow::bail!(
            "path '{}' resolves outside allowed directories (home: {}, cwd: {})",
            path,
            home.display(),
            cwd.display()
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
        "Read file contents. Supports partial reading via offset and limit. \
         Set outline=true to extract an overview (function/class names for code, \
         headings for markdown)."
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
                },
                "outline": {
                    "type": "boolean",
                    "description": "If true, extract only structural overview: function/struct/impl names for Rust, def/class for Python, function/class/const for JS/TS, headings for Markdown. (default: false)."
                }
            },
            "required": ["path"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        10_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;

        let resolved = validate_path(path)?;
        let path = resolved.to_str().unwrap_or(path);

        let outline = args["outline"].as_bool().unwrap_or(false);

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read '{}': {}", path, e))?;

        if outline {
            let outline_text = extract_outline(path, &content);
            return Ok(ToolResult {
                success: true,
                output: if outline_text.is_empty() {
                    format!("{} ({} lines) — no structural elements found", path, content.lines().count())
                } else {
                    format!("{} ({} lines) — outline:\n{}", path, content.lines().count(), outline_text)
                },
                error: None,
            });
        }

        let offset = args["offset"].as_u64().unwrap_or(0) as usize; // 0 means from start
        let limit = args["limit"].as_u64().map(|l| l as usize);

        let lines: Vec<&str> = content.lines().collect();
        let start = if offset > 0 {
            (offset - 1).min(lines.len())
        } else {
            0
        };

        let selected: Vec<String> = if let Some(limit) = limit {
            lines[start..]
                .iter()
                .take(limit)
                .enumerate()
                .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
                .collect()
        } else {
            lines[start..]
                .iter()
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

/// Extract a structural outline from a file based on its extension.
fn extract_outline(path: &str, content: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let total_lines = content.lines().count();
    let patterns: Vec<(&str, &str)> = match ext {
        "rs" => vec![
            (r"^\s*(pub\s+)?(async\s+)?fn\s+(\w+)", "fn"),
            (r"^\s*(pub\s+)?struct\s+(\w+)", "struct"),
            (r"^\s*(pub\s+)?enum\s+(\w+)", "enum"),
            (r"^\s*(pub\s+)?trait\s+(\w+)", "trait"),
            (r"^\s*impl\s+", "impl"),
            (r"^\s*mod\s+(\w+)", "mod"),
            (r"^\s*type\s+(\w+)", "type"),
        ],
        "py" => vec![
            (r"^\s*def\s+(\w+)", "def"),
            (r"^\s*class\s+(\w+)", "class"),
            (r"^\s*async\s+def\s+(\w+)", "async def"),
        ],
        "js" | "ts" | "jsx" | "tsx" | "mjs" => vec![
            (r"^\s*(export\s+)?(async\s+)?function\s+(\w+)", "function"),
            (r"^\s*(export\s+)?class\s+(\w+)", "class"),
            (r"^\s*(export\s+)?const\s+(\w+)\s*=", "const"),
            (r"^\s*(export\s+)?(default\s+)?interface\s+(\w+)", "interface"),
            (r"^\s*(export\s+)?type\s+(\w+)", "type"),
        ],
        "md" | "markdown" => vec![
            (r"^#{1,6}\s+.*", "heading"),
        ],
        "toml" => vec![
            (r"^\[[\w.-]+\]", "section"),
        ],
        "yaml" | "yml" => vec![
            (r"^[\w-]+\s*:", "key"),
        ],
        "json" => {
            // For JSON, just show top-level keys
            return extract_json_outline(content);
        }
        _ => {
            // Generic: look for function-like patterns
            vec![
                (r"(?i)^\s*(pub|public|private|protected)?\s*(static\s+)?(async\s+)?function\s+(\w+)", "function"),
                (r"(?i)^\s*(pub|public|private|protected)?\s*class\s+(\w+)", "class"),
            ]
        }
    };

    if patterns.is_empty() {
        return String::new();
    }

    let mut result = Vec::new();
    for (i, line) in content.lines().enumerate() {
        for (pattern, kind) in &patterns {
            if let Ok(re) = regex::Regex::new(pattern) {
                if re.is_match(line) {
                    result.push(format!("{:>6}  [{:>10}]  {}", i + 1, kind, line.trim()));
                    break;
                }
            }
        }
    }

    if result.is_empty() {
        String::new()
    } else {
        format!("total {} lines, {} structural elements\n{}", total_lines, result.len(), result.join("\n"))
    }
}

fn extract_json_outline(content: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(content) else {
        return String::new();
    };
    match val {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map.keys().map(|k| format!("  {}", k)).collect();
            format!("JSON object with {} top-level keys:\n{}", keys.len(), keys.join("\n"))
        }
        serde_json::Value::Array(arr) => {
            format!("JSON array with {} elements", arr.len())
        }
        _ => "JSON primitive value".to_string(),
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

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
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

        tokio::fs::write(path, content)
            .await
            .map_err(|e| anyhow::anyhow!("failed to write '{}': {}", path, e))?;

        let line_count = content.lines().count();
        let byte_count = content.len();

        Ok(ToolResult {
            success: true,
            output: format!(
                "wrote {} bytes ({} lines) to {}\n  first: {}\n  last: {}",
                byte_count,
                line_count,
                path,
                content
                    .lines()
                    .next()
                    .map(|l| str_utils::truncate_line(l, 80))
                    .unwrap_or_else(|| "(empty)".to_string()),
                content
                    .lines()
                    .last()
                    .map(|l| str_utils::truncate_line(l, 80))
                    .unwrap_or_else(|| "(empty)".to_string()),
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
        "Edit a file by replacing an exact string match with new content. \
         old_string must appear exactly once unless replace_all=true."
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
                    "description": "The exact text to find (must appear exactly once, unless replace_all is true)."
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text (use empty string to delete)."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "If true, replace all occurrences of old_string (default: false)."
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;
        let resolved = validate_path(path)?;
        let path = resolved.to_str().unwrap_or(path);

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read '{}': {}", path, e))?;

        // Single-edit mode: old_string / new_string
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'old_string' is required"))?;
        let new_string = args["new_string"].as_str().unwrap_or("");
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("old_string not found in file".to_string()),
            });
        }
        if count > 1 && !replace_all {
            return Ok(ToolResult {
                success: false,
                output: format!("old_string found {} times, must be unique (or set replace_all=true)", count),
                error: Some(format!(
                    "old_string matched {} times, expected exactly 1 (set replace_all=true to replace all)",
                    count
                )),
            });
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        tokio::fs::write(path, &new_content)
            .await
            .map_err(|e| anyhow::anyhow!("failed to write '{}': {}", path, e))?;

        let replaced_count = if replace_all { count } else { 1 };
        Ok(ToolResult {
            success: true,
            output: format!(
                "replaced {} occurrence(s) in {} (line {}):\n  - {}\n  + {}",
                replaced_count,
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
