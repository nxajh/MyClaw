//! Search tools: glob (file name) and content (regex) search.

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

// ── GlobSearchTool ───────────────────────────────────────────────────────────

#[derive(Default)]
pub struct GlobSearchTool;

impl GlobSearchTool {
    pub fn new() -> Self {
        Self
    }
}

/// Convert a simple glob pattern to a regex.
/// Supports: `*` (any non-slash), `**` (any path), `?` (single char).
fn glob_to_regex(pattern: &str) -> String {
    let mut regex = String::with_capacity(pattern.len() * 2);
    regex.push('^');
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                // ** = match any path prefix (including none)
                regex.push_str("(.*/)?");
                i += 2;
                // Skip trailing / after **
                if i < chars.len() && chars[i] == '/' {
                    i += 1;
                }
            }
            '*' => {
                regex.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                regex.push_str("[^/]");
                i += 1;
            }
            c if ".\\+()[]{}|^$".contains(c) => {
                regex.push('\\');
                regex.push(c);
                i += 1;
            }
            c => {
                regex.push(c);
                i += 1;
            }
        }
    }
    regex.push('$');
    regex
}

/// Resolve the search base directory.
/// If `path` is None or "." or empty, falls back to the workspace root
/// (the MyClaw working directory), NOT the process cwd which may differ.
fn resolve_search_base(path: Option<&str>) -> PathBuf {
    let raw = path.unwrap_or(".").trim();
    if raw.is_empty() || raw == "." {
        // Use the MyClaw workspace directory, not the OS process cwd.
        // The workspace dir is set as the daemon's working directory at startup.
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(raw)
    }
}

fn walk_dir(dir: &Path, results: &mut Vec<String>, max: usize) -> std::io::Result<()> {
    if results.len() >= max {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden and common non-project dirs.
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
            {
                continue;
            }
            walk_dir(&path, results, max)?;
        } else {
            results.push(path.to_string_lossy().into_owned());
        }
        if results.len() >= max {
            return Ok(());
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for GlobSearchTool {
    fn name(&self) -> &str {
        "glob_search"
    }

    fn description(&self) -> &str {
        "Search for files matching a glob pattern. Supports *, **, and ? wildcards. Optionally returns file metadata (size, mtime)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, e.g. '**/*.rs', 'src/**/*.toml'."
                },
                "path": {
                    "type": "string",
                    "description": "Base directory to search in (default: current directory)."
                },
                "include_metadata": {
                    "type": "boolean",
                    "description": "If true, include file size and last-modified time for each match (default: false)."
                }
            },
            "required": ["pattern"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        3_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'pattern' is required"))?;
        let include_metadata = args["include_metadata"].as_bool().unwrap_or(false);

        let base = resolve_search_base(args["path"].as_str());

        if !base.exists() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("path '{}' does not exist", base.display())),
            });
        }

        let regex_str = glob_to_regex(pattern);
        let re = regex::Regex::new(&regex_str)
            .map_err(|e| anyhow::anyhow!("invalid glob pattern '{}': {}", pattern, e))?;

        let mut files = Vec::new();
        walk_dir(&base, &mut files, 1000)?;

        // Match against relative paths from base.
        let matches: Vec<String> = files
            .iter()
            .filter_map(|f| {
                let rel = Path::new(f).strip_prefix(&base).ok()?;
                let rel_str = rel.to_string_lossy();
                if re.is_match(&rel_str) {
                    Some(f.clone())
                } else {
                    None
                }
            })
            .collect();

        let truncated = matches.len() > 500;
        let output = if matches.is_empty() {
            format!(
                "no files matching '{}' found in {}",
                pattern,
                base.display()
            )
        } else {
            let mut out = format!("{} files found:\n", matches.len());
            if include_metadata {
                for f in matches.iter().take(500) {
                    let meta_info = std::fs::metadata(f)
                        .ok()
                        .map(|m| {
                            let size = m.len();
                            let mtime = m.modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            format!("  [{} bytes, mtime={}]", size, mtime)
                        })
                        .unwrap_or_default();
                    out.push_str(&format!("{}{}\n", f, meta_info));
                }
            } else {
                let display: Vec<&str> = matches.iter().take(500).map(|s| s.as_str()).collect();
                out.push_str(&display.join("\n"));
            }
            if truncated {
                out.push_str("\n... (truncated at 500 results)");
            }
            out
        };

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

// ── ContentSearchTool ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ContentSearchTool;

impl ContentSearchTool {
    pub fn new() -> Self {
        Self
    }
}

/// Search a single file for regex matches, optionally including context lines.
fn search_in_file(
    path: &Path,
    re: &regex::Regex,
    max_lines: usize,
    context_lines: usize,
) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(path).ok()?;
    if !re.is_match(&content) {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut results = Vec::new();
    let mut prev_match_end: Option<usize> = None; // for merging overlapping context

    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            // Determine context range, merging with previous if overlapping.
            let ctx_start = i.saturating_sub(context_lines);
            let ctx_end = (i + context_lines + 1).min(lines.len());

            // If this overlaps with the previous match's context, continue seamlessly.
            let start_from = match prev_match_end {
                Some(prev_end) if prev_end >= ctx_start => prev_end,
                _ => {
                    if context_lines > 0 && ctx_start > 0 {
                        results.push(format!("{}-{}-\t...", path.display(), ctx_start));
                    }
                    ctx_start
                }
            };

            for j in start_from..ctx_end {
                let prefix = if j == i { "" } else { "-" };
                results.push(format!("{}:{}{}\t{}", path.display(), j + 1, prefix, lines[j]));
            }
            prev_match_end = Some(ctx_end);

            // Check if we've hit max results (count only actual matches, not context).
            let match_count = results.iter().filter(|r| !r.contains(":\t-")).count();
            if match_count >= max_lines {
                break;
            }
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

#[async_trait]
impl Tool for ContentSearchTool {
    fn name(&self) -> &str {
        "content_search"
    }

    fn description(&self) -> &str {
        "Search file contents by regex pattern. Returns matching lines with file path and line numbers. Supports context_lines to show surrounding lines (like grep -A/-B)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "regex": {
                    "type": "string",
                    "description": "Regular expression to search for."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default: current directory)."
                },
                "include": {
                    "type": "string",
                    "description": "File name glob filter, e.g. '*.rs', '*.{rs,toml}'."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of matching lines to return (default 200)."
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match (like grep -C). Default: 0 (no context)."
                }
            },
            "required": ["regex"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        3_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let pattern = args["regex"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'regex' is required"))?;
        let include = args["include"].as_str();
        let max_results = args["max_results"].as_u64().unwrap_or(200) as usize;
        let context_lines = args["context_lines"].as_u64().unwrap_or(0) as usize;

        let re = regex::Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("invalid regex '{}': {}", pattern, e))?;

        // Build include filter regex from glob.
        let include_re = include.map(|inc| {
            // Convert simple glob like "*.rs" or "*.{rs,toml}" to regex.
            let regexified = inc
                .replace('.', r"\.")
                .replace('*', ".*")
                .replace('{', "(")
                .replace('}', ")")
                .replace(',', "|");
            regex::Regex::new(&format!("^{}$", regexified))
                .unwrap_or_else(|_| regex::Regex::new(".*").unwrap())
        });

        let base = resolve_search_base(args["path"].as_str());
        let mut all_files = Vec::new();
        if base.is_file() {
            all_files.push(base.to_string_lossy().into_owned());
        } else {
            walk_dir(&base, &mut all_files, 5000)?;
        }

        let mut results = Vec::new();
        for file_path_str in &all_files {
            let file_path = Path::new(file_path_str);
            // Apply include filter.
            if let Some(ref inc_re) = include_re {
                let name = file_path.file_name().unwrap_or_default().to_string_lossy();
                if !inc_re.is_match(&name) {
                    continue;
                }
            }
            // Skip binary-ish files and very large files.
            if let Ok(meta) = std::fs::metadata(file_path) {
                if meta.len() > 5_000_000 {
                    continue;
                }
            }

            // Count actual matches so far to enforce max_results.
            let current_match_count = results.iter().filter(|r| !r.contains(":-\t") && !r.contains(":\t...")).count();
            if current_match_count >= max_results {
                break;
            }

            if let Some(matches) = search_in_file(
                file_path,
                &re,
                max_results - current_match_count,
                context_lines,
            ) {
                results.extend(matches);
            }
        }

        let truncated = results.len() >= max_results;
        let output = if results.is_empty() {
            format!("no matches for '{}' in {}", pattern, base.display())
        } else {
            let mut out = results.join("\n");
            if truncated {
                out.push_str(&format!("\n... (truncated at {} results)", max_results));
            }
            out
        };

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}
