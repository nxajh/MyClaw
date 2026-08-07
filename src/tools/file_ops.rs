//! File operation tools: read, write, edit.

use crate::providers::{Tool, ToolResult};
use crate::str_utils;
use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
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
        "Read file contents. Supports partial reading via line offset/limit or byte_offset/byte_limit. \
         Set outline=true to extract an overview (function/class names for code, \
         headings for markdown). Supports symbol lookup with symbol/include_body for common code files. \
         Returns range and truncation metadata."
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
                },
                "byte_offset": {
                    "type": "integer",
                    "description": "Starting byte offset, 0-based. If set, reads by byte range instead of line range. Adjusts to valid UTF-8 boundaries."
                },
                "byte_limit": {
                    "type": "integer",
                    "description": "Maximum number of bytes to read in byte range mode. Default: 4096."
                },
                "symbol": {
                    "type": "string",
                    "description": "Symbol name to locate in code files (function/struct/enum/trait/type/mod/class/const)."
                },
                "include_body": {
                    "type": "boolean",
                    "description": "With symbol, include the symbol body/block when possible instead of only the declaration context."
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

        if let Some(symbol) = args["symbol"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            let include_body = args["include_body"].as_bool().unwrap_or(false);
            return Ok(read_symbol(path, &content, symbol, include_body));
        }

        if args.get("byte_offset").and_then(|v| v.as_u64()).is_some()
            || args.get("byte_limit").and_then(|v| v.as_u64()).is_some()
        {
            let byte_offset = args["byte_offset"].as_u64().unwrap_or(0) as usize;
            let byte_limit = args["byte_limit"].as_u64().unwrap_or(4096).max(1) as usize;
            let total_bytes = content.len();
            let requested_start = byte_offset.min(total_bytes);
            let requested_end = requested_start.saturating_add(byte_limit).min(total_bytes);
            let start = next_char_boundary(&content, requested_start);
            let end = prev_char_boundary(&content, requested_end);
            let snippet = if start <= end {
                &content[start..end]
            } else {
                ""
            };
            let prefix = if start > 0 { "..." } else { "" };
            let suffix = if end < total_bytes { "..." } else { "" };

            return Ok(ToolResult {
                success: true,
                output: if snippet.is_empty() {
                    format!(
                        "{} ({} bytes) — byte range {}..{} is empty or beyond end; total_bytes={}; shown_start={}; shown_end={}; truncated={}",
                        path,
                        total_bytes,
                        requested_start,
                        requested_end,
                        total_bytes,
                        start,
                        end,
                        requested_start >= total_bytes || start != 0 || end < total_bytes
                    )
                } else {
                    format!(
                        "{} ({} bytes) — bytes {}..{} requested, {}..{} shown; total_bytes={}; truncated={}\n{}{}{}",
                        path,
                        total_bytes,
                        requested_start,
                        requested_end,
                        start,
                        end,
                        total_bytes,
                        start > 0 || end < total_bytes,
                        prefix,
                        snippet,
                        suffix
                    )
                },
                error: None,
            });
        }

        if outline {
            let outline_text = extract_outline(path, &content);
            return Ok(ToolResult {
                success: true,
                output: if outline_text.is_empty() {
                    format!(
                        "{} ({} lines) — no structural elements found",
                        path,
                        content.lines().count()
                    )
                } else {
                    format!(
                        "{} ({} lines) — outline:\n{}",
                        path,
                        content.lines().count(),
                        outline_text
                    )
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

        let shown_end = start + selected.len();
        let truncated = shown_end < lines.len() || start > 0;
        let output = selected.join("\n");

        Ok(ToolResult {
            success: true,
            output: if output.is_empty() {
                format!(
                    "{} ({} lines, {} bytes) — empty range; total_lines={}; shown_start={}; shown_end={}; truncated={}",
                    path,
                    lines.len(),
                    content.len(),
                    lines.len(),
                    start + 1,
                    shown_end,
                    start >= lines.len()
                )
            } else {
                format!(
                    "{} ({} lines, {} bytes) — lines {}..{} shown; total_lines={}; truncated={}\n{}",
                    path,
                    lines.len(),
                    content.len(),
                    start + 1,
                    shown_end,
                    lines.len(),
                    truncated,
                    output
                )
            },
            error: None,
        })
    }
}

fn next_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn prev_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn read_symbol(path: &str, content: &str, symbol: &str, include_body: bool) -> ToolResult {
    let Some((line_idx, _line)) = find_symbol_line(content, symbol) else {
        return ToolResult {
            success: false,
            output: format!(
                "symbol '{}' not found in {}; total_lines={}; total_bytes={}",
                symbol,
                path,
                content.lines().count(),
                content.len()
            ),
            error: Some(format!("symbol '{}' not found", symbol)),
        };
    };

    let lines: Vec<&str> = content.lines().collect();
    let (start, end) = if include_body {
        symbol_body_range(&lines, line_idx)
    } else {
        (line_idx.saturating_sub(3), (line_idx + 4).min(lines.len()))
    };
    let selected = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    ToolResult {
        success: true,
        output: format!(
            "{} — symbol='{}' declaration_line={}; shown_lines={}..{}; total_lines={}; include_body={}; truncated={}\n{}",
            path,
            symbol,
            line_idx + 1,
            start + 1,
            end,
            lines.len(),
            include_body,
            start > 0 || end < lines.len(),
            selected
        ),
        error: None,
    }
}

fn find_symbol_line(content: &str, symbol: &str) -> Option<(usize, String)> {
    let escaped = regex::escape(symbol);
    let re = regex::Regex::new(&format!(
        r"^\s*(?:pub\s+)?(?:(?:async\s+)?fn|struct|enum|trait|type|mod|const|static)\s+{}\b|^\s*impl(?:<[^>]+>)?\s+{}\b|^\s*(?:export\s+)?(?:(?:async\s+)?function|class|interface|type|const)\s+{}\b|^\s*(?:def|class)\s+{}\b",
        escaped, escaped, escaped, escaped
    ))
    .ok()?;
    content
        .lines()
        .enumerate()
        .find(|(_, line)| re.is_match(line))
        .map(|(idx, line)| (idx, line.to_string()))
}

fn symbol_body_range(lines: &[&str], line_idx: usize) -> (usize, usize) {
    let start = line_idx;
    let Some(open_rel) = lines[start..].iter().position(|line| line.contains('{')) else {
        return (line_idx.saturating_sub(3), (line_idx + 8).min(lines.len()));
    };
    let mut depth = 0isize;
    let mut seen_open = false;
    for (idx, line) in lines.iter().enumerate().skip(start + open_rel) {
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' if seen_open => depth -= 1,
                _ => {}
            }
        }
        if seen_open && depth <= 0 {
            return (start, (idx + 1).min(lines.len()));
        }
    }
    (start, lines.len())
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
            (
                r"^\s*(export\s+)?(default\s+)?interface\s+(\w+)",
                "interface",
            ),
            (r"^\s*(export\s+)?type\s+(\w+)", "type"),
        ],
        "md" | "markdown" => vec![(r"^#{1,6}\s+.*", "heading")],
        "toml" => vec![(r"^\[[\w.-]+\]", "section")],
        "yaml" | "yml" => vec![(r"^[\w-]+\s*:", "key")],
        "json" => {
            // For JSON, just show top-level keys
            return extract_json_outline(content);
        }
        _ => {
            // Generic: look for function-like patterns
            vec![
                (
                    r"(?i)^\s*(pub|public|private|protected)?\s*(static\s+)?(async\s+)?function\s+(\w+)",
                    "function",
                ),
                (
                    r"(?i)^\s*(pub|public|private|protected)?\s*class\s+(\w+)",
                    "class",
                ),
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
        format!(
            "total {} lines, {} structural elements\n{}",
            total_lines,
            result.len(),
            result.join("\n")
        )
    }
}

fn extract_json_outline(content: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(content) else {
        return String::new();
    };
    match val {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map.keys().map(|k| format!("  {}", k)).collect();
            format!(
                "JSON object with {} top-level keys:\n{}",
                keys.len(),
                keys.join("\n")
            )
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
        crate::config::validate_write(&resolved, content)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
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
        let old_hash = sha256_hex(content.as_bytes());
        let old_bytes = content.len();
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
                output: format!(
                    "old_string found {} times, must be unique (or set replace_all=true)",
                    count
                ),
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

        // Validate content-level protection (credential lines in config file).
        crate::config::validate_write(&resolved, &new_content)
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        tokio::fs::write(path, &new_content)
            .await
            .map_err(|e| anyhow::anyhow!("failed to write '{}': {}", path, e))?;

        let written = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to verify '{}': readback failed: {}", path, e))?;
        if written != new_content {
            return Ok(ToolResult {
                success: false,
                output: format!(
                    "write verification failed for {}; expected_hash={}; actual_hash={}; expected_bytes={}; actual_bytes={}",
                    path,
                    sha256_hex(new_content.as_bytes()),
                    sha256_hex(written.as_bytes()),
                    new_content.len(),
                    written.len()
                ),
                error: Some("write verification failed: readback differs from intended content".to_string()),
            });
        }
        let new_hash = sha256_hex(written.as_bytes());
        let new_bytes = written.len();

        let replaced_count = if replace_all { count } else { 1 };
        let line_number = find_line_number(&content, old_string);
        let diff = replacement_context_diff(&content, old_string, new_string, line_number);
        Ok(ToolResult {
            success: true,
            output: format!(
                "replaced {} occurrence(s) in {} (first match line {}); bytes {} -> {}; sha256 {} -> {}; verified_readback=true\n{}",
                replaced_count, path, line_number, old_bytes, new_bytes, old_hash, new_hash, diff,
            ),
            error: None,
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Find the 1-based line number where `needle` first occurs in `haystack`.
fn find_line_number(haystack: &str, needle: &str) -> usize {
    if let Some(pos) = haystack.find(needle) {
        haystack[..pos].lines().count() + 1
    } else {
        0
    }
}

fn replacement_context_diff(
    content: &str,
    old_string: &str,
    new_string: &str,
    line_number: usize,
) -> String {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let new_lines: Vec<&str> = new_string.lines().collect();
    let all_lines: Vec<&str> = content.lines().collect();

    let old_len = old_lines.len().max(1);
    let start = line_number.saturating_sub(1);
    let context_before_start = start.saturating_sub(3);
    let after_start = (start + old_len).min(all_lines.len());
    let after_end = (after_start + 3).min(all_lines.len());

    let mut out = String::new();
    out.push_str("--- before\n+++ after\n");
    out.push_str(&format!("@@ line {} @@\n", line_number));

    for line in &all_lines[context_before_start..start] {
        out.push(' ');
        out.push_str(&str_utils::truncate_line(line, 200));
        out.push('\n');
    }
    if old_lines.is_empty() {
        out.push_str("-\n");
    } else {
        for line in old_lines {
            out.push('-');
            out.push_str(&str_utils::truncate_line(line, 200));
            out.push('\n');
        }
    }
    if new_lines.is_empty() {
        out.push_str("+\n");
    } else {
        for line in new_lines {
            out.push('+');
            out.push_str(&str_utils::truncate_line(line, 200));
            out.push('\n');
        }
    }
    for line in &all_lines[after_start..after_end] {
        out.push(' ');
        out.push_str(&str_utils::truncate_line(line, 200));
        out.push('\n');
    }

    out
}
