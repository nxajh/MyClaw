//! File operation tools: read, write, edit.

use crate::providers::{Tool, ToolResult};
use crate::str_utils;
use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Resolve `path` to a clean absolute path (expanding `.`/`..` components,
/// without requiring the path to exist yet, so this works for writes too).
///
/// This used to also reject anything outside `$HOME`/cwd, but that boundary
/// provided no real protection: the `shell` tool is granted at the exact
/// same trust tier (see `agents::tool_executor::is_write_tool` /
/// `needs_approval` — shell has no destination-path restriction of its own,
/// only a small deny-list of process/service-management commands), so any
/// caller that could reach `file_write` could equally reach an unrestricted
/// `echo ... > /anywhere` via `shell`. The only thing the boundary caught
/// was a well-behaved model passing a wrong relative path by mistake, at
/// the cost of blocking legitimate writes outside the home directory (e.g.
/// a mounted data disk). `crate::config::is_path_protected` (checked by
/// callers) still denies specific sensitive paths like `~/.ssh/**`
/// regardless of location.
fn validate_path(path: &str) -> anyhow::Result<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };

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
        _ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;

        let resolved = validate_path(path)?;
        if crate::config::is_path_protected(&resolved) {
            anyhow::bail!("path '{}' is protected and cannot be read", path);
        }
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
        _ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'content' is required"))?;

        let resolved = validate_path(path)?;
        if crate::config::is_path_protected(&resolved) {
            anyhow::bail!("path '{}' is protected and cannot be modified", path);
        }
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
        _ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'path' is required"))?;
        let resolved = validate_path(path)?;
        if crate::config::is_path_protected(&resolved) {
            anyhow::bail!("path '{}' is protected and cannot be modified", path);
        }
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
        let diff = replacement_context_diff(&content, old_string, new_string);
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
    match haystack.find(needle) {
        Some(pos) => haystack[..pos].matches('\n').count() + 1,
        None => 0,
    }
}

/// Build a unified-diff-style context around the first `old_string` ->
/// `new_string` replacement in `content`.
///
/// `old_string` need not be a whole line (issue #108): it may be a mid-line
/// substring, or span multiple lines. Rather than diffing `old_string` and
/// `new_string` as standalone lines — which misrenders when the match
/// doesn't start at a line boundary — this locates the byte range of the
/// match, widens it out to the full line(s) it falls within, and diffs
/// those full lines (old vs. reconstructed-with-replacement) so the shown
/// line numbers and context match what's actually on disk.
fn replacement_context_diff(content: &str, old_string: &str, new_string: &str) -> String {
    let Some(pos) = content.find(old_string) else {
        return String::new();
    };
    let end = pos + old_string.len();

    let line_start = content[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = content[end..]
        .find('\n')
        .map(|i| end + i)
        .unwrap_or(content.len());
    let start_line_number = content[..line_start].matches('\n').count() + 1;

    let old_full = &content[line_start..line_end];
    let new_full = format!(
        "{}{}{}",
        &content[line_start..pos],
        new_string,
        &content[end..line_end]
    );

    let all_lines: Vec<&str> = content.lines().collect();
    let start_idx = start_line_number - 1;
    let end_idx = (start_idx + old_full.split('\n').count()).min(all_lines.len());
    let context_before_start = start_idx.saturating_sub(3);
    let context_after_end = (end_idx + 3).min(all_lines.len());

    let mut out = String::new();
    out.push_str("--- before\n+++ after\n");
    out.push_str(&format!("@@ line {} @@\n", start_line_number));

    for line in &all_lines[context_before_start..start_idx] {
        out.push(' ');
        out.push_str(&str_utils::truncate_line(line, 200));
        out.push('\n');
    }
    for line in old_full.split('\n') {
        out.push('-');
        out.push_str(&str_utils::truncate_line(line, 200));
        out.push('\n');
    }
    for line in new_full.split('\n') {
        out.push('+');
        out.push_str(&str_utils::truncate_line(line, 200));
        out.push('\n');
    }
    for line in &all_lines[end_idx..context_after_end] {
        out.push(' ');
        out.push_str(&str_utils::truncate_line(line, 200));
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod file_edit_diff_tests {
    use super::*;

    /// issue #108: old_string that is a mid-line substring (not the whole
    /// line) must resolve to the line it's actually on, and the diff must
    /// show the full old/new line — not a fabricated bare-substring line.
    #[test]
    fn find_line_number_handles_mid_line_substring() {
        let content = "TOOLTEST-LINE-1: The quick brown fox jumps over the lazy dog.\n\
                        TOOLTEST-LINE-2: some other text\n\
                        TOOLTEST-LINE-3: marker=alpha-7742 (for file_edit replacement)\n\
                        TOOLTEST-LINE-4: end of file sentinel EOF-9931\n";
        let needle = "marker=alpha-7742 (for file_edit replacement)";
        assert_eq!(find_line_number(content, needle), 3);
    }

    #[test]
    fn find_line_number_handles_line_start_match() {
        let content = "line1\nline2\nline3\n";
        assert_eq!(find_line_number(content, "line1"), 1);
        assert_eq!(find_line_number(content, "line2"), 2);
        assert_eq!(find_line_number(content, "line3"), 3);
    }

    #[test]
    fn diff_reconstructs_full_line_for_mid_line_substring() {
        let content = "TOOLTEST-LINE-1: The quick brown fox jumps over the lazy dog.\n\
                        TOOLTEST-LINE-2: some other text\n\
                        TOOLTEST-LINE-3: marker=alpha-7742 (for file_edit replacement)\n\
                        TOOLTEST-LINE-4: end of file sentinel EOF-9931\n";
        let old = "marker=alpha-7742 (for file_edit replacement)";
        let new = "marker=beta-9913 (file_edit verified)";
        let diff = replacement_context_diff(content, old, new);

        assert!(diff.contains("@@ line 3 @@"), "got: {diff}");
        // The full original line 3 is the deleted line, not a bare
        // substring fabricated as its own line.
        assert!(
            diff.contains("-TOOLTEST-LINE-3: marker=alpha-7742 (for file_edit replacement)"),
            "got: {diff}"
        );
        assert!(
            diff.contains("+TOOLTEST-LINE-3: marker=beta-9913 (file_edit verified)"),
            "got: {diff}"
        );
        // Line 3 itself must not also appear as unchanged context.
        assert!(
            !diff.contains(" TOOLTEST-LINE-3:"),
            "line 3 should not appear as unmodified context: {diff}"
        );
        // Surrounding lines are untouched context.
        assert!(diff.contains(" TOOLTEST-LINE-2: some other text"), "got: {diff}");
        assert!(
            diff.contains(" TOOLTEST-LINE-4: end of file sentinel EOF-9931"),
            "got: {diff}"
        );
    }

    #[test]
    fn diff_still_correct_for_whole_line_match() {
        let content = "line1\nline2\nline3\n";
        let diff = replacement_context_diff(content, "line2", "replaced2");
        assert!(diff.contains("@@ line 2 @@"), "got: {diff}");
        assert!(diff.contains("-line2"), "got: {diff}");
        assert!(diff.contains("+replaced2"), "got: {diff}");
        assert!(diff.contains(" line1"), "got: {diff}");
        assert!(diff.contains(" line3"), "got: {diff}");
    }

    #[tokio::test]
    async fn execute_reports_correct_line_for_mid_line_substring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matrix.txt");
        std::fs::write(
            &path,
            "TOOLTEST-LINE-1: The quick brown fox jumps over the lazy dog.\n\
             TOOLTEST-LINE-2: some other text\n\
             TOOLTEST-LINE-3: marker=alpha-7742 (for file_edit replacement)\n\
             TOOLTEST-LINE-4: end of file sentinel EOF-9931\n",
        )
        .unwrap();

        let tool = FileEditTool::new();
        let result = tool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "marker=alpha-7742 (for file_edit replacement)",
                    "new_string": "marker=beta-9913 (file_edit verified)",
                }),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
            )
            .await
            .unwrap();

        assert!(result.success, "got: {:?}", result);
        assert!(
            result.output.contains("(first match line 3)"),
            "got: {}",
            result.output
        );

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("TOOLTEST-LINE-3: marker=beta-9913 (file_edit verified)"));
    }
}

#[cfg(test)]
mod protected_path_tests {
    use super::*;

    #[tokio::test]
    async fn file_read_rejects_protected_path() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
        let protected = format!("{home}/.ssh/id_rsa");
        let tool = FileReadTool::new();
        let err = tool
            .execute(
                json!({"path": protected}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("is protected and cannot be read"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn file_read_allows_unprotected_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "hello").unwrap();
        let tool = FileReadTool::new();
        let result = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }
}
