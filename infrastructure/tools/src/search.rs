//! Search tools: glob (file name) and content (regex) search.

use async_trait::async_trait;
use capability::tool::{Tool, ToolResult};
use serde_json::json;
use std::path::Path;

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
            if name.starts_with('.') || name == "target" || name == "node_modules" || name == "__pycache__" {
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
        "Search for files matching a glob pattern. Supports *, **, and ? wildcards."
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
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'pattern' is required"))?;
        let base = args["path"].as_str().unwrap_or(".");
        let base = Path::new(base);

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
        walk_dir(base, &mut files, 1000)?;

        // Match against relative paths from base.
        let matches: Vec<String> = files.iter()
            .filter_map(|f| {
                let rel = Path::new(f).strip_prefix(base).ok()?;
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
            format!("no files matching '{}' found in {}", pattern, base.display())
        } else {
            let display: Vec<&str> = matches.iter().take(500).map(|s| s.as_str()).collect();
            let mut out = format!("{} files found:\n", matches.len());
            out.push_str(&display.join("\n"));
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

fn search_in_file(path: &Path, re: &regex::Regex, max_lines: usize) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(path).ok()?;
    if !re.is_match(&content) {
        return None;
    }
    let mut results = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if re.is_match(line) {
            results.push(format!("{}:{}\t{}", path.display(), i + 1, line.trim()));
            if results.len() >= max_lines {
                break;
            }
        }
    }
    if results.is_empty() { None } else { Some(results) }
}

#[async_trait]
impl Tool for ContentSearchTool {
    fn name(&self) -> &str {
        "content_search"
    }

    fn description(&self) -> &str {
        "Search file contents by regex pattern. Returns matching lines with file path and line numbers."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression pattern to search for."
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
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'pattern' is required"))?;
        let base = args["path"].as_str().unwrap_or(".");
        let include = args["include"].as_str();
        let max_results = args["max_results"].as_u64().unwrap_or(200) as usize;

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
            regex::Regex::new(&format!("^{}$", regexified)).unwrap_or_else(|_| regex::Regex::new(".*").unwrap())
        });

        let base = Path::new(base);
        let mut all_files = Vec::new();
        if base.is_file() {
            all_files.push(base.to_string_lossy().into_owned());
        } else {
            walk_dir(base, &mut all_files, 5000)?;
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
            if let Some(matches) = search_in_file(file_path, &re, max_results - results.len()) {
                results.extend(matches);
                if results.len() >= max_results {
                    break;
                }
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
