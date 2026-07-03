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
//! - [User timezone preference](user_timezone.md)
//! ```
//!
//! No separate index file. The index is generated dynamically by scanning
//! `memory/*.md` frontmatter. Cross-session sync via file watcher.
//!
//! ## System-reminder injection policy
//!
//! Only `user`, `feedback`, and `rule` types are injected into
//! system-reminders — these are facts the agent must always obey.
//! All other types are available via memory_list / memory_search.

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
    pub mem_type: String,
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
    pub description: String,
    pub tags: Vec<String>,
    pub link_count: usize,
}

impl From<&MemoryFile> for IndexEntry {
    fn from(f: &MemoryFile) -> Self {
        Self {
            name: f.name.clone(),
            mem_type: f.mem_type.clone(),
            description: f.description.clone(),
            tags: f.tags.clone(),
            link_count: f.links.len(),
        }
    }
}

// ── Injection helpers ──────────────────────────────────────────────────────

/// Types injected into every conversation's system-reminder.
/// Open string matching — not limited to a fixed enum.
pub fn should_inject(mem_type: &str) -> bool {
    matches!(mem_type, "user" | "feedback" | "rule")
}

// ── Directory management ───────────────────────────────────────────────────

/// Ensure the knowledge directory exists.
/// Returns the knowledge directory path.
pub fn ensure_memory_dir(knowledge_dir: &str) -> std::io::Result<std::path::PathBuf> {
    let memory_dir = Path::new(knowledge_dir);
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
    let mut description: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut abstract_val: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut mem_type: Option<String> = None;
    let mut created_at = None;
    let mut updated_at: Option<String> = None;

    for line in frontmatter_text.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = strip_yaml_quotes(value.trim());
            match key {
                "name" => name = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                "summary" => summary = Some(value.to_string()),
                "abstract" => abstract_val = Some(value.to_string()),
                "tags" => tags = parse_tags(value),
                "type" => mem_type = Some(value.to_string()),
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
        mem_type: mem_type?,
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
/// Returns `Vec<LinkRef>` with target names (no path, no `.md`).
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

        // Parse markdown link: [label](target)
        if let Some(link) = parse_md_link(trimmed) {
            links.push(link);
        }
    }

    links
}

/// Parse a single markdown link `[label](target)` from a line.
fn parse_md_link(line: &str) -> Option<LinkRef> {
    let bracket_start = line.find('[')?;
    let rest_after_bracket = &line[bracket_start + 1..];
    let bracket_end_rel = rest_after_bracket.find(']')?;
    let label = &rest_after_bracket[..bracket_end_rel];

    let rest = &rest_after_bracket[bracket_end_rel + 1..];
    let paren_start = rest.find('(')?;
    let rest_after_paren = &rest[paren_start + 1..];
    let paren_end_rel = rest_after_paren.find(')')?;
    let target_raw = &rest_after_paren[..paren_end_rel];

    // Strip path prefix (take last component) and .md suffix
    let target = target_raw
        .rsplit('/')
        .next()
        .unwrap_or(target_raw)
        .trim_end_matches(".md")
        .to_string();

    if target.is_empty() {
        return None;
    }

    Some(LinkRef {
        target,
        label: label.to_string(),
    })
}

/// Build a reverse-link (backlink) index: for each entity name, which
/// other entities link to it.
pub fn build_backlinks(files: &[MemoryFile]) -> HashMap<String, Vec<String>> {
    let mut backlinks: HashMap<String, Vec<String>> = HashMap::new();
    for f in files {
        for link in &f.links {
            backlinks
                .entry(link.target.clone())
                .or_default()
                .push(f.name.clone());
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
/// Only includes types matching `should_inject` (user, feedback, rule).
/// Groups entries by type, types sorted alphabetically.
pub fn format_wiki_index(entries: &[IndexEntry]) -> String {
    // Filter to injected types only
    let injectable: Vec<&IndexEntry> = entries
        .iter()
        .filter(|e| should_inject(&e.mem_type))
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
        if let Some(pos) = truncated[..max_bytes].rfind('\n') {
            truncated.truncate(pos);
        } else {
            truncated.truncate(max_bytes);
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
        let content = "---\nname: user_lang\ndescription: 用户偏好使用中文进行所有交流\ntags: [user, language, preference]\ntype: user\ncreated_at: 2026-05-07\n---\n\n中文交流。";
        let dir = std::env::temp_dir().join("myclaw_test_memory_parse_desc");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user_lang.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.name, "user_lang");
        assert_eq!(mf.description, "用户偏好使用中文进行所有交流");
        assert_eq!(mf.tags, vec!["user", "language", "preference"]);
        assert_eq!(mf.mem_type, "user");
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
        let content = "Some content.\n\n## See Also\n- [Used by foo](foo.md)\n- [Related to bar](bar)\n- [Nested path](subdir/baz.md)\n\n## Other\n- Not a link section";
        let links = extract_links(content);
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].label, "Used by foo");
        assert_eq!(links[0].target, "foo");
        assert_eq!(links[1].label, "Related to bar");
        assert_eq!(links[1].target, "bar");
        assert_eq!(links[2].label, "Nested path");
        assert_eq!(links[2].target, "baz");
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
            },
            MemoryFile {
                name: "beta".into(),
                mem_type: "entity".into(),
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
                description: "Project context".into(),
                tags: vec![],
                link_count: 0,
            },
            IndexEntry {
                name: "no_diff".into(),
                mem_type: "feedback".into(),
                description: "不要总结 diff".into(),
                tags: vec!["workflow".into()],
                link_count: 1,
            },
            IndexEntry {
                name: "lang".into(),
                mem_type: "user".into(),
                description: "中文回复".into(),
                tags: vec![],
                link_count: 0,
            },
            IndexEntry {
                name: "rule1".into(),
                mem_type: "rule".into(),
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
}
