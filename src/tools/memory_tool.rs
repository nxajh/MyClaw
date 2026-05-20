//! Memory tools — persistent memory CRUD with validation.
//!
//! Four tools:
//! - `memory_list`: list all memory entries with metadata
//! - `memory_view`: read a specific memory file's full content
//! - `memory_search`: keyword search across all memory files
//! - `memory_manage`: add / replace / remove entries with validation

use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::providers::{Tool, ToolResult};

// ── Shared types ──────────────────────────────────────────────────────────

const MAX_CONTENT_CHARS: usize = 10_000;
const MAX_ABSTRACT_CHARS: usize = 500;
const MAX_NAME_LENGTH: usize = 64;

/// Validate a memory file name.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Name cannot be empty.".to_string());
    }
    if name.len() > MAX_NAME_LENGTH {
        return Err(format!("Name too long (max {} chars).", MAX_NAME_LENGTH));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!("Invalid name '{}'. Use only a-z, 0-9, _, -", name));
    }
    if name.contains("..") {
        return Err("'..' not allowed in name.".to_string());
    }
    Ok(())
}

/// Scan memory directory and return parsed entries.
fn scan_entries(memory_dir: &Path) -> Vec<crate::memory::IndexEntry> {
    let files = crate::memory::scan_memory_files(memory_dir);
    files.iter().map(crate::memory::IndexEntry::from).collect()
}

/// Resolve memory directory from the tool's knowledge_dir.
struct MemoryPaths {
    memory_dir: PathBuf,
}

impl MemoryPaths {
    fn new(knowledge_dir: &str) -> Result<Self, String> {
        let dir = PathBuf::from(knowledge_dir).join(crate::memory::MEMORY_DIR_NAME);
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("Failed to create memory dir: {}", e))?;
        }
        Ok(Self { memory_dir: dir })
    }
}

// ── Threat patterns for memory content scanning ──────────────────────────

fn scan_memory_content(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    let patterns = [
        ("ignore previous instructions", "prompt_injection"),
        ("ignore all instructions", "prompt_injection"),
        ("ignore above instructions", "prompt_injection"),
        ("system prompt override", "sys_prompt_override"),
        ("disregard your instructions", "disregard_rules"),
        ("do not tell the user", "deception_hide"),
    ];
    for (pattern, label) in &patterns {
        if lower.contains(pattern) {
            return Some(format!(
                "Blocked: content matches threat pattern '{}'. \
                 Memory content is injected into the system prompt and must not \
                 contain injection payloads.",
                label
            ));
        }
    }
    None
}

fn scan_memory_content_opt(content: &str) -> Result<(), String> {
    match scan_memory_content(content) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn atomic_write(target: &Path, content: &str) -> std::io::Result<()> {
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

/// Build frontmatter string.
fn build_frontmatter(name: &str, abstract_text: &str, tags: &[String], mem_type: &crate::memory::MemoryType, created_at: &str) -> String {
    let mut fm = format!(
        "---\nname: {}\nsummary: \"{}\"\ntype: {}\ncreated_at: {}",
        name, abstract_text, mem_type.as_str(), created_at
    );
    if !tags.is_empty() {
        fm.push_str(&format!("\ntags: [{}]", tags.join(", ")));
    }
    fm.push_str("\n---\n\n");
    fm
}

// ══════════════════════════════════════════════════════════════════════════
// memory_list
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryListTool {
    knowledge_dir: String,
}

impl MemoryListTool {
    pub fn new(knowledge_dir: String) -> Self {
        Self { knowledge_dir }
    }
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "memory_list"
    }

    fn description(&self) -> &str {
        "List all memory entries with metadata. Use this to browse what the agent remembers, \
         check if a specific fact is stored, or find entries to update/remove."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let paths = match MemoryPaths::new(&self.knowledge_dir) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": e}).to_string(),
                error: None,
            }),
        };

        let entries = scan_entries(&paths.memory_dir);

        if entries.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "success": true,
                    "count": 0,
                    "entries": [],
                    "hint": "Use memory_manage(action='add') to create your first memory."
                }).to_string(),
                error: None,
            });
        }

        let json_entries: Vec<serde_json::Value> = entries.iter().map(|e| {
            let mut obj = json!({
                "type": e.mem_type.as_str(),
                "name": e.name,
                "summary": e.abstract_text,
            });
            if !e.tags.is_empty() {
                obj["tags"] = json!(e.tags);
            }
            obj
        }).collect();

        Ok(ToolResult {
            success: true,
            output: json!({
                "success": true,
                "count": entries.len(),
                "entries": json_entries,
                "hint": "Use memory_view(name) to read full content, or memory_search(query) to search."
            }).to_string(),
            error: None,
        })
    }
}

// ══════════════════════════════════════════════════════════════════════════
// memory_view
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryViewTool {
    knowledge_dir: String,
}

impl MemoryViewTool {
    pub fn new(knowledge_dir: String) -> Self {
        Self { knowledge_dir }
    }
}

#[async_trait]
impl Tool for MemoryViewTool {
    fn name(&self) -> &str {
        "memory_view"
    }

    fn description(&self) -> &str {
        "Read a memory entry's full content. Provide the memory name (not filename)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the memory entry to read."
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => return Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": "'name' is required."}).to_string(),
                error: None,
            }),
        };

        let paths = match MemoryPaths::new(&self.knowledge_dir) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": e}).to_string(),
                error: None,
            }),
        };

        let files = crate::memory::scan_memory_files(&paths.memory_dir);
        let file = files.iter().find(|f| f.name == name);

        match file {
            Some(mf) => {
                let mut output = json!({
                    "success": true,
                    "name": mf.name,
                    "type": mf.mem_type.as_str(),
                    "summary": mf.abstract_text,
                    "created_at": mf.created_at,
                    "content": mf.content,
                });
                if !mf.tags.is_empty() {
                    output["tags"] = json!(mf.tags);
                }
                Ok(ToolResult {
                    success: true,
                    output: output.to_string(),
                    error: None,
                })
            }
            None => {
                let available: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                Ok(ToolResult {
                    success: false,
                    output: json!({
                        "success": false,
                        "error": format!("Memory '{}' not found.", name),
                        "available": available,
                        "hint": "Use memory_list() to see all entries."
                    }).to_string(),
                    error: None,
                })
            }
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// memory_search
// ══════════════════════════════════════════════════════════════════════════

pub struct MemorySearchTool {
    knowledge_dir: String,
}

impl MemorySearchTool {
    pub fn new(knowledge_dir: String) -> Self {
        Self { knowledge_dir }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search memory entries by keyword. Searches across name, abstract, tags, and content. \
         Returns matching entries with relevance info."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. Matches against name, abstract, tags, and content."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = match args["query"].as_str() {
            Some(q) => q.to_lowercase(),
            None => return Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": "'query' is required."}).to_string(),
                error: None,
            }),
        };

        let paths = match MemoryPaths::new(&self.knowledge_dir) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": e}).to_string(),
                error: None,
            }),
        };

        let files = crate::memory::scan_memory_files(&paths.memory_dir);

        // Scoring: name=3, tags=2, abstract=2, content=1
        let mut results: Vec<(i32, &crate::memory::MemoryFile)> = Vec::new();
        for mf in &files {
            let mut score = 0i32;
            if mf.name.to_lowercase().contains(&query) {
                score += 3;
            }
            if mf.tags.iter().any(|t| t.to_lowercase().contains(&query)) {
                score += 2;
            }
            if mf.abstract_text.to_lowercase().contains(&query) {
                score += 2;
            }
            if mf.content.to_lowercase().contains(&query) {
                score += 1;
            }
            if score > 0 {
                results.push((score, mf));
            }
        }

        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "success": true,
                    "count": 0,
                    "results": [],
                    "hint": "No matching memories. Try different keywords or use memory_list() to browse all."
                }).to_string(),
                error: None,
            });
        }

        results.sort_by(|a, b| b.0.cmp(&a.0));

        let json_results: Vec<serde_json::Value> = results.iter().map(|(score, mf)| {
            let content_lower = mf.content.to_lowercase();
            let snippet = if let Some(pos) = content_lower.find(&query) {
                let start = pos.saturating_sub(40);
                let end = (pos + query.len() + 60).min(mf.content.len());
                let mut s = String::new();
                if start > 0 { s.push_str("..."); }
                s.push_str(&mf.content[start..end]);
                if end < mf.content.len() { s.push_str("..."); }
                s
            } else if mf.abstract_text.to_lowercase().contains(&query) {
                mf.abstract_text.clone()
            } else {
                mf.content.chars().take(80).collect()
            };

            let mut result = json!({
                "name": mf.name,
                "type": mf.mem_type.as_str(),
                "summary": mf.abstract_text,
                "snippet": snippet,
                "relevance": score,
            });
            if !mf.tags.is_empty() {
                result["tags"] = json!(mf.tags);
            }
            result
        }).collect();

        Ok(ToolResult {
            success: true,
            output: json!({
                "success": true,
                "count": json_results.len(),
                "results": json_results,
            }).to_string(),
            error: None,
        })
    }
}

// ══════════════════════════════════════════════════════════════════════════
// memory_manage
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryManageTool {
    knowledge_dir: String,
}

impl MemoryManageTool {
    pub fn new(knowledge_dir: String) -> Self {
        Self { knowledge_dir }
    }
}

#[async_trait]
impl Tool for MemoryManageTool {
    fn name(&self) -> &str {
        "memory_manage"
    }

    fn description(&self) -> &str {
        "Manage persistent memories. Actions: add (create new entry), replace (update existing), \
         remove (delete entry).\n\nUse add when: user asks to remember something, you discover a \
         stable preference or fact.\nUse replace when: existing memory needs updating.\n\
         Use remove when: memory is stale or wrong.\n\nConfirm with user before removing memories."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "replace", "remove"],
                    "description": "Action to perform."
                },
                "name": {
                    "type": "string",
                    "description": "Memory entry name (used as identifier). Lowercase, hyphens allowed."
                },
                "content": {
                    "type": "string",
                    "description": "Memory content. Required for add and replace."
                },
                "memory_type": {
                    "type": "string",
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "Category. user=preferences (always injected), feedback=behavior corrections (always injected), \
                     project=project context (on-demand), reference=external references (on-demand). Default: project."
                },
                "summary": {
                    "type": "string",
                    "description": "Brief summary of the key content (1-2 sentences). \
                     Required for user/feedback types (injected into system prompt). \
                     Auto-generated from content if omitted."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tags for filtering and categorization, e.g. [\"rust\", \"qqbot\", \"bug\"]."
                }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");

        let result = match action {
            "add" => self.action_add(name, &args),
            "replace" => self.action_replace(name, &args),
            "remove" => self.action_remove(name),
            _ => Err(format!(
                "Unknown action '{}'. Use: add, replace, remove", action
            )),
        };

        match result {
            Ok(v) => Ok(ToolResult { success: true, output: v.to_string(), error: None }),
            Err(msg) => Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": msg}).to_string(),
                error: None,
            }),
        }
    }
}

impl MemoryManageTool {
    fn action_add(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        validate_name(name)?;

        let content = args["content"].as_str()
            .ok_or("'content' is required for add.")?;
        if content.trim().is_empty() {
            return Err("Content cannot be empty.".to_string());
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!("Content exceeds {} character limit.", MAX_CONTENT_CHARS));
        }

        scan_memory_content_opt(content)?;

        let paths = MemoryPaths::new(&self.knowledge_dir)?;

        let files = crate::memory::scan_memory_files(&paths.memory_dir);
        if files.iter().any(|f| f.name == name) {
            return Err(format!(
                "Memory '{}' already exists. Use 'replace' to update it.", name
            ));
        }

        let mem_type = self.resolve_type(args);
        let abstract_text = self.resolve_abstract(args, content);
        let tags = self.resolve_tags(args);
        let filename = format!("{}.md", name);
        let now = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let frontmatter = build_frontmatter(name, &abstract_text, &tags, &mem_type, &now);
        let file_content = format!("{}{}", frontmatter, content);

        let target = paths.memory_dir.join(&filename);
        atomic_write(&target, &file_content)
            .map_err(|e| format!("Failed to write memory file: {}", e))?;

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' added.", name),
            "name": name,
            "type": mem_type.as_str(),
            "summary": abstract_text,
            "tags": tags,
        }))
    }

    fn action_replace(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        validate_name(name)?;

        let paths = MemoryPaths::new(&self.knowledge_dir)?;
        let files = crate::memory::scan_memory_files(&paths.memory_dir);
        let existing = files.iter().find(|f| f.name == name)
            .ok_or_else(|| format!("Memory '{}' not found.", name))?;

        let content = args["content"].as_str()
            .ok_or("'content' is required for replace.")?;
        if content.trim().is_empty() {
            return Err("Content cannot be empty.".to_string());
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!("Content exceeds {} character limit.", MAX_CONTENT_CHARS));
        }

        scan_memory_content_opt(content)?;

        // Preserve existing metadata unless overridden
        let mem_type = args["memory_type"].as_str()
            .and_then(crate::memory::MemoryType::from_str_lossy)
            .unwrap_or(existing.mem_type);
        let abstract_text = if args["summary"].as_str().is_some() {
            self.resolve_abstract(args, content)
        } else {
            existing.abstract_text.clone()
        };
        let tags = if args["tags"].is_array() {
            self.resolve_tags(args)
        } else {
            existing.tags.clone()
        };
        let filename = existing.path.file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or(name);
        let fallback_filename = format!("{}.md", name);
        let filename = if filename == name { &fallback_filename } else { filename };
        let now = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let frontmatter = build_frontmatter(name, &abstract_text, &tags, &mem_type, &now);
        let file_content = format!("{}{}", frontmatter, content);

        let target = paths.memory_dir.join(filename);
        atomic_write(&target, &file_content)
            .map_err(|e| format!("Failed to write memory file: {}", e))?;

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' updated.", name),
            "name": name,
        }))
    }

    fn action_remove(&self, name: &str) -> Result<serde_json::Value, String> {
        validate_name(name)?;

        let paths = MemoryPaths::new(&self.knowledge_dir)?;
        let files = crate::memory::scan_memory_files(&paths.memory_dir);
        let existing = files.iter().find(|f| f.name == name)
            .ok_or_else(|| format!("Memory '{}' not found.", name))?;

        std::fs::remove_file(&existing.path)
            .map_err(|e| format!("Failed to remove memory file: {}", e))?;

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' removed.", name),
        }))
    }

    fn resolve_type(&self, args: &serde_json::Value) -> crate::memory::MemoryType {
        args["memory_type"].as_str()
            .and_then(crate::memory::MemoryType::from_str_lossy)
            .unwrap_or(crate::memory::MemoryType::Project)
    }

    /// Resolve abstract: explicit parameter, or auto-generate from content.
    fn resolve_abstract(&self, args: &serde_json::Value, content: &str) -> String {
        if let Some(abs) = args["summary"].as_str() {
            let trimmed = abs.trim();
            if !trimmed.is_empty() {
                if trimmed.chars().count() > MAX_ABSTRACT_CHARS {
                    let truncated: String = trimmed.chars().take(MAX_ABSTRACT_CHARS).collect();
                    return truncated;
                }
                return trimmed.to_string();
            }
        }
        // Auto-generate from first non-empty line of content
        let first_line = content.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(content);
        let truncated: String = first_line.chars().take(200).collect();
        truncated
    }

    fn resolve_tags(&self, args: &serde_json::Value) -> Vec<String> {
        match args["tags"].as_array() {
            Some(arr) => arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => Vec::new(),
        }
    }
}
