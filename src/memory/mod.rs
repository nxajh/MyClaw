//! Memory system — file-based persistent memory with frontmatter index.
//!
//! Memory files live in `workspace/memory/`, each with YAML frontmatter:
//!
//! ```markdown
//! ---
//! name: user_language
//! summary: 用户偏好使用中文进行所有交流
//! tags: [user, language]
//! type: user
//! created_at: 2026-05-07
//! ---
//!
//! Memory content...
//! ```
//!
//! No separate index file. The index is generated dynamically by scanning
//! `memory/*.md` frontmatter. Cross-session sync via file watcher.
//!
//! ## System-reminder injection policy
//!
//! Only `user` (preferences) and `feedback` (behavior corrections) types are
//! injected into system-reminders — these are facts the agent must always obey.
//! `project` and `reference` types are available via memory_list / memory_search.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

// ── Constants ──────────────────────────────────────────────────────────────

pub const MAX_INDEX_LINES: usize = 200;
pub const MAX_INDEX_BYTES: usize = 25_000;
pub const MEMORY_DIR_NAME: &str = "memory";

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryType {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }

    /// Types injected into system-reminders (agent must always obey).
    pub fn injected_types() -> &'static [MemoryType] {
        &[Self::User, Self::Feedback]
    }

    /// All types in canonical order (used for listing/grouping).
    pub fn all() -> &'static [MemoryType] {
        &[Self::User, Self::Feedback, Self::Project, Self::Reference]
    }
}

#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub name: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub mem_type: MemoryType,
    pub created_at: String,
    pub content: String,
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub mem_type: MemoryType,
    pub name: String,
    pub filename: String,
    pub summary: String,
    pub tags: Vec<String>,
}

impl From<&MemoryFile> for IndexEntry {
    fn from(f: &MemoryFile) -> Self {
        Self {
            mem_type: f.mem_type,
            name: f.name.clone(),
            filename: f
                .path
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or("")
                .to_string(),
            summary: f.summary.clone(),
            tags: f.tags.clone(),
        }
    }
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
///
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
    let mut summary: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut mem_type = None;
    let mut created_at = None;

    for line in frontmatter_text.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "name" => name = Some(value.to_string()),
                "summary" | "description" => summary = Some(value.to_string()),
                "tags" => tags = parse_tags(value),
                "type" => mem_type = MemoryType::from_str_lossy(value),
                "created_at" => created_at = Some(value.to_string()),
                _ => {}
            }
        }
    }

    Some(MemoryFile {
        name: name?,
        summary: summary.unwrap_or_default(),
        tags,
        mem_type: mem_type?,
        created_at: created_at.unwrap_or_default(),
        content,
        path: path.to_path_buf(),
    })
}

/// Parse a tags value. Supports:
/// - YAML array: `[foo, bar, baz]`
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
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

// ── Index formatting (for system-reminder injection) ──────────────────────

/// Generate a formatted index string for system-reminder injection.
/// Only includes `user` and `feedback` types.
pub fn format_memory_index(entries: &[IndexEntry]) -> String {
    // Filter to injected types only
    let injectable: Vec<&IndexEntry> = entries
        .iter()
        .filter(|e| MemoryType::injected_types().contains(&e.mem_type))
        .collect();

    if injectable.is_empty() {
        return "暂无需要遵守的记忆。".to_string();
    }

    let mut lines = Vec::new();

    for &mem_type in MemoryType::injected_types() {
        let group: Vec<&&IndexEntry> = injectable
            .iter()
            .filter(|e| e.mem_type == mem_type)
            .collect();
        if group.is_empty() {
            continue;
        }

        lines.push(format!("### {}", mem_type.as_str()));
        for entry in &group {
            let mut line = format!("- **{}**", entry.name);
            if !entry.tags.is_empty() {
                line.push_str(&format!(" [{}]", entry.tags.join(", ")));
            }
            lines.push(line);
            if !entry.summary.is_empty() {
                lines.push(format!("  {}", entry.summary));
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

// ── Memory section for system prompt ───────────────────────────────────────

/// Build the complete Memory section for the system prompt.
/// Includes static instructions + dynamic index from memory/*.md.
pub fn build_memory_section(knowledge_dir: &str) -> String {
    if knowledge_dir.is_empty() {
        return String::new();
    }

    let memory_dir = Path::new(knowledge_dir);
    let files = scan_memory_files(memory_dir);
    let entries: Vec<IndexEntry> = files.iter().map(IndexEntry::from).collect();
    let index_text = format_memory_index(&entries);

    format!(
        r#"## Memory

你有文件级持久记忆系统，文件存放在 `memory/` 目录。
记忆按 type 分类：user（用户偏好）、feedback（行为纠正）、project（项目背景）、reference（外部引用）。
当记忆内容与当前任务相关时，用 file_read 读取详细文件。

如果用户明确要求记住某事，或你发现偏好/行为模式变化，用 file_write 写入 memory/ 目录。
文件必须包含 YAML frontmatter（name / summary / type / created_at），可选 tags。
不要存可以从代码/文件推导的信息（代码路径、架构、git history）。

### 记忆索引

{}"#,
        index_text
    )
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_with_summary() {
        // New format uses "summary:", key is "summary:".
        let content = "---\nname: user_lang\nsummary: 用户偏好使用中文进行所有交流\ntags: [user, language, preference]\ntype: user\ncreated_at: 2026-05-07\n---\n\n中文交流。";
        let dir = std::env::temp_dir().join("myclaw_test_memory_parse");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user_lang.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.name, "user_lang");
        assert_eq!(mf.summary, "用户偏好使用中文进行所有交流");
        assert_eq!(mf.tags, vec!["user", "language", "preference"]);
        assert_eq!(mf.mem_type, MemoryType::User);
        assert_eq!(mf.created_at, "2026-05-07");
        assert_eq!(mf.content, "中文交流。");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_frontmatter_no_summary() {
        // Files without summary should parse fine (empty summary)
        let content = "---\nname: test\ntype: project\ncreated_at: 2026-05-07\n---\n\nContent.";
        let dir = std::env::temp_dir().join("myclaw_test_memory_no_summary");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        fs::write(&path, content).unwrap();

        let mf = parse_memory_file(&path).unwrap();
        assert_eq!(mf.name, "test");
        assert_eq!(mf.summary, "");

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
    fn test_format_index_only_user_and_feedback() {
        let entries = vec![
            IndexEntry {
                mem_type: MemoryType::Project,
                name: "project1".into(),
                filename: "project1.md".into(),
                summary: "Project context".into(),
                tags: vec![],
            },
            IndexEntry {
                mem_type: MemoryType::Feedback,
                name: "no_diff".into(),
                filename: "feedback_no_diff.md".into(),
                summary: "不要总结 diff".into(),
                tags: vec!["workflow".into()],
            },
            IndexEntry {
                mem_type: MemoryType::User,
                name: "lang".into(),
                filename: "user_lang.md".into(),
                summary: "中文回复".into(),
                tags: vec![],
            },
        ];
        let index = format_memory_index(&entries);
        assert!(index.contains("### user"));
        assert!(index.contains("### feedback"));
        assert!(!index.contains("### project")); // project should NOT be injected
        assert!(index.contains("lang"));
        assert!(index.contains("no_diff"));
        assert!(!index.contains("project1"));
    }

    #[test]
    fn test_format_index_empty_injectable() {
        // Only project/reference entries → empty index
        let entries = vec![IndexEntry {
            mem_type: MemoryType::Project,
            name: "project1".into(),
            filename: "project1.md".into(),
            summary: "Project context".into(),
            tags: vec![],
        }];
        let index = format_memory_index(&entries);
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
