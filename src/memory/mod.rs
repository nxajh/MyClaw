//! Memory wiki — file-based knowledge graph with frontmatter index.
//!
//! Memory files live in `workspace/memory/`, each with YAML frontmatter:
//!
//! ```markdown
//! ---
//! name: user_language
//! description: 用户偏好使用中文进行所有交流
//! tags: [user, language]
//! type: user
//! created_at: 2026-05-07
//! updated_at: 2026-07-01
//! ---
//!
//! Memory content...
//!
//! ## See Also
//! - [Related: user_timezone](user_timezone.md)
//!
//! Canonical See Also href is always `<logical_name>.md` (logical name has no suffix).
//! ```
//!
//! No separate index file. The index is generated dynamically by scanning
//! `memory/*.md` frontmatter. Cross-session sync via file watcher.
//!
//! ## Injection policy
//!
//! Each memory file has an `inject` field in frontmatter (`always` or `search`).
//! `always` entries are injected into every conversation's system-reminder
//! (diff-based). `search` entries are available via memory_list / memory_search
//! but never auto-injected. The `type` field is purely semantic categorization
//! and does not control injection.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

// ── Constants ──────────────────────────────────────────────────────────────

pub const MAX_INDEX_LINES: usize = 200;
pub const MAX_INDEX_BYTES: usize = 25_000;
pub const MEMORY_DIR_NAME: &str = "memory";

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub name: String,
    pub scope: Option<String>,
    pub user_id: Option<String>,
    pub mem_type: String,
    pub inject: String,
    pub description: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub links: Vec<LinkRef>,
    pub content: String,
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub struct LinkRef {
    /// Target entity name (no path prefix, no `.md` suffix).
    pub target: String,
    /// Link label / relationship description.
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub name: String,
    pub mem_type: String,
    pub inject: String,
    pub description: String,
    pub tags: Vec<String>,
    pub link_count: usize,
}

impl From<&MemoryFile> for IndexEntry {
    fn from(f: &MemoryFile) -> Self {
        Self {
            name: f.name.clone(),
            mem_type: f.mem_type.clone(),
            inject: f.inject.clone(),
            description: f.description.clone(),
            tags: f.tags.clone(),
            link_count: f.links.len(),
        }
    }
}

// ── Layered storage (P4 / RFC #101 §6) ────────────────────────────────────
//
// Layout:
//   {base_dir}/memory/               agent layer (the memory root itself)
//   {base_dir}/users/{uuid}/memory/  user layer (dir name = bare uuid)
//
// Directories are authoritative for layering; frontmatter `scope`/`user_id`
// are kept as redundant verification. Transition compatibility: pre-migration
// single-pool data still sits in the agent dir with `scope: user` frontmatter
// — for those entries frontmatter wins over the directory, so they keep
// reading/writing as user-layer entries until the stage-3 migration moves
// them physically.

/// Users root derived from a memory root (`{base}/memory` → `{base}/users`).
fn users_root_of(memory_root: &Path) -> std::path::PathBuf {
    match memory_root.parent() {
        Some(base) => base.join("users"),
        None => std::path::PathBuf::from("users"),
    }
}

/// Directory of one user's private memory layer. Pure computation — creating
/// the directory is the caller's job.
pub fn user_memory_dir(memory_root: &Path, user_id: &str) -> std::path::PathBuf {
    users_root_of(memory_root)
        .join(crate::ids::bare_dir_name(user_id))
        .join(MEMORY_DIR_NAME)
}

/// Normalize `scope`/`user_id` so the frontmatter fields alone determine the
/// layer (the scan helpers guarantee their correctness, so downstream code
/// never has to re-derive provenance from paths):
/// - agent-dir file with `scope: user` → stays user-layer (transition
///   fallback; `user_id` must come from frontmatter);
/// - agent-dir file otherwise → agent layer;
/// - user-dir file → user layer, `user_id` falls back to the dir owner.
fn normalize_ownership(f: &mut MemoryFile, from_agent_dir: bool, dir_owner: Option<&str>) {
    if !from_agent_dir {
        f.scope = Some("user".to_string());
        if f.user_id.as_deref().map_or(true, |u| u.is_empty()) {
            f.user_id = Some(dir_owner.unwrap_or("unknown").to_string());
        }
    } else if f.scope.as_deref() != Some("user") {
        f.scope = Some("agent".to_string());
    }
}

/// Scan the agent layer only (the memory root dir). Pre-migration user
/// entries still living there are excluded — they belong to the user layer.
pub fn scan_agent_layer(memory_root: &Path) -> Vec<MemoryFile> {
    let mut files = scan_memory_files(memory_root);
    files.retain_mut(|f| {
        normalize_ownership(f, true, None);
        f.scope.as_deref() != Some("user")
    });
    files
}

/// Scan one user's user layer: their `users/{uuid}/memory` dir plus
/// pre-migration fallback entries still in the agent dir (frontmatter
/// `scope: user` + exact `user_id` match).
pub fn scan_user_layer(memory_root: &Path, user_id: &str) -> Vec<MemoryFile> {
    let mut files = scan_memory_files(&user_memory_dir(memory_root, user_id));
    for f in &mut files {
        normalize_ownership(f, false, Some(user_id));
    }
    // Transition fallback: single-pool entries not yet migrated.
    for mut f in scan_memory_files(memory_root) {
        if f.scope.as_deref() == Some("user") && f.user_id.as_deref() == Some(user_id) {
            normalize_ownership(&mut f, true, None);
            files.push(f);
        }
    }
    files
}

/// Merged view visible to one user: the whole agent layer plus their own
/// user layer. Same-name shadowing: the user layer wins (RFC §6.2).
pub fn scan_merged_for_user(memory_root: &Path, user_id: &str) -> Vec<MemoryFile> {
    let user = scan_user_layer(memory_root, user_id);
    let mut agent = scan_agent_layer(memory_root);
    // User layer shadows same-named agent entries.
    let mut seen: std::collections::HashSet<String> =
        user.iter().map(|f| f.name.clone()).collect();
    agent.retain(|f| seen.insert(f.name.clone()));
    let mut files: Vec<MemoryFile> = user.into_iter().chain(agent).collect();
    files.sort_by(|a, b| (&a.mem_type, &a.name).cmp(&(&b.mem_type, &b.name)));
    files
}

/// Every user-layer entry across all user dirs, plus agent-dir fallback
/// entries — for cross-user maintenance jobs (distill). `user_id` is
/// normalized on every entry so per-user grouping works.
pub fn scan_all_user_layers(memory_root: &Path) -> Vec<MemoryFile> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(users_root_of(memory_root)) {
        for entry in rd.flatten() {
            let owner = entry.file_name().to_string_lossy().to_string();
            let mem_dir = entry.path().join(MEMORY_DIR_NAME);
            for mut f in scan_memory_files(&mem_dir) {
                normalize_ownership(&mut f, false, Some(&owner));
                out.push(f);
            }
        }
    }
    // Transition fallback: single-pool entries not yet migrated.
    for mut f in scan_memory_files(memory_root) {
        if f.scope.as_deref() == Some("user") {
            normalize_ownership(&mut f, true, None);
            out.push(f);
        }
    }
    out
}

// ── Injection helpers ──────────────────────────────────────────────────────

/// Whether a memory entry should be auto-injected into system-reminders.
/// Based on the per-entry `inject` field, not the semantic `type`.
pub fn should_inject(inject: &str) -> bool {
    inject == "always"
}

// ── Directory management ───────────────────────────────────────────────────

/// Ensure the memory root directory exists.
/// Returns the memory root path.
pub fn ensure_memory_dir(memory_root: &str) -> std::io::Result<std::path::PathBuf> {
    let memory_dir = Path::new(memory_root);
    fs::create_dir_all(memory_dir)?;
    Ok(memory_dir.to_path_buf())
}

// ── Scanning ───────────────────────────────────────────────────────────────

/// Scan `memory/*.md` files, parse frontmatter, return valid entries.
/// Files with missing or malformed frontmatter are silently skipped.
pub fn scan_memory_files(memory_dir: &Path) -> Vec<MemoryFile> {
    let entries = match fs::read_dir(memory_dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };

    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("md")) {
            continue;
        }
        if let Some(mf) = parse_memory_file(&path) {
            files.push(mf);
        }
    }

    // Stable sort: by type, then by name.
    files.sort_by(|a, b| (&a.mem_type, &a.name).cmp(&(&b.mem_type, &b.name)));
    files
}

/// Parse a single `.md` file's YAML frontmatter + content.
/// Returns `None` if frontmatter is missing or malformed.
fn parse_memory_file(path: &Path) -> Option<MemoryFile> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();

    // Frontmatter must start with "---\n"
    if !trimmed.starts_with("---") {
        return None;
    }

    // Find closing "---"
    let rest = &trimmed[3..];
    let rest = rest.trim_start_matches(['\r', '\n']);
    let end = rest.find("\n---")?;

    let frontmatter_text = &rest[..end];
    let content = rest[end + 4..].trim().to_string();

    // Parse YAML frontmatter (simple key: value parsing)
    let mut name = None;
    let mut scope: Option<String> = None;
    let mut user_id: Option<String> = None;
    let mut description: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut abstract_val: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut mem_type: Option<String> = None;
    let mut inject: Option<String> = None;
    let mut created_at = None;
    let mut updated_at: Option<String> = None;

    for line in frontmatter_text.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = strip_yaml_quotes(value.trim());
            match key {
                "name" => name = Some(value.to_string()),
                "scope" => scope = Some(value.to_string()),
                "user_id" => user_id = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                "summary" => summary = Some(value.to_string()),
                "abstract" => abstract_val = Some(value.to_string()),
                "tags" => tags = parse_tags(value),
                "type" => mem_type = Some(value.to_string()),
                "inject" => inject = Some(value.to_string()),
                "created_at" => created_at = Some(value.to_string()),
                "updated_at" => updated_at = Some(value.to_string()),
                _ => {}
            }
        }
    }

    // Unify description: priority description > summary > abstract
    let description = description.or(summary).or(abstract_val).unwrap_or_default();

    // Parse See Also links from body
    let links = extract_links(&content);

    Some(MemoryFile {
        name: name?,
        scope,
        user_id,
        mem_type: mem_type?,
        inject: inject.unwrap_or_else(|| "search".to_string()),
        description,
        tags,
        created_at: created_at.unwrap_or_default(),
        updated_at,
        links,
        content,
        path: path.to_path_buf(),
    })
}

/// Strip surrounding YAML double quotes from a value: `"foo"` → `foo`.
/// Leaves unquoted values unchanged.
fn strip_yaml_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse a tags value. Supports:
/// - YAML array: `[foo, bar, baz]` (values may be quoted)
/// - Comma-separated: `foo, bar, baz`
fn parse_tags(value: &str) -> Vec<String> {
    let value = value.trim();
    // Strip [ ] if present
    let inner = if value.starts_with('[') && value.ends_with(']') {
        &value[1..value.len() - 1]
    } else {
        value
    };
    inner
        .split(',')
        .map(|t| strip_yaml_quotes(t.trim()).to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Extract markdown links from the `## See Also` section.
/// Returns `Vec<LinkRef>` with logical target names (no path, no `.md`).
/// Only canonical hrefs are accepted: last path segment must end with `.md`.
pub fn extract_links_from_content(content: &str) -> Vec<LinkRef> {
    extract_links(content)
}

fn extract_links(content: &str) -> Vec<LinkRef> {
    let mut in_see_also = false;
    let mut links = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect any `## ` heading — toggle whether we're in See Also
        if trimmed.starts_with("## ") {
            in_see_also = trimmed.eq_ignore_ascii_case("## see also");
            continue;
        }

        if !in_see_also {
            continue;
        }

        // Parse markdown link: [label](name.md)
        if let Some(link) = parse_md_link(trimmed) {
            links.push(link);
        }
    }

    links
}

/// Parse a single markdown link `[label](target)` from a line.
///
/// Canonical form requires a `.md` suffix on the href (after any path segment).
/// Bare names like `(other_memory_name)` are rejected and not indexed.
/// Layer-qualified hrefs `agent:<name>.md` / `user:<name>.md` are accepted —
/// logical `LinkRef.target` keeps the prefix (`agent:<name>`).
/// Logical same-layer targets are stored without the `.md` suffix.
fn parse_md_link(line: &str) -> Option<LinkRef> {
    let bracket_start = line.find('[')?;
    let rest_after_bracket = &line[bracket_start + 1..];
    let bracket_end_rel = rest_after_bracket.find(']')?;
    let label = &rest_after_bracket[..bracket_end_rel];

    let rest = &rest_after_bracket[bracket_end_rel + 1..];
    let paren_start = rest.find('(')?;
    let rest_after_paren = &rest[paren_start + 1..];
    let paren_end_rel = rest_after_paren.find(')')?;
    let target_raw = rest_after_paren[..paren_end_rel].trim();

    if target_raw.is_empty() {
        return None;
    }
    // External / non-memory hrefs are not graph edges
    if target_raw.starts_with("http://")
        || target_raw.starts_with("https://")
        || target_raw.starts_with('#')
        || target_raw.starts_with("mailto:")
    {
        return None;
    }

    // Last path segment only (allow accidental relative paths, still require .md)
    let segment = target_raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(target_raw)
        .trim();
    if segment.is_empty() || segment == ".." || segment.contains("..") {
        return None;
    }

    // Require .md suffix — bare logical names are not accepted
    let stem = segment
        .strip_suffix(".md")
        .or_else(|| segment.strip_suffix(".MD"))
        .or_else(|| segment.strip_suffix(".Md"))
        .or_else(|| segment.strip_suffix(".mD"))?;
    if stem.is_empty() {
        return None;
    }

    Some(LinkRef {
        target: stem.to_string(),
        label: label.to_string(),
    })
}

/// Layer-qualified link target prefix (`agent:` / `user:`), RFC #101 §6.3.
/// `(layer, bare_name)` — e.g. `agent:x` → `("agent", "x")`. `@<uuid>` is a
/// reserved form and not implemented.
pub fn parse_layer_prefix(target: &str) -> Option<(&'static str, &str)> {
    if let Some(rest) = target.strip_prefix("agent:") {
        if !rest.is_empty() {
            return Some(("agent", rest));
        }
    }
    if let Some(rest) = target.strip_prefix("user:") {
        if !rest.is_empty() {
            return Some(("user", rest));
        }
    }
    None
}

/// Bare entity name of a link target: strips a layer prefix if present.
pub fn strip_layer_prefix(target: &str) -> &str {
    match parse_layer_prefix(target) {
        Some((_, name)) => name,
        None => target,
    }
}

/// Effective layer of a scanned memory file. The scan helpers normalize
/// `scope` on every entry, so the frontmatter field is authoritative here.
fn file_layer(f: &MemoryFile) -> &'static str {
    if f.scope.as_deref() == Some("user") {
        "user"
    } else {
        "agent"
    }
}

/// Build a reverse-link (backlink) index: for each entity name, which
/// other entities link to it. Layer rules (RFC #101 §6.3):
/// - `agent:x` / `user:x` targets match the entry `x` in that layer;
/// - a bare target only matches a same-layer entry — pointing at another
///   layer's same-named entry does NOT count (cross-layer must be explicit).
pub fn build_backlinks(files: &[MemoryFile]) -> HashMap<String, Vec<String>> {
    let mut backlinks: HashMap<String, Vec<String>> = HashMap::new();
    for f in files {
        let src_layer = file_layer(f);
        for link in &f.links {
            let (target_layer, target_name) = match parse_layer_prefix(&link.target) {
                Some((layer, name)) => (layer, name),
                // Bare name: same layer only, never crosses layers.
                None => (src_layer, link.target.as_str()),
            };
            let hit = files
                .iter()
                .any(|g| g.name == target_name && file_layer(g) == target_layer);
            if hit {
                backlinks
                    .entry(target_name.to_string())
                    .or_default()
                    .push(f.name.clone());
            }
        }
    }
    // Deduplicate and sort each entry
    for refs in backlinks.values_mut() {
        refs.sort();
        refs.dedup();
    }
    backlinks
}

// ── Index formatting (for system-reminder injection) ──────────────────────

/// Generate a formatted index string for system-reminder injection.
/// Only includes entries with `inject: always`.
/// Groups entries by type, types sorted alphabetically.
pub fn format_wiki_index(entries: &[IndexEntry]) -> String {
    // Filter to injected entries only
    let injectable: Vec<&IndexEntry> = entries
        .iter()
        .filter(|e| should_inject(&e.inject))
        .collect();

    if injectable.is_empty() {
        return "暂无需要遵守的记忆。".to_string();
    }

    // Collect distinct type strings, sorted
    let mut types: Vec<&str> = injectable.iter().map(|e| e.mem_type.as_str()).collect();
    types.sort();
    types.dedup();

    let mut lines = Vec::new();

    for mem_type in &types {
        let group: Vec<&&IndexEntry> = injectable
            .iter()
            .filter(|e| e.mem_type == *mem_type)
            .collect();
        if group.is_empty() {
            continue;
        }

        lines.push(format!("### {}", mem_type));
        for entry in &group {
            let mut line = format!("- **{}**", entry.name);
            if !entry.tags.is_empty() {
                line.push_str(&format!(" [{}]", entry.tags.join(", ")));
            }
            lines.push(line);
            if !entry.description.is_empty() {
                lines.push(format!("  {}", entry.description));
            }
        }
        lines.push(String::new());
    }

    let text = lines.join("\n");
    truncate_index(&text, MAX_INDEX_LINES, MAX_INDEX_BYTES)
}

/// Generate a formatted full index string for tools/forks.
/// Includes all memory types (not only injected types).
pub fn format_full_memory_index(entries: &[IndexEntry]) -> String {
    if entries.is_empty() {
        return "(empty — no memories yet)".to_string();
    }

    let mut types: Vec<&str> = entries.iter().map(|e| e.mem_type.as_str()).collect();
    types.sort();
    types.dedup();

    let mut lines = Vec::new();
    for mem_type in &types {
        let group: Vec<&IndexEntry> = entries.iter().filter(|e| e.mem_type == *mem_type).collect();
        if group.is_empty() {
            continue;
        }
        lines.push(format!("### {}", mem_type));
        for entry in group {
            let mut line = format!("- **{}**", entry.name);
            if !entry.tags.is_empty() {
                line.push_str(&format!(" [{}]", entry.tags.join(", ")));
            }
            if entry.link_count > 0 {
                line.push_str(&format!(" ({} links)", entry.link_count));
            }
            lines.push(line);
            if !entry.description.is_empty() {
                lines.push(format!("  {}", entry.description));
            }
        }
        lines.push(String::new());
    }

    let text = lines.join("\n");
    truncate_index(&text, MAX_INDEX_LINES, MAX_INDEX_BYTES)
}

/// Truncate index text to line and byte limits.
pub fn truncate_index(content: &str, max_lines: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let was_line_truncated = lines.len() > max_lines;
    let was_byte_truncated = content.len() > max_bytes;

    if !was_line_truncated && !was_byte_truncated {
        return content.to_string();
    }

    let mut truncated = if was_line_truncated {
        lines[..max_lines].join("\n")
    } else {
        content.to_string()
    };

    if truncated.len() > max_bytes {
        // Find the last char boundary at or before max_bytes to avoid panicking
        // on multi-byte UTF-8 sequences.
        let mut end = max_bytes;
        while end > 0 && !truncated.is_char_boundary(end) {
            end -= 1;
        }
        if let Some(pos) = truncated[..end].rfind('\n') {
            truncated.truncate(pos);
        } else {
            truncated.truncate(end);
        }
    }

    truncated.push_str(&format!(
        "\n\n> WARNING: Memory index truncated ({} lines / {} bytes limit). \
         Keep entries concise; move detail into individual files.",
        max_lines, max_bytes,
    ));

    truncated
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_with_description() {
        let content = "---\nname: user_lang\ndescription: 用户偏好使用中文进行所有交流\ninject: always\ntags: [user, language, preference]\ntype: user\ncreated_at: 2026-05-07\n---\n\n中文交流。";
        let dir = std::env::temp_dir().join("myclaw_test_memory_parse_desc");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user_lang.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.name, "user_lang");
        assert_eq!(mf.description, "用户偏好使用中文进行所有交流");
        assert_eq!(mf.tags, vec!["user", "language", "preference"]);
        assert_eq!(mf.mem_type, "user");
        assert_eq!(mf.inject, "always");
        assert_eq!(mf.created_at, "2026-05-07");
        assert!(mf.updated_at.is_none());
        assert_eq!(mf.content, "中文交流。");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_frontmatter_summary_fallback() {
        // Old-format files with "summary:" still parse — stored as description.
        let content = "---\nname: test\nsummary: A short summary\ntype: entity\ncreated_at: 2026-05-07\n---\n\nContent.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_summary_fb");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.name, "test");
        assert_eq!(mf.description, "A short summary");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_frontmatter_abstract_fallback() {
        let content = "---\nname: test2\nabstract: An abstract\ntype: entity\ncreated_at: 2026-05-07\n---\n\nContent.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_abstract_fb");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test2.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.description, "An abstract");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_description_overrides_summary() {
        let content = "---\nname: test3\ndescription: The real desc\nsummary: Ignored\ntype: entity\ncreated_at: 2026-05-07\n---\n\nContent.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_desc_over");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test3.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.description, "The real desc");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_updated_at() {
        let content = "---\nname: test4\ndescription: desc\ntype: entity\ncreated_at: 2026-05-07\nupdated_at: 2026-07-01\n---\n\nContent.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_updated");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test4.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.updated_at.as_deref(), Some("2026-07-01"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_scope_ownership() {
        // scope: user + user_id → carried through
        let content = "---\nname: scoped\nscope: user\nuser_id: myclaw/u/abc\ntype: project\ncreated_at: 2026-08-18\n---\n\nBody.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_scope");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scoped.md");
        fs::write(&path, content).unwrap();
        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.scope.as_deref(), Some("user"));
        assert_eq!(mf.user_id.as_deref(), Some("myclaw/u/abc"));
        let _ = fs::remove_dir_all(&dir);

        // No scope field → None (agent layer by default)
        let content = "---\nname: unscoped\ntype: project\ncreated_at: 2026-08-18\n---\n\nBody.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_noscope");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unscoped.md");
        fs::write(&path, content).unwrap();
        let mf = parse_memory_file(&path).unwrap();
        assert!(mf.scope.is_none());
        assert!(mf.user_id.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_frontmatter_no_description() {
        let content = "---\nname: test\ntype: entity\ncreated_at: 2026-05-07\n---\n\nContent.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_no_desc");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.name, "test");
        assert_eq!(mf.description, "");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_no_frontmatter() {
        let dir = std::env::temp_dir().join("myclaw_test_memory_no_fm");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plain.md");
        fs::write(&path, "Just some text without frontmatter").unwrap();

        assert!(parse_memory_file(&path).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_links() {
        let content = "Some content.\n\n## See Also\n- [Used by foo](foo.md)\n- [Related to bar](bar)\n- [Nested path](subdir/baz.md)\n- [External](https://example.com/x.md)\n- [Anchor](#section)\n\n## Other\n- Not a link section";
        let links = extract_links(content);
        // Bare `(bar)` is rejected; external/anchor skipped. Only canonical .md hrefs count.
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].label, "Used by foo");
        assert_eq!(links[0].target, "foo");
        assert_eq!(links[1].label, "Nested path");
        assert_eq!(links[1].target, "baz");
    }

    #[test]
    fn test_parse_md_link_requires_md_suffix() {
        assert!(parse_md_link("- [Related: foo](foo.md)").is_some());
        assert!(parse_md_link("- [Related: foo](path/to/foo.md)").is_some());
        assert_eq!(
            parse_md_link("- [Related: foo](path/to/foo.md)")
                .unwrap()
                .target,
            "foo"
        );
        // bare logical name — no longer accepted
        assert!(parse_md_link("- [Related: foo](foo)").is_none());
        assert!(parse_md_link("- [Related: foo](subdir/foo)").is_none());
        assert!(parse_md_link("- [empty](.md)").is_none());
        assert!(parse_md_link("- [ext](https://example.com/a.md)").is_none());
    }

    #[test]
    fn test_extract_links_no_section() {
        let content = "No See Also section here.";
        let links = extract_links(content);
        assert!(links.is_empty());
    }

    #[test]
    fn test_build_backlinks() {
        let files = vec![
            MemoryFile {
                name: "alpha".into(),
                mem_type: "entity".into(),
                inject: "search".into(),
                description: String::new(),
                tags: vec![],
                created_at: String::new(),
                updated_at: None,
                links: vec![
                    LinkRef {
                        target: "beta".into(),
                        label: "uses".into(),
                    },
                    LinkRef {
                        target: "gamma".into(),
                        label: "fixes".into(),
                    },
                ],
                content: String::new(),
                path: std::path::PathBuf::new(),
                scope: None,
                user_id: None,
            },
            MemoryFile {
                name: "beta".into(),
                mem_type: "entity".into(),
                inject: "search".into(),
                description: String::new(),
                tags: vec![],
                created_at: String::new(),
                updated_at: None,
                links: vec![LinkRef {
                    target: "gamma".into(),
                    label: "depends on".into(),
                }],
                content: String::new(),
                path: std::path::PathBuf::new(),
                scope: None,
                user_id: None,
            },
        ];

        let backlinks = build_backlinks(&files);
        assert_eq!(backlinks.get("beta"), Some(&vec!["alpha".to_string()]));
        assert_eq!(
            backlinks.get("gamma"),
            Some(&vec!["alpha".to_string(), "beta".to_string()])
        );
        assert!(backlinks.get("alpha").is_none());
    }

    #[test]
    fn test_format_wiki_index_injected_only() {
        let entries = vec![
            IndexEntry {
                name: "project1".into(),
                mem_type: "entity".into(),
                inject: "search".into(),
                description: "Project context".into(),
                tags: vec![],
                link_count: 0,
            },
            IndexEntry {
                name: "no_diff".into(),
                mem_type: "feedback".into(),
                inject: "always".into(),
                description: "不要总结 diff".into(),
                tags: vec!["workflow".into()],
                link_count: 1,
            },
            IndexEntry {
                name: "lang".into(),
                mem_type: "user".into(),
                inject: "always".into(),
                description: "中文回复".into(),
                tags: vec![],
                link_count: 0,
            },
            IndexEntry {
                name: "rule1".into(),
                mem_type: "rule".into(),
                inject: "always".into(),
                description: "Always test".into(),
                tags: vec![],
                link_count: 0,
            },
        ];
        let index = format_wiki_index(&entries);
        assert!(index.contains("### user"));
        assert!(index.contains("### feedback"));
        assert!(index.contains("### rule"));
        assert!(!index.contains("### entity")); // entity should NOT be injected
        assert!(index.contains("lang"));
        assert!(index.contains("no_diff"));
        assert!(index.contains("rule1"));
        assert!(!index.contains("project1"));
    }

    #[test]
    fn test_format_wiki_index_empty_injectable() {
        let entries = vec![IndexEntry {
            name: "project1".into(),
            mem_type: "entity".into(),
            inject: "search".into(),
            description: "Project context".into(),
            tags: vec![],
            link_count: 0,
        }];
        let index = format_wiki_index(&entries);
        assert!(index.contains("暂无需要遵守的记忆"));
    }

    #[test]
    fn test_truncate_index() {
        let long: String = (0..300)
            .map(|i| format!("- file{}.md — summary {}", i, i))
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = truncate_index(&long, 200, 25_000);
        assert!(truncated.contains("WARNING"));
        assert!(truncated.lines().count() <= 202);
    }

    // ── P4 layered storage tests ───────────────────────────────────────────

    const ALICE: &str = "myclaw/u/018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e7f";
    const BOB: &str = "myclaw/u/018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e80";

    /// Build `{base}/memory` (agent layer) layout and return the memory root.
    fn layered_layout(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("myclaw_test_layered_{}", tag));
        let _ = fs::remove_dir_all(&base);
        let memory_root = base.join("memory");
        fs::create_dir_all(&memory_root).unwrap();
        memory_root
    }

    fn write_file(dir: &Path, name: &str, fm: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join(format!("{}.md", name)),
            format!("---\nname: {}\n{}\ntype: project\ncreated_at: 2026-08-30\n---\n\nbody {}", name, fm, name),
        )
        .unwrap();
    }

    #[test]
    fn user_memory_dir_is_bare_uuid_under_users() {
        let root = layered_layout("dir");
        let dir = user_memory_dir(&root, ALICE);
        assert_eq!(
            dir,
            root.parent().unwrap().join("users").join("018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e7f").join("memory")
        );
        // Legacy routing keys fall back to the escaped dir_name form.
        assert_eq!(
            user_memory_dir(&root, "telegram:123"),
            root.parent().unwrap().join("users").join("telegram_123").join("memory")
        );
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn scan_merged_shadows_agent_with_user_layer() {
        let root = layered_layout("shadow");
        write_file(&root, "dup", "scope: agent");
        let alice_dir = user_memory_dir(&root, ALICE);
        write_file(&alice_dir, "dup", &format!("scope: user\nuser_id: {}", ALICE));
        write_file(&root, "agent-only", "scope: agent");

        let merged = scan_merged_for_user(&root, ALICE);
        let dup = merged.iter().find(|f| f.name == "dup").unwrap();
        assert_eq!(dup.scope.as_deref(), Some("user"), "user layer must shadow agent");
        assert!(merged.iter().any(|f| f.name == "agent-only"));
        assert_eq!(merged.len(), 2);

        // Bob sees the agent entry, not alice's shadow.
        let bob_view = scan_merged_for_user(&root, BOB);
        let dup_bob = bob_view.iter().find(|f| f.name == "dup").unwrap();
        assert_eq!(dup_bob.scope.as_deref(), Some("agent"));
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn transition_fallback_agent_dir_user_entries_stay_user_layer() {
        let root = layered_layout("fallback");
        // Pre-migration single-pool entry: physically in the agent dir,
        // frontmatter says user.
        write_file(&root, "legacy-user", &format!("scope: user\nuser_id: {}", ALICE));
        write_file(&root, "legacy-agent", "scope: agent");

        let agent_layer = scan_agent_layer(&root);
        assert!(!agent_layer.iter().any(|f| f.name == "legacy-user"));
        assert!(agent_layer.iter().any(|f| f.name == "legacy-agent"));

        let alice_layer = scan_user_layer(&root, ALICE);
        assert!(alice_layer.iter().any(|f| f.name == "legacy-user"));
        // Not attributed to another user.
        assert!(scan_user_layer(&root, BOB).is_empty());

        let merged = scan_merged_for_user(&root, ALICE);
        assert!(merged.iter().any(|f| f.name == "legacy-user"));
        assert!(merged.iter().any(|f| f.name == "legacy-agent"));
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn scan_all_user_layers_spans_every_user_dir() {
        let root = layered_layout("allusers");
        write_file(&user_memory_dir(&root, ALICE), "alice-note", &format!("scope: user\nuser_id: {}", ALICE));
        write_file(&user_memory_dir(&root, BOB), "bob-note", &format!("scope: user\nuser_id: {}", BOB));
        write_file(&root, "agent-note", "scope: agent");
        // Legacy single-pool entry still in the agent dir.
        write_file(&root, "legacy-note", &format!("scope: user\nuser_id: {}", ALICE));

        let all = scan_all_user_layers(&root);
        let names: Vec<&str> = all.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"alice-note"));
        assert!(names.contains(&"bob-note"));
        assert!(names.contains(&"legacy-note"));
        assert!(!names.contains(&"agent-note"));
        // user_id normalized (FQID from frontmatter where present).
        let alice_note = all.iter().find(|f| f.name == "alice-note").unwrap();
        assert_eq!(alice_note.user_id.as_deref(), Some(ALICE));
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn user_layer_injection_isolation() {
        // Regression for the injection leak: alice's inject=always user-layer
        // entry must never enter bob's merged view.
        let root = layered_layout("inject");
        write_file(&user_memory_dir(&root, ALICE), "alice-always", &format!("scope: user\nuser_id: {}\ninject: always", ALICE));
        let bob_view = scan_merged_for_user(&root, BOB);
        assert!(bob_view.iter().all(|f| f.name != "alice-always"));
        let alice_view = scan_merged_for_user(&root, ALICE);
        assert!(alice_view.iter().any(|f| f.name == "alice-always"));
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn test_parse_layer_prefix() {
        assert_eq!(parse_layer_prefix("agent:x"), Some(("agent", "x")));
        assert_eq!(parse_layer_prefix("user:foo-bar"), Some(("user", "foo-bar")));
        assert_eq!(parse_layer_prefix("x"), None);
        assert_eq!(parse_layer_prefix("agent:"), None);
        assert_eq!(strip_layer_prefix("agent:x"), "x");
        assert_eq!(strip_layer_prefix("user:y"), "y");
        assert_eq!(strip_layer_prefix("z"), "z");
    }

    #[test]
    fn test_extract_links_layer_qualified() {
        let content = "Body.\n\n## See Also\n- [Same layer](foo.md)\n- [Agent entry](agent:bar.md)\n- [User entry](user:baz.md)";
        let links = extract_links(content);
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["foo", "agent:bar", "user:baz"]);
    }

    #[test]
    fn test_build_backlinks_layer_rules() {
        let mf = |name: &str, scope: Option<&str>, targets: &[&str]| MemoryFile {
            name: name.into(),
            scope: scope.map(|s| s.to_string()),
            user_id: None,
            mem_type: "entity".into(),
            inject: "search".into(),
            description: String::new(),
            tags: vec![],
            created_at: String::new(),
            updated_at: None,
            links: targets
                .iter()
                .map(|t| LinkRef {
                    target: t.to_string(),
                    label: "rel".into(),
                })
                .collect(),
            content: String::new(),
            path: std::path::PathBuf::new(),
        };
        // Both layers have an entry named "x".
        let files = vec![
            mf("a", Some("agent"), &["x", "user:x"]),
            mf("u", Some("user"), &["x", "agent:x"]),
        ];
        let backlinks = build_backlinks(&files);
        // Bare "x" from a → same layer (agent) → no agent-layer "x" exists →
        // no hit. user:x from a → explicit cross-layer → hits user x.
        // Bare "x" from u → same layer → hits user x. agent:x from u →
        // explicit → hits agent x? No agent x exists → no hit.
        assert_eq!(
            backlinks.get("x"),
            Some(&vec!["a".to_string(), "u".to_string()])
        );
        let _ = fs::remove_dir_all(std::env::temp_dir().join("myclaw_test_layered_dir"));
    }
}
