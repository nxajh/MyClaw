//! UTF-8-safe string truncation utilities and YAML front-matter parsing.
//!
//! Rust's `&str[..n]` and `String::truncate(n)` operate on **byte** offsets
//! and panic if `n` falls inside a multi-byte character. All functions here
//! accept **character** counts and safely convert to byte offsets.
//!
//! # Front-matter parsing
//!
//! Shared by `skill_loader` and `agent_loader` to parse YAML front matter
//! from Markdown files (`SKILL.md`, `AGENT.md`).

/// Return the byte offset of the `max_chars`-th character, or the full string
/// length if the string is shorter.
// char_offset("hello", 3) == 3
// char_offset("你好世界", 2) == 6   (each CJK char = 3 bytes)
// char_offset("abc", 10) == 3      (shorter than limit)
pub fn char_offset(s: &str, max_chars: usize) -> usize {
    s.char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Return a `&str` slice containing at most `max_chars` characters.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    &s[..char_offset(s, max_chars)]
}

/// Truncate to `max_chars` characters, appending `"..."` if truncated.
/// Collapses to the first line.
pub fn truncate_line(s: &str, max_chars: usize) -> String {
    let first_line = s.lines().next().unwrap_or("");
    if first_line.chars().count() <= max_chars {
        first_line.to_string()
    } else {
        format!("{}...", truncate_chars(first_line, max_chars - 3))
    }
}

// ── YAML Front-matter parsing (shared by skill_loader & agent_loader) ──────────

/// Separate YAML front matter from Markdown body.
///
/// Input must start with `---` on its own line. Returns `(front_matter, body)`.
/// If no valid front matter is found, returns `("", content)`.
pub fn parse_front_matter(content: &str) -> (String, String) {
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return (String::new(), trimmed.to_string());
    }

    // Find the second ---
    if let Some(end) = trimmed[3..].find("\n---") {
        let front_matter = trimmed[3..3 + end].trim().to_string();
        let body = trimmed[3 + end + 4..].trim().to_string(); // skip "\n---\n"
        return (front_matter, body);
    }

    (String::new(), trimmed.to_string())
}

/// Extract a string value from simple YAML text by key.
///
/// Handles quoted and unquoted values: `name: foo` and `name: "foo"`.
pub fn extract_yaml_string(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&format!("{}:", key)) {
            let value = rest.trim();
            // Strip surrounding quotes
            let value = if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                &value[1..value.len() - 1]
            } else {
                value
            };
            return Some(value.to_string());
        }
    }
    None
}

/// Extract a list value from simple YAML text by key.
///
/// Supports both inline (`[a, b, c]`) and multi-line (`- a\n- b`) formats.
pub fn extract_yaml_list(yaml: &str, key: &str) -> Vec<String> {
    let mut in_list = false;
    let mut items = Vec::new();

    for line in yaml.lines() {
        let trimmed = line.trim();

        if in_list {
            if let Some(item) = trimmed.strip_prefix("- ") {
                let item = item.trim();
                let item = if (item.starts_with('"') && item.ends_with('"'))
                    || (item.starts_with('\'') && item.ends_with('\''))
                {
                    &item[1..item.len() - 1]
                } else {
                    item
                };
                items.push(item.to_string());
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                // List ended
                break;
            }
        } else if let Some(rest) = line.trim().strip_prefix(&format!("{}:", key)) {
            let rest = rest.trim();
            if rest.starts_with('[') {
                // Inline list: keywords: [a, b, c]
                let inner = rest.trim_matches(|c| c == '[' || c == ']');
                items = inner
                    .split(',')
                    .map(|s| {
                        s.trim()
                            .trim_matches(|c| c == '"' || c == '\'')
                            .to_string()
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
                break;
            } else if rest.is_empty() {
                // Multi-line list
                in_list = true;
            }
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_front_matter() {
        let content = "---\nname: weather\ndescription: \"Get weather\"\n---\n\n# Body";
        let (fm, body) = parse_front_matter(content);
        assert!(fm.contains("name: weather"));
        assert!(body.contains("# Body"));
    }

    #[test]
    fn test_parse_front_matter_none() {
        let content = "# No front matter\n\nJust text.";
        let (fm, body) = parse_front_matter(content);
        assert!(fm.is_empty());
        assert!(body.contains("# No front matter"));
    }

    #[test]
    fn test_extract_yaml_string() {
        let yaml = "name: weather\ndescription: \"Get weather\"";
        assert_eq!(extract_yaml_string(yaml, "name"), Some("weather".to_string()));
        assert_eq!(
            extract_yaml_string(yaml, "description"),
            Some("Get weather".to_string())
        );
        assert_eq!(extract_yaml_string(yaml, "missing"), None);
    }

    #[test]
    fn test_extract_yaml_list_inline() {
        let yaml = "tools: [shell, file_read, \"file_write\"]";
        let items = extract_yaml_list(yaml, "tools");
        assert_eq!(items, vec!["shell", "file_read", "file_write"]);
    }

    #[test]
    fn test_extract_yaml_list_multiline() {
        let yaml = "tools:\n  - shell\n  - file_read\n  - file_write";
        let items = extract_yaml_list(yaml, "tools");
        assert_eq!(items, vec!["shell", "file_read", "file_write"]);
    }
}
