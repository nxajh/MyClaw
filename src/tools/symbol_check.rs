//! Symbol/code audit tool.

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct SymbolCheckTool;

impl SymbolCheckTool {
    pub fn new() -> Self {
        Self
    }
}

fn resolve_base(path: Option<&str>) -> PathBuf {
    let raw = path.unwrap_or(".").trim();
    if raw.is_empty() || raw == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(raw)
    }
}

fn skip_dir(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.starts_with('.') || name == "target" || name == "node_modules" || name == "__pycache__"
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, max: usize) -> std::io::Result<bool> {
    if out.len() >= max {
        return Ok(true);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if skip_dir(&path) {
                continue;
            }
            if collect_files(&path, out, max)? {
                return Ok(true);
            }
        } else {
            out.push(path);
            if out.len() >= max {
                return Ok(true);
            }
        }
    }
    Ok(false)
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

fn definition_regex(symbol: &str) -> anyhow::Result<regex::Regex> {
    let escaped = regex::escape(symbol);
    regex::Regex::new(&format!(
        r"(?m)^\s*(?:pub\s+)?(?:(?:async\s+)?fn|struct|enum|trait|type|mod|const|static)\s+{}\b|^\s*impl(?:<[^>]+>)?\s+{}\b|^\s*(?:export\s+)?(?:(?:async\s+)?function|class|interface|type|const)\s+{}\b|^\s*(?:def|class)\s+{}\b",
        escaped, escaped, escaped, escaped
    )).map_err(|e| anyhow::anyhow!("invalid symbol regex for '{}': {}", symbol, e))
}

fn word_regex(symbol: &str) -> anyhow::Result<regex::Regex> {
    regex::Regex::new(&format!(r"\b{}\b", regex::escape(symbol)))
        .map_err(|e| anyhow::anyhow!("invalid reference regex for '{}': {}", symbol, e))
}

fn first_match_line(content: &str, re: &regex::Regex) -> Option<(usize, String)> {
    for (i, line) in content.lines().enumerate() {
        if re.is_match(line) {
            return Some((i + 1, line.trim().to_string()));
        }
    }
    None
}

#[async_trait]
impl Tool for SymbolCheckTool {
    fn name(&self) -> &str {
        "symbol_check"
    }

    fn description(&self) -> &str {
        "Check whether documented identifiers or symbols exist in code. Returns present / referenced-only / missing plus path and line evidence."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbols": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Identifiers or symbols to check."
                },
                "path": {
                    "type": "string",
                    "description": "Base file or directory to search in (default: current directory)."
                },
                "include": {
                    "type": "string",
                    "description": "File path glob filter relative to base, e.g. 'src/**/*.rs' or '**/*.{ts,tsx}'. Default searches common source files."
                },
                "max_files": {
                    "type": "integer",
                    "description": "Maximum files to scan (default 5000)."
                }
            },
            "required": ["symbols"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        4_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let symbols_val = args["symbols"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("'symbols' must be an array"))?;
        let symbols: Vec<String> = symbols_val
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if symbols.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("no symbols provided".to_string()),
            });
        }

        let base = resolve_base(args["path"].as_str());
        if !base.exists() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("path '{}' does not exist", base.display())),
            });
        }
        let max_files = args["max_files"].as_u64().unwrap_or(5000).max(1) as usize;
        let include = args["include"].as_str();
        let include_re = include
            .map(glob_to_regex)
            .map(|s| regex::Regex::new(&s))
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid include glob: {}", e))?;

        let mut files = Vec::new();
        let truncated = if base.is_file() {
            files.push(base.clone());
            false
        } else {
            collect_files(&base, &mut files, max_files)?
        };

        let default_exts: HashSet<&str> = ["rs", "py", "js", "ts", "jsx", "tsx", "mjs", "go", "java", "c", "cc", "cpp", "h", "hpp"]
            .into_iter()
            .collect();
        let mut scanned = 0usize;
        let mut contents = Vec::new();
        for file in files {
            let rel = file.strip_prefix(&base).unwrap_or(&file).to_string_lossy();
            if let Some(ref re) = include_re {
                if !re.is_match(&rel) {
                    continue;
                }
            } else {
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !default_exts.contains(ext) {
                    continue;
                }
            }
            if std::fs::metadata(&file).map(|m| m.len() > 5_000_000).unwrap_or(false) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&file) {
                scanned += 1;
                contents.push((file, content));
            }
        }

        let mut out = format!(
            "symbol_check: base={} scanned_files={} requested_symbols={} truncated={} include={}\n",
            base.display(),
            scanned,
            symbols.len(),
            truncated,
            include.unwrap_or("<common source files>")
        );

        for symbol in symbols {
            let def_re = definition_regex(&symbol)?;
            let ref_re = word_regex(&symbol)?;
            let mut definition = None;
            let mut reference = None;
            let mut ref_count = 0usize;
            for (file, content) in &contents {
                if definition.is_none() {
                    if let Some((line, text)) = first_match_line(content, &def_re) {
                        definition = Some((file.display().to_string(), line, text));
                    }
                }
                if ref_re.is_match(content) {
                    ref_count += ref_re.find_iter(content).count();
                    if reference.is_none() {
                        if let Some((line, text)) = first_match_line(content, &ref_re) {
                            reference = Some((file.display().to_string(), line, text));
                        }
                    }
                }
            }
            match (definition, reference) {
                (Some((path, line, text)), _) => out.push_str(&format!(
                    "present\t{}\t{}:{}\t{}\n",
                    symbol, path, line, text
                )),
                (None, Some((path, line, text))) => out.push_str(&format!(
                    "referenced-only\t{}\treferences={}\tfirst={}{}{}\t{}\n",
                    symbol, ref_count, path, ":", line, text
                )),
                (None, None) => out.push_str(&format!("missing\t{}\n", symbol)),
            }
        }

        Ok(ToolResult {
            success: true,
            output: out,
            error: None,
        })
    }
}
