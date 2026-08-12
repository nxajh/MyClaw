//! Memory tools — persistent memory CRUD with validation.
//!
//! Four tools:
//! - `memory_list`: list all memory entries with metadata
//! - `memory_view`: read a specific memory file's full content
//! - `memory_search`: keyword search across all memory files
//! - `memory_manage`: add / replace / remove entries with validation

use async_trait::async_trait;
use serde_json::json;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::session::Session;
use crate::agents::user_profile::UserResolver;
use crate::memory::{LinkRef, MemoryFile, build_backlinks};
use crate::providers::{Tool, ToolResult};

// ── Shared types ──────────────────────────────────────────────────────────

const MAX_CONTENT_CHARS: usize = 10_000;
const MAX_DESCRIPTION_CHARS: usize = 500;
const MAX_NAME_LENGTH: usize = 64;
const MEMORY_AUDIT_LOG: &str = "memory_audit.jsonl";

fn short_sha256(content: &str) -> String {
    let hash = crate::providers::capability_chat::sha256_hex(content.as_bytes());
    hash.chars().take(16).collect()
}

fn redact_audit_reason(reason: Option<&str>) -> Option<String> {
    reason
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(500).collect())
}

struct MemoryAudit<'a> {
    user_id: &'a str,
    scope: &'a str,
    action: &'a str,
    name: &'a str,
    old_hash: Option<String>,
    new_hash: Option<String>,
    args: &'a serde_json::Value,
}

fn append_memory_audit(workspace_dir: &Path, session: &Session, audit: MemoryAudit<'_>) {
    let audit_dir = workspace_dir
        .join(crate::memory::MEMORY_DIR_NAME)
        .join(".audit");
    if let Err(e) = std::fs::create_dir_all(&audit_dir) {
        tracing::warn!(err = %e, "memory audit: failed to create audit dir");
        return;
    }

    let entry = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "session_id": session.id,
        "session_owner": session.owner,
        "user_id": audit.user_id,
        "scope": audit.scope,
        "action": audit.action,
        "memory_name": audit.name,
        "old_hash": audit.old_hash,
        "new_hash": audit.new_hash,
        "reason": redact_audit_reason(audit.args["reason"].as_str()),
        "model": audit.args["model"].as_str(),
        "source": "memory_manage",
    });

    let path = audit_dir.join(MEMORY_AUDIT_LOG);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{}", entry) {
                tracing::warn!(
                    err = %e,
                    path = %path.display(),
                    "memory audit: failed to write entry"
                );
            }
        }
        Err(e) => tracing::warn!(
            err = %e,
            path = %path.display(),
            "memory audit: failed to open log"
        ),
    }
}

/// Validate a memory file name.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Name cannot be empty.".to_string());
    }
    if name.len() > MAX_NAME_LENGTH {
        return Err(format!("Name too long (max {} chars).", MAX_NAME_LENGTH));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("Invalid name '{}'. Use only a-z, 0-9, _, -", name));
    }
    if name.contains("..") {
        return Err("'..' not allowed in name.".to_string());
    }
    Ok(())
}

/// Scan memory files from both global and per-user directories, deduplicated by name.
fn scan_merged(workspace_dir: &Path, user_id: &str) -> Vec<crate::memory::MemoryFile> {
    let global_dir = workspace_dir.join(crate::memory::MEMORY_DIR_NAME);
    let user_dir = workspace_dir
        .join("users")
        .join(user_id)
        .join(crate::memory::MEMORY_DIR_NAME);

    let mut files = crate::memory::scan_memory_files(&global_dir);
    let global_names: std::collections::HashSet<String> =
        files.iter().map(|f| f.name.clone()).collect();

    for f in crate::memory::scan_memory_files(&user_dir) {
        if !global_names.contains(&f.name) {
            files.push(f);
        }
    }
    files
}

fn normalize_optional_filter(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
}

fn query_tokens(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    query
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '，' || c == '；')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

fn ascii_boundary_contains(haystack: &str, needle: &str) -> bool {
    let mut search_start = 0;
    while let Some(relative_pos) = haystack[search_start..].find(needle) {
        let start = search_start + relative_pos;
        let end = start + needle.len();
        let before = haystack[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = haystack[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if !before && !after {
            return true;
        }
        search_start = end;
    }
    false
}

fn token_matches(haystack: &str, token: &str) -> bool {
    if token.chars().all(|c| c.is_ascii_alphanumeric()) {
        ascii_boundary_contains(haystack, token)
    } else {
        haystack.contains(token)
    }
}

fn find_token_match(haystack: &str, token: &str) -> Option<usize> {
    if !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return haystack.find(token);
    }

    let mut search_start = 0;
    while let Some(relative_pos) = haystack[search_start..].find(token) {
        let start = search_start + relative_pos;
        let end = start + token.len();
        let before = haystack[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = haystack[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if !before && !after {
            return Some(start);
        }
        search_start = end;
    }
    None
}

fn field_match_score(field: &str, query: &str, tokens: &[String], weight: i32) -> i32 {
    let lower = field.to_lowercase();
    let mut score = 0;
    if !query.is_empty() && lower.contains(query) {
        score += weight * 3;
    }
    for token in tokens {
        if token_matches(&lower, token) {
            score += weight;
        }
    }
    score
}

fn memory_search_score(mf: &crate::memory::MemoryFile, query: &str, tokens: &[String]) -> i32 {
    let mut score = 0;
    score += field_match_score(&mf.name, query, tokens, 10);
    score += mf
        .tags
        .iter()
        .map(|tag| field_match_score(tag, query, tokens, 8))
        .sum::<i32>();
    score += field_match_score(&mf.description, query, tokens, 6);
    score += mf
        .links
        .iter()
        .map(|link| {
            field_match_score(&link.label, query, tokens, 2)
                + field_match_score(&link.target, query, tokens, 2)
        })
        .sum::<i32>();
    score += field_match_score(&mf.content, query, tokens, 3);
    score
}

fn best_snippet(mf: &crate::memory::MemoryFile, query: &str, tokens: &[String]) -> String {
    let content_lower = mf.content.to_lowercase();
    let needle = if !query.is_empty() && content_lower.contains(query) {
        Some(query.to_string())
    } else {
        tokens
            .iter()
            .find(|token| find_token_match(&content_lower, token.as_str()).is_some())
            .cloned()
    };

    if let Some(needle) = needle {
        if let Some(byte_pos) = find_token_match(&content_lower, &needle) {
            let char_pos = content_lower[..byte_pos].chars().count();
            let needle_chars = needle.chars().count();
            let start = mf
                .content
                .char_indices()
                .nth(char_pos.saturating_sub(40))
                .map(|(i, _)| i)
                .unwrap_or(0);
            let end = mf
                .content
                .char_indices()
                .nth(char_pos + needle_chars + 60)
                .map(|(i, _)| i)
                .unwrap_or(mf.content.len());
            let mut s = String::new();
            if start > 0 {
                s.push_str("...");
            }
            s.push_str(&mf.content[start..end]);
            if end < mf.content.len() {
                s.push_str("...");
            }
            return s;
        }
    }

    if field_match_score(&mf.description, query, tokens, 1) > 0 {
        return mf.description.clone();
    }
    if let Some(link) = mf.links.iter().find(|link| {
        field_match_score(&link.label, query, tokens, 1)
            + field_match_score(&link.target, query, tokens, 1)
            > 0
    }) {
        return format!("See Also: {} -> {}", link.label, link.target);
    }
    mf.content.chars().take(120).collect()
}

/// Common helper: resolve the user_id from a session via the resolver.
fn user_id_for(session: &Session, resolver: &UserResolver) -> String {
    resolver.resolve(&session.owner)
}

/// Resolve the target scope: "user" (default) or "agent".
fn resolve_scope(args: &serde_json::Value) -> &'static str {
    match args["scope"].as_str() {
        Some("agent") => "agent",
        _ => "user",
    }
}

/// Directory for a scope's memory files.
fn scope_memory_dir(workspace_dir: &Path, scope: &str, user_id: &str) -> PathBuf {
    if scope == "agent" {
        workspace_dir.join(crate::memory::MEMORY_DIR_NAME)
    } else {
        workspace_dir
            .join("users")
            .join(user_id)
            .join(crate::memory::MEMORY_DIR_NAME)
    }
}

/// Scan memory files from a single scope (not merged).
fn scan_scope(workspace_dir: &Path, scope: &str, user_id: &str) -> Vec<crate::memory::MemoryFile> {
    crate::memory::scan_memory_files(&scope_memory_dir(workspace_dir, scope, user_id))
}

// ── PII guard for agent-scope writes ─────────────────────────────────────

/// Check content for user-identifying patterns before an agent-scope write.
/// This is a conservative bottom-line guard; the distillation prompt is the
/// primary de-identification mechanism.
fn scan_agent_pii(content: &str) -> Option<String> {
    use regex::Regex;

    let patterns: &[(&str, &str)] = &[
        // Known channel-prefixed routing keys: telegram:myclaw:6270938644,
        // qqbot:xiaoer:E8CAAE..., client:default:web:0f53a6e9-...
        (
            r"\b(?:telegram|qqbot|wechat|whatsapp|slack|discord|client|web|webuser):[a-z0-9_]+:[A-Za-z0-9_:\-]+",
            "routing_key",
        ),
        // Long digit runs (user ids, phone numbers).
        (r"\b\d{8,}\b", "numeric_identifier"),
        // Email addresses.
        (
            r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
            "email",
        ),
        // Chinese mobile numbers (11 digits starting with 1[3-9]).
        (r"\b1[3-9]\d{9}\b", "phone"),
    ];

    // Compile once per call; the set is tiny and this path is low-frequency.
    for (pattern, label) in patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(content) {
                return Some(format!(
                    "Blocked: agent-scope memory contains a user-identifying pattern '{}'. \
                     Agent memories are shared across users and must be de-identified. \
                     Remove routing keys, user ids, emails, and phone numbers before writing, \
                     or use scope='user'.",
                    label
                ));
            }
        }
    }
    None
}

fn scan_agent_pii_opt(content: &str) -> Result<(), String> {
    match scan_agent_pii(content) {
        Some(err) => Err(err),
        None => Ok(()),
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

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}

fn validate_body_only(content: &str) -> Result<(), String> {
    let trimmed = content.trim_start();
    if trimmed.starts_with("---") {
        return Err(
            "Content must be BODY ONLY: do not include YAML frontmatter or `---` blocks."
                .to_string(),
        );
    }
    if content.lines().any(|line| line.trim() == "---") {
        return Err("Content must not contain standalone `---` blocks; memory_manage generates frontmatter automatically.".to_string());
    }
    Ok(())
}

fn link_values(links: &[LinkRef]) -> Vec<serde_json::Value> {
    links
        .iter()
        .map(|l| serde_json::json!({ "target": l.target, "label": l.label }))
        .collect()
}

fn lint_memory_content(name: &str, content: &str, files: &[MemoryFile]) -> Vec<String> {
    let mut warnings = Vec::new();
    let links = crate::memory::extract_links_from_content(content);
    let names: std::collections::HashSet<&str> = files.iter().map(|f| f.name.as_str()).collect();
    for link in &links {
        if link.target != name && !names.contains(link.target.as_str()) {
            warnings.push(format!(
                "See Also link '{}' points to a missing memory.",
                link.target
            ));
        }
    }
    // Detect non-canonical See Also hrefs (bare logical names without .md).
    // extract_links ignores them, so flag any [label](target) in ## See Also
    // whose target is not external and does not end with .md.
    {
        let mut in_see_also = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("## ") {
                in_see_also = trimmed.eq_ignore_ascii_case("## see also");
                continue;
            }
            if !in_see_also {
                continue;
            }
            if let Some(open) = trimmed.find("](") {
                if let Some(close_rel) = trimmed[open + 2..].find(')') {
                    let target = trimmed[open + 2..open + 2 + close_rel].trim();
                    if target.is_empty()
                        || target.starts_with("http://")
                        || target.starts_with("https://")
                        || target.starts_with('#')
                        || target.starts_with("mailto:")
                    {
                        continue;
                    }
                    let segment = target
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(target)
                        .trim();
                    if !segment.to_ascii_lowercase().ends_with(".md") {
                        warnings.push(format!(
                            "See Also link target '{}' must use canonical href `<name>.md` (bare names are not indexed).",
                            target
                        ));
                    }
                }
            }
        }
    }
    if files.len() > 1 && links.is_empty() && !content.to_lowercase().contains("## see also") {
        warnings.push(
            "No See Also links found; add 1-3 links as `[Related: name](name.md)` when applicable."
                .to_string(),
        );
    }
    warnings
}

/// Build frontmatter string.
fn build_frontmatter(
    name: &str,
    description: &str,
    tags: &[String],
    mem_type: &str,
    inject: &str,
    created_at: &str,
    updated_at: Option<&str>,
    source_session: Option<&str>,
) -> String {
    let mut fm = format!(
        "---
name: {}
description: {}
type: {}
inject: {}
created_at: {}",
        yaml_scalar(name),
        yaml_scalar(description),
        yaml_scalar(mem_type),
        inject,
        yaml_scalar(created_at)
    );
    if let Some(ua) = updated_at {
        fm.push_str(&format!(
            "
updated_at: {}",
            yaml_scalar(ua)
        ));
    }
    if let Some(ss) = source_session {
        fm.push_str(&format!(
            "
source_session: {}",
            yaml_scalar(ss)
        ));
    }
    if !tags.is_empty() {
        let tag_values: Vec<String> = tags.iter().map(|t| yaml_scalar(t)).collect();
        fm.push_str(&format!(
            "
tags: [{}]",
            tag_values.join(", ")
        ));
    }
    fm.push_str(
        "
---

",
    );
    fm
}

// ══════════════════════════════════════════════════════════════════════════
// memory_list
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryListTool {
    workspace_dir: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemoryListTool {
    pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            workspace_dir,
            resolver,
        }
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
            "properties": {
                "memory_type": {
                    "type": "string",
                    "description": "Optional type filter. Omit, pass empty string, or whitespace to list all types."
                }
            }
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let type_filter = normalize_optional_filter(args["memory_type"].as_str());
        let user_id = user_id_for(session, &self.resolver);
        let files = scan_merged(&self.workspace_dir, &user_id);
        let entries: Vec<crate::memory::IndexEntry> = files
            .iter()
            .filter(|mf| {
                type_filter
                    .as_ref()
                    .is_none_or(|wanted| mf.mem_type.to_lowercase() == *wanted)
            })
            .map(crate::memory::IndexEntry::from)
            .collect();

        if entries.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "success": true,
                    "count": 0,
                    "entries": [],
                    "hint": "Use memory_manage(action='add') to create your first memory."
                })
                .to_string(),
                error: None,
            });
        }

        let backlinks = build_backlinks(&files);
        let json_entries: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                let backlinks_count = backlinks.get(&e.name).map(|b| b.len()).unwrap_or(0);
                let mut obj = json!({
                    "type": &e.mem_type,
                    "name": &e.name,
                    "description": &e.description,
                    "link_count": e.link_count,
                    "backlink_count": backlinks_count,
                });
                if !e.tags.is_empty() {
                    obj["tags"] = json!(e.tags);
                }
                obj
            })
            .collect();

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
    workspace_dir: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemoryViewTool {
    pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            workspace_dir,
            resolver,
        }
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

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: json!({"success": false, "error": "'name' is required."}).to_string(),
                    error: None,
                });
            }
        };

        let user_id = user_id_for(session, &self.resolver);
        let files = scan_merged(&self.workspace_dir, &user_id);
        let file = files.iter().find(|f| f.name == name);

        match file {
            Some(mf) => {
                let backlinks = build_backlinks(&files);
                let file_backlinks = backlinks.get(&mf.name).cloned().unwrap_or_default();
                let outgoing = link_values(&mf.links);

                let mut output = json!({
                    "success": true,
                    "name": &mf.name,
                    "type": &mf.mem_type,
                    "description": &mf.description,
                    "created_at": &mf.created_at,
                    "content": &mf.content,
                    "links": outgoing,
                    "backlinks": file_backlinks,
                });
                if let Some(ref ss) = mf.source_session {
                    output["source_session"] = json!(ss);
                }
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
                    })
                    .to_string(),
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
    workspace_dir: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemorySearchTool {
    pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            workspace_dir,
            resolver,
        }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search memory entries by keyword. Searches across name, description, tags, content, link labels, and link targets. Returns matching entries with relevance info."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. Matches against name, description, tags, content, link labels, and link targets."
                },
                "memory_type": {
                    "type": "string",
                    "description": "Optional type filter. Omit, pass empty string, or whitespace to search all types."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return (default 20, max 100)."
                },
                "include_related": {
                    "type": "boolean",
                    "description": "If true, include outgoing links and backlinks for each direct result."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let query = match args["query"].as_str() {
            Some(q) => q.trim().to_lowercase(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: json!({"success": false, "error": "'query' is required."}).to_string(),
                    error: None,
                });
            }
        };
        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": "'query' cannot be empty."}).to_string(),
                error: None,
            });
        }
        let tokens = query_tokens(&query);
        let type_filter = normalize_optional_filter(args["memory_type"].as_str());
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        let include_related = args["include_related"].as_bool().unwrap_or(false);

        let user_id = user_id_for(session, &self.resolver);
        let files = scan_merged(&self.workspace_dir, &user_id);

        let mut results: Vec<(i32, &crate::memory::MemoryFile)> = Vec::new();
        for mf in &files {
            if let Some(ref wanted) = type_filter {
                if mf.mem_type.to_lowercase() != *wanted {
                    continue;
                }
            }
            let score = memory_search_score(mf, &query, &tokens);
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

        results.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        results.truncate(limit);

        let backlinks = build_backlinks(&files);
        let json_results: Vec<serde_json::Value> = results
            .iter()
            .map(|(score, mf)| {
                let snippet = best_snippet(mf, &query, &tokens);

                let outgoing = link_values(&mf.links);
                let file_backlinks = backlinks.get(&mf.name).cloned().unwrap_or_default();
                let mut related = Vec::new();
                if include_related {
                    for link in &mf.links {
                        related.push(
                            json!({ "direction": "out", "name": link.target, "label": link.label }),
                        );
                    }
                    for backlink in &file_backlinks {
                        related.push(json!({ "direction": "in", "name": backlink }));
                    }
                }

                let mut result = json!({
                    "name": &mf.name,
                    "type": &mf.mem_type,
                    "description": &mf.description,
                    "snippet": snippet,
                    "relevance": score,
                    "links": outgoing,
                    "backlinks": file_backlinks,
                });
                if include_related {
                    result["related"] = json!(related);
                }
                if !mf.tags.is_empty() {
                    result["tags"] = json!(mf.tags);
                }
                result
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: json!({
                "success": true,
                "count": json_results.len(),
                "results": json_results,
            })
            .to_string(),
            error: None,
        })
    }
}

// ══════════════════════════════════════════════════════════════════════════
// memory_manage
// ══════════════════════════════════════════════════════════════════════════

pub struct MemoryManageTool {
    workspace_dir: PathBuf,
    resolver: Arc<UserResolver>,
}

impl MemoryManageTool {
    pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self {
        Self {
            workspace_dir,
            resolver,
        }
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
         Use remove when: memory is stale or wrong.\n\nConfirm with user before removing memories.\n\n\
         Scopes: 'user' (default) stores the memory in the current user's private layer; 'agent' \
         stores it in the shared agent layer (cross-user methodology/processes/rules, must be \
         de-identified). Use 'agent' only for generalizable knowledge without user-specific \
         identifiers."
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
                "scope": {
                    "type": "string",
                    "enum": ["user", "agent"],
                    "description": "Target layer: 'user' (default) = this user's private memory; \
                     'agent' = shared cross-user layer (de-identified only)."
                },
                "name": {
                    "type": "string",
                    "description": "Memory entry name (used as identifier). Lowercase, hyphens allowed."
                },
                "content": {
                    "type": "string",
                    "description": "Memory content. Required for add and replace. BODY ONLY: plain markdown content, no YAML frontmatter and no `---` blocks. When applicable, end with `## See Also` and 1-3 links in canonical form: `[Related: other_memory_name](other_memory_name.md)` — href must be `<name>.md` (not bare name, not a path)."
                },
                "memory_type": {
                    "type": "string",
                    "description": "Semantic category (user, feedback, rule, project, reference, or custom). \
                     Does NOT control injection — use the `inject` field for that. Default: project."
                },
                "inject": {
                    "type": "string",
                    "enum": ["always", "search"],
                    "description": "Injection policy. `always`: description injected into every conversation's \
                     system-reminder (use for behavioral rules, personality traits, communication preferences \
                     that affect every interaction). `search`: available via memory_search only, never \
                     auto-injected (use for technical gotchas, situational facts, project context). \
                     Default: search."
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of the key content (1-2 sentences). \
                     Required for inject=always (injected into system prompt). \
                     Auto-generated from content if omitted. Formerly known as 'summary'."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tags for filtering and categorization, e.g. [\"rust\", \"qqbot\", \"bug\"]."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Required and must be true for action='remove'. Only remove after explicit user confirmation."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional short reason for audit logging."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model identifier for audit logging when a background agent writes memory."
                }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        let user_id = user_id_for(session, &self.resolver);

        let result = match action {
            "add" => self.action_add(name, &args, &user_id, session),
            "replace" => self.action_replace(name, &args, &user_id, session),
            "remove" => self.action_remove(name, &args, &user_id, session),
            _ => Err(format!(
                "Unknown action '{}'. Use: add, replace, remove",
                action
            )),
        };

        match result {
            Ok(v) => Ok(ToolResult {
                success: true,
                output: v.to_string(),
                error: None,
            }),
            Err(msg) => Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": msg}).to_string(),
                error: None,
            }),
        }
    }
}

impl MemoryManageTool {
    fn action_add(
        &self,
        name: &str,
        args: &serde_json::Value,
        user_id: &str,
        session: &Session,
    ) -> Result<serde_json::Value, String> {
        validate_name(name)?;
        let scope = resolve_scope(args);

        let content = args["content"]
            .as_str()
            .ok_or("'content' is required for add.")?;
        if content.trim().is_empty() {
            return Err("Content cannot be empty.".to_string());
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!(
                "Content exceeds {} character limit.",
                MAX_CONTENT_CHARS
            ));
        }

        validate_body_only(content)?;
        scan_memory_content_opt(content)?;
        if scope == "agent" {
            scan_agent_pii_opt(content)?;
        }

        let files = scan_scope(&self.workspace_dir, scope, user_id);
        if files.iter().any(|f| f.name == name) {
            return Err(format!(
                "Memory '{}' already exists in the {} scope. Use 'replace' to update it.",
                name, scope
            ));
        }

        let mem_type = self.resolve_type(args);
        let inject = self.resolve_inject(args);
        let description = self.resolve_description(args, content);
        let tags = self.resolve_tags(args);
        let filename = format!("{}.md", name);
        let now = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let warnings = lint_memory_content(name, content, &files);
        let frontmatter = build_frontmatter(name, &description, &tags, &mem_type, &inject, &now, None, Some(&session.id));
        let file_content = format!("{}{}", frontmatter, content);

        let target = scope_memory_dir(&self.workspace_dir, scope, user_id).join(&filename);
        // Ensure the memory dir exists
        let _ = std::fs::create_dir_all(target.parent().unwrap_or(&target));
        atomic_write(&target, &file_content)
            .map_err(|e| format!("Failed to write memory file: {}", e))?;
        append_memory_audit(
            &self.workspace_dir,
            session,
            MemoryAudit {
                user_id,
                scope,
                action: "add",
                name,
                old_hash: None,
                new_hash: Some(short_sha256(&file_content)),
                args,
            },
        );

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' added.", name),
            "name": name,
            "type": &mem_type,
            "description": &description,
            "tags": tags,
            "warnings": warnings,
        }))
    }

    fn action_replace(
        &self,
        name: &str,
        args: &serde_json::Value,
        user_id: &str,
        session: &Session,
    ) -> Result<serde_json::Value, String> {
        validate_name(name)?;
        let scope = resolve_scope(args);

        let files = scan_scope(&self.workspace_dir, scope, user_id);
        let existing = files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| format!("Memory '{}' not found in the {} scope.", name, scope))?;
        let old_hash = std::fs::read_to_string(&existing.path)
            .ok()
            .map(|content| short_sha256(&content));

        let content = args["content"]
            .as_str()
            .ok_or("'content' is required for replace.")?;
        if content.trim().is_empty() {
            return Err("Content cannot be empty.".to_string());
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!(
                "Content exceeds {} character limit.",
                MAX_CONTENT_CHARS
            ));
        }

        validate_body_only(content)?;
        scan_memory_content_opt(content)?;
        if scope == "agent" {
            scan_agent_pii_opt(content)?;
        }

        // Preserve existing metadata unless overridden
        let mem_type = normalize_optional_filter(args["memory_type"].as_str())
            .unwrap_or_else(|| existing.mem_type.clone());
        let inject = match args["inject"].as_str() {
            Some("always") => "always".to_string(),
            Some("search") => "search".to_string(),
            _ => existing.inject.clone(),
        };
        let description =
            if args["description"].as_str().is_some() || args["summary"].as_str().is_some() {
                self.resolve_description(args, content)
            } else {
                existing.description.clone()
            };
        let tags = if args["tags"].is_array() {
            self.resolve_tags(args)
        } else {
            existing.tags.clone()
        };
        let now = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let warnings = lint_memory_content(name, content, &files);
        let frontmatter = build_frontmatter(
            name,
            &description,
            &tags,
            &mem_type,
            &inject,
            &existing.created_at,
            Some(&now),
            Some(&session.id),
        );
        let file_content = format!("{}{}", frontmatter, content);

        // Write to the same location as the existing file.
        let target = existing.path.clone();
        atomic_write(&target, &file_content)
            .map_err(|e| format!("Failed to write memory file: {}", e))?;
        append_memory_audit(
            &self.workspace_dir,
            session,
            MemoryAudit {
                user_id,
                scope,
                action: "replace",
                name,
                old_hash,
                new_hash: Some(short_sha256(&file_content)),
                args,
            },
        );

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' updated.", name),
            "name": name,
            "warnings": warnings,
        }))
    }

    fn action_remove(
        &self,
        name: &str,
        args: &serde_json::Value,
        user_id: &str,
        session: &Session,
    ) -> Result<serde_json::Value, String> {
        validate_name(name)?;
        let scope = resolve_scope(args);
        if args["confirm"].as_bool() != Some(true) {
            return Err(
                "Removing memory requires confirm=true after explicit user confirmation."
                    .to_string(),
            );
        }

        let files = scan_scope(&self.workspace_dir, scope, user_id);
        let existing = files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| format!("Memory '{}' not found in the {} scope.", name, scope))?;
        let old_hash = std::fs::read_to_string(&existing.path)
            .ok()
            .map(|content| short_sha256(&content));

        std::fs::remove_file(&existing.path)
            .map_err(|e| format!("Failed to remove memory file: {}", e))?;
        append_memory_audit(
            &self.workspace_dir,
            session,
            MemoryAudit {
                user_id,
                scope,
                action: "remove",
                name,
                old_hash,
                new_hash: None,
                args,
            },
        );

        Ok(json!({
            "success": true,
            "message": format!("Memory '{}' removed.", name),
        }))
    }

    fn resolve_type(&self, args: &serde_json::Value) -> String {
        normalize_optional_filter(args["memory_type"].as_str())
            .unwrap_or_else(|| "project".to_string())
    }

    fn resolve_inject(&self, args: &serde_json::Value) -> String {
        match args["inject"].as_str() {
            Some("always") => "always".to_string(),
            _ => "search".to_string(),
        }
    }

    /// Resolve description: explicit parameter, or auto-generate from content.
    fn resolve_description(&self, args: &serde_json::Value, content: &str) -> String {
        if let Some(desc) = args["description"]
            .as_str()
            .or_else(|| args["summary"].as_str())
        {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                if trimmed.chars().count() > MAX_DESCRIPTION_CHARS {
                    let truncated: String = trimmed.chars().take(MAX_DESCRIPTION_CHARS).collect();
                    return truncated;
                }
                return trimmed.to_string();
            }
        }
        // Auto-generate from first non-empty line of content
        let first_line = content
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(content);
        let truncated: String = first_line.chars().take(200).collect();
        truncated
    }

    fn resolve_tags(&self, args: &serde_json::Value) -> Vec<String> {
        match args["tags"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => Vec::new(),
        }
    }
}
