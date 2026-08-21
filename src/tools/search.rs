//! Search tools: glob (file name) and content (regex) search.

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct GlobSearchTool;

impl GlobSearchTool {
    pub fn new() -> Self {
        Self
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let mut regex = String::with_capacity(pattern.len() * 2);
    regex.push('^');
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                regex.push_str("(.*/)?");
                i += 2;
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

fn resolve_search_base(path: Option<&str>) -> PathBuf {
    let raw = path.unwrap_or(".").trim();
    if raw.is_empty() || raw == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(raw)
    }
}

#[derive(Default)]
struct WalkStats {
    scanned_files: usize,
    skipped_dirs: Vec<String>,
    skipped_large_files: usize,
    unreadable_files: usize,
    skipped_protected_files: usize,
    truncated: bool,
}

fn should_skip_dir(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.starts_with('.') || name == "target" || name == "node_modules" || name == "__pycache__"
}

fn walk_dir(
    dir: &Path,
    results: &mut Vec<PathBuf>,
    max: usize,
    stats: &mut WalkStats,
) -> std::io::Result<()> {
    if results.len() >= max {
        stats.truncated = true;
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if should_skip_dir(&path) {
                if stats.skipped_dirs.len() < 20 {
                    stats.skipped_dirs.push(path.display().to_string());
                }
                continue;
            }
            walk_dir(&path, results, max, stats)?;
        } else {
            results.push(path);
            stats.scanned_files += 1;
        }
        if results.len() >= max {
            stats.truncated = true;
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
        "Search for files matching a glob pattern. Supports *, **, and ? wildcards. Returns diagnostics for empty results, skipped directories, and traversal truncation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. '**/*.rs', 'src/**/*.toml'." },
                "path": { "type": "string", "description": "Base directory to search in (default: current directory)." },
                "include_metadata": { "type": "boolean", "description": "If true, include file size and last-modified time for each match (default: false)." }
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

        let mut stats = WalkStats::default();
        let mut files = Vec::new();
        if base.is_file() {
            stats.scanned_files = 1;
            files.push(base.clone());
        } else {
            walk_dir(&base, &mut files, 1000, &mut stats)?;
        }

        let matches: Vec<PathBuf> = files
            .iter()
            .filter_map(|f| {
                let rel = f.strip_prefix(&base).unwrap_or(f);
                let rel_str = rel.to_string_lossy();
                if re.is_match(&rel_str) {
                    Some(f.clone())
                } else {
                    None
                }
            })
            .collect();

        let mut output = format!(
            "glob_search diagnostics: base={} base_exists=true pattern={} regex={} scanned_files={} skipped_dirs={} traversal_truncated={} include_metadata={}\n",
            base.display(),
            pattern,
            regex_str,
            stats.scanned_files,
            stats.skipped_dirs.len(),
            stats.truncated,
            include_metadata
        );
        if !stats.skipped_dirs.is_empty() {
            output.push_str(&format!("skipped_dirs_sample={}\n", stats.skipped_dirs.join(", ")));
        }

        if matches.is_empty() {
            output.push_str("matches=0\nNo results found.");
        } else {
            let display_limit = 500;
            output.push_str(&format!("matches={}\n", matches.len()));
            for f in matches.iter().take(display_limit) {
                if include_metadata {
                    let meta_info = std::fs::metadata(f)
                        .ok()
                        .map(|m| {
                            let size = m.len();
                            let mtime = m
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            format!("  [{} bytes, mtime={}]", size, mtime)
                        })
                        .unwrap_or_default();
                    output.push_str(&format!("{}{}\n", f.display(), meta_info));
                } else {
                    output.push_str(&format!("{}\n", f.display()));
                }
            }
            if matches.len() > display_limit {
                output.push_str(&format!("... (display_truncated at {} results)", display_limit));
            }
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[derive(Default)]
pub struct ContentSearchTool;

impl ContentSearchTool {
    pub fn new() -> Self {
        Self
    }
}

fn search_in_file(
    path: &Path,
    re: &regex::Regex,
    max_lines: usize,
    context_lines: usize,
    max_line_chars: usize,
    match_window_chars: usize,
) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(path).ok()?;
    if !re.is_match(&content) {
        return None;
    }

    let lines: Vec<&str> = content.lines().collect();
    let line_byte_offsets = line_byte_offsets(&content);
    let mut results = Vec::new();
    let mut prev_match_end: Option<usize> = None;
    let mut match_count = 0;

    for (i, line) in lines.iter().enumerate() {
        if !re.is_match(line) {
            continue;
        }

        let ctx_start = i.saturating_sub(context_lines);
        let ctx_end = (i + context_lines + 1).min(lines.len());
        let start_from = match prev_match_end {
            Some(prev_end) if prev_end >= ctx_start => prev_end,
            _ => {
                if context_lines > 0 && ctx_start > 0 {
                    results.push(format!("{}-{}-\t...", path.display(), ctx_start));
                }
                ctx_start
            }
        };

        for (j, context_line) in lines.iter().enumerate().take(ctx_end).skip(start_from) {
            if j == i {
                if display_char_count(context_line) > max_line_chars {
                    let mut line_matches = 0;
                    let matches_on_line = re.find_iter(context_line).count();
                    for mat in re.find_iter(context_line) {
                        let col = context_line[..mat.start()].chars().count() + 1;
                        let byte_offset = line_byte_offsets.get(j).copied().unwrap_or(0) + mat.start();
                        results.push(format!(
                            "{}:{}:{} [byte {}]\t{}",
                            path.display(),
                            j + 1,
                            col,
                            byte_offset,
                            match_window(context_line, mat.start(), mat.end(), match_window_chars)
                        ));
                        match_count += 1;
                        line_matches += 1;
                        if match_count >= max_lines || line_matches >= 3 {
                            break;
                        }
                    }
                    if matches_on_line > 3 {
                        results.push(format!(
                            "{}:{}\t... (line has more matches; showing first 3 windows)",
                            path.display(),
                            j + 1
                        ));
                    }
                } else {
                    let first_match = re.find(context_line);
                    let suffix = first_match
                        .map(|mat| {
                            let col = context_line[..mat.start()].chars().count() + 1;
                            let byte_offset = line_byte_offsets.get(j).copied().unwrap_or(0) + mat.start();
                            format!(":{} [byte {}]", col, byte_offset)
                        })
                        .unwrap_or_default();
                    results.push(format!("{}:{}{}\t{}", path.display(), j + 1, suffix, context_line));
                    match_count += 1;
                }
            } else {
                results.push(format!(
                    "{}:{}-\t{}",
                    path.display(),
                    j + 1,
                    truncate_chars(context_line, max_line_chars)
                ));
            }

            if match_count >= max_lines {
                break;
            }
        }

        prev_match_end = Some(ctx_end);
        if match_count >= max_lines {
            break;
        }
    }

    if results.is_empty() { None } else { Some(results) }
}

fn line_byte_offsets(content: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        offsets.push(offset);
        offset += line.len();
    }
    if content.is_empty() || !content.ends_with('\n') {
        offsets.push(offset);
    }
    offsets
}

fn display_char_count(s: &str) -> usize {
    s.chars().count()
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn match_window(line: &str, match_start: usize, match_end: usize, window_chars: usize) -> String {
    let start = byte_index_chars_before(line, match_start, window_chars);
    let end = byte_index_chars_after(line, match_end, window_chars);
    let mut out = String::new();
    if start > 0 {
        out.push_str("...");
    }
    out.push_str(&line[start..end]);
    if end < line.len() {
        out.push_str("...");
    }
    out
}

fn byte_index_chars_before(s: &str, byte_idx: usize, chars_before: usize) -> usize {
    if chars_before == 0 {
        return byte_idx;
    }
    s[..byte_idx]
        .char_indices()
        .rev()
        .nth(chars_before.saturating_sub(1))
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn byte_index_chars_after(s: &str, byte_idx: usize, chars_after: usize) -> usize {
    if chars_after == 0 {
        return byte_idx;
    }
    s[byte_idx..]
        .char_indices()
        .nth(chars_after)
        .map(|(idx, _)| byte_idx + idx)
        .unwrap_or(s.len())
}

#[async_trait]
impl Tool for ContentSearchTool {
    fn name(&self) -> &str {
        "content_search"
    }

    fn description(&self) -> &str {
        "Search file contents by regex pattern. Returns matching lines with file path, line numbers, columns, byte offsets, and diagnostics for empty results/skipped files."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "regex": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "File or directory to search in (default: current directory)." },
                "include": { "type": "string", "description": "File name glob filter, e.g. '*.rs', '*.{rs,toml}'." },
                "max_results": { "type": "integer", "description": "Maximum number of matching lines to return (default 200)." },
                "context_lines": { "type": "integer", "description": "Number of context lines to show before and after each match (like grep -C). Default: 0." },
                "max_line_chars": { "type": "integer", "description": "Maximum characters to output for any single line before switching to match-window snippets (default 1000)." },
                "match_window_chars": { "type": "integer", "description": "For long lines, chars before/after each regex match (default 200)." }
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
        let max_results = args["max_results"].as_u64().unwrap_or(200).max(1) as usize;
        let context_lines = args["context_lines"].as_u64().unwrap_or(0) as usize;
        let max_line_chars = args["max_line_chars"].as_u64().unwrap_or(1000).max(80) as usize;
        let match_window_chars = args["match_window_chars"].as_u64().unwrap_or(200).max(20) as usize;

        let re = regex::Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("invalid regex '{}': {}", pattern, e))?;
        let include_regex_str = include.map(|inc| {
            inc.replace('.', r"\.")
                .replace('*', ".*")
                .replace('{', "(")
                .replace('}', ")")
                .replace(',', "|")
        });
        let include_re = include_regex_str
            .as_ref()
            .map(|s| regex::Regex::new(&format!("^{}$", s)))
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid include glob: {}", e))?;

        let base = resolve_search_base(args["path"].as_str());
        if !base.exists() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("path '{}' does not exist", base.display())),
            });
        }

        let mut stats = WalkStats::default();
        let mut all_files = Vec::new();
        if base.is_file() {
            stats.scanned_files = 1;
            all_files.push(base.clone());
        } else {
            walk_dir(&base, &mut all_files, 5000, &mut stats)?;
        }

        let mut results: Vec<String> = Vec::new();
        let mut candidate_files = 0usize;
        for file_path in &all_files {
            if let Some(ref inc_re) = include_re {
                let name = file_path.file_name().unwrap_or_default().to_string_lossy();
                if !inc_re.is_match(&name) {
                    continue;
                }
            }
            // Same backstop as file_read: never surface the contents of a
            // protected path (~/.ssh/**, **/.env, ...) through a recursive
            // content search either — content_search reads file bytes just
            // like file_read does, at the same auto-approved trust tier.
            let abs_file_path = if file_path.is_absolute() {
                file_path.clone()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(file_path)
            };
            if crate::config::is_path_protected(&abs_file_path) {
                stats.skipped_protected_files += 1;
                continue;
            }
            if let Ok(meta) = std::fs::metadata(file_path) {
                if meta.len() > 5_000_000 {
                    stats.skipped_large_files += 1;
                    continue;
                }
            }
            candidate_files += 1;
            let current_match_count = results
                .iter()
                .filter(|r| !r.contains(":-\t") && !r.contains(":\t...") && !r.contains("-\t"))
                .count();
            if current_match_count >= max_results {
                break;
            }
            match std::fs::read_to_string(file_path) {
                Ok(content) => {
                    if !re.is_match(&content) {
                        continue;
                    }
                }
                Err(_) => {
                    stats.unreadable_files += 1;
                    continue;
                }
            }
            if let Some(matches) = search_in_file(
                file_path,
                &re,
                max_results - current_match_count,
                context_lines,
                max_line_chars,
                match_window_chars,
            ) {
                results.extend(matches);
            }
        }

        let matched_lines = results
            .iter()
            .filter(|r| !r.contains(":-\t") && !r.contains(":\t...") && !r.contains("-\t"))
            .count();
        let display_truncated = matched_lines >= max_results;
        let mut output = format!(
            "content_search diagnostics: base={} base_exists=true regex={} include={} scanned_files={} candidate_files={} skipped_dirs={} skipped_large_files={} unreadable_files={} skipped_protected_files={} traversal_truncated={} result_truncated={} max_results={}\n",
            base.display(),
            pattern,
            include.unwrap_or("<none>"),
            stats.scanned_files,
            candidate_files,
            stats.skipped_dirs.len(),
            stats.skipped_large_files,
            stats.unreadable_files,
            stats.skipped_protected_files,
            stats.truncated,
            display_truncated,
            max_results
        );
        if !stats.skipped_dirs.is_empty() {
            output.push_str(&format!("skipped_dirs_sample={}\n", stats.skipped_dirs.join(", ")));
        }

        if results.is_empty() {
            output.push_str("matches=0\nNo results found.");
        } else {
            output.push_str(&format!("matches={}\n", matched_lines));
            output.push_str(&results.join("\n"));
            if display_truncated {
                output.push_str(&format!("\n... (truncated at {} results)", max_results));
            }
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod protected_path_tests {
    use super::*;

    #[tokio::test]
    async fn content_search_skips_protected_files() {
        let dir = tempfile::tempdir().unwrap();
        // `**/.env` is a default protected pattern regardless of directory,
        // so this doesn't depend on the real $HOME the way `~/.ssh/**` would.
        std::fs::write(dir.path().join(".env"), "SECRET_KEY=do-not-leak").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "SECRET_KEY appears here too").unwrap();

        let tool = ContentSearchTool::new();
        let result = tool
            .execute(
                json!({"regex": "SECRET_KEY", "path": dir.path().to_str().unwrap()}),
                &crate::agents::session::Session::new("test".to_string()),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(
            !result.output.contains("do-not-leak"),
            "protected .env contents leaked into search results: {}",
            result.output
        );
        assert!(
            result.output.contains("notes.txt"),
            "unprotected file's match should still be found: {}",
            result.output
        );
        assert!(
            result.output.contains("skipped_protected_files=1"),
            "diagnostics should report the skipped protected file: {}",
            result.output
        );
    }
}
