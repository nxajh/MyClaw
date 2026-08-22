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

/// Neutralize Unicode spoofing characters by replacing them with visible
/// `[U+XXXX]` markers. Defends against "Trojan Source" attacks where
/// bidirectional override / isolate controls, zero-width characters, or
/// variation selectors visually reorder or disguise text.
///
/// **Replaced ranges:**
/// - `U+202A..=U+202E` — bidirectional overrides (LRE, RLE, PDF, LRO, RLO)
/// - `U+2066..=U+2069` — bidirectional isolates (LRI, RLI, FSI, PDI)
/// - `U+200B..=U+200D` — zero-width spaces (ZWSP, ZWNJ, ZWJ)
/// - `U+FEFF`           — BOM / zero-width no-break space
/// - `U+2060`           — word joiner
///
/// Normal text is unaffected. Returns the cleaned string plus the count of
/// characters replaced (useful for logging).
pub fn neutralize_spoofing(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if matches!(cp,
            0x202A..=0x202E   // bidi overrides
            | 0x2066..=0x2069 // bidi isolates
            | 0x200B..=0x200D // zero-width
            | 0xFEFF           // BOM
            | 0x2060           // word joiner
        ) {
            out.push_str(&format!("[U+{:04X}]", cp));
        } else {
            out.push(c);
        }
    }
    out
}

/// Return `true` if the string contains any spoofing character.
pub fn has_spoofing_chars(s: &str) -> bool {
    s.chars().any(|c| {
        let cp = c as u32;
        matches!(cp,
            0x202A..=0x202E
            | 0x2066..=0x2069
            | 0x200B..=0x200D
            | 0xFEFF
            | 0x2060
        )
    })
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
                    .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
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

/// Extract a boolean value from simple YAML text by key.
pub fn extract_yaml_bool(yaml: &str, key: &str) -> Option<bool> {
    extract_yaml_string(yaml, key).and_then(|v| match v.to_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    })
}

/// Extract a nested YAML mapping block by key (e.g. `metadata:` at the
/// top level, followed by indented `key: value` lines) and return it
/// dedented — ready to be re-parsed with `extract_yaml_string` /
/// `extract_yaml_list` / `extract_yaml_bool` as if it were its own
/// top-level document.
///
/// Only matches an unindented occurrence of `key:` with nothing after the
/// colon on the same line (a block opener, not an inline scalar/list).
/// Returns `None` if the key is absent, has an inline value, or the block
/// is empty.
pub fn extract_yaml_block(yaml: &str, key: &str) -> Option<String> {
    let mut lines = yaml.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with(char::is_whitespace) {
            continue; // not a top-level key
        }
        let Some(rest) = line.trim_end().strip_prefix(&format!("{}:", key)) else {
            continue;
        };
        if !rest.trim().is_empty() {
            return None; // inline scalar/list, not a block
        }

        let mut block_lines = Vec::new();
        let mut indent: Option<usize> = None;
        while let Some(&next_line) = lines.peek() {
            if next_line.trim().is_empty() {
                block_lines.push(String::new());
                lines.next();
                continue;
            }
            let this_indent = next_line.len() - next_line.trim_start().len();
            if this_indent == 0 {
                break; // dedented back to top level — block ended
            }
            let indent = *indent.get_or_insert(this_indent);
            if this_indent < indent {
                break;
            }
            block_lines.push(next_line[indent..].to_string());
            lines.next();
        }

        return if block_lines.iter().all(|l| l.trim().is_empty()) {
            None
        } else {
            Some(block_lines.join("\n"))
        };
    }
    None
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
        assert_eq!(
            extract_yaml_string(yaml, "name"),
            Some("weather".to_string())
        );
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

    #[test]
    fn test_extract_yaml_block_basic() {
        let yaml = "name: flight\nmetadata:\n  version: \"1.0.0\"\n  keywords: [a, b]\ndescription: x";
        let block = extract_yaml_block(yaml, "metadata").unwrap();
        assert_eq!(extract_yaml_string(&block, "version"), Some("1.0.0".to_string()));
        assert_eq!(extract_yaml_list(&block, "keywords"), vec!["a", "b"]);
        // The block does not leak sibling top-level keys.
        assert_eq!(extract_yaml_string(&block, "description"), None);
    }

    #[test]
    fn test_extract_yaml_block_missing_key_returns_none() {
        let yaml = "name: flight\ndescription: x";
        assert_eq!(extract_yaml_block(yaml, "metadata"), None);
    }

    #[test]
    fn test_extract_yaml_block_inline_value_is_not_a_block() {
        // `metadata: {}` or any inline value on the same line is not a
        // nested block this parser supports — must return None, not panic
        // or misparse.
        let yaml = "metadata: {}\nname: x";
        assert_eq!(extract_yaml_block(yaml, "metadata"), None);
    }

    #[test]
    fn test_extract_yaml_block_empty_block_returns_none() {
        let yaml = "metadata:\nname: x";
        assert_eq!(extract_yaml_block(yaml, "metadata"), None);
    }

    #[test]
    fn test_extract_yaml_block_preserves_nested_multiline_list_indentation() {
        // A nested multi-line list under the block key must dedent by only
        // the block's own indent, keeping the list's relative indentation
        // intact for extract_yaml_list's multi-line parsing.
        let yaml = "metadata:\n  arguments:\n    - from_city\n    - to_city\n  version: \"1.0\"";
        let block = extract_yaml_block(yaml, "metadata").unwrap();
        assert_eq!(
            extract_yaml_list(&block, "arguments"),
            vec!["from_city", "to_city"]
        );
        assert_eq!(extract_yaml_string(&block, "version"), Some("1.0".to_string()));
    }

    #[test]
    fn test_neutralize_spoofing_rtl_override() {
        // U+202E (RLO) reverses display order — classic Trojan Source
        let malicious = "src/\u{202E}txt.js";
        let cleaned = super::neutralize_spoofing(malicious);
        assert_eq!(cleaned, "src/[U+202E]txt.js");
        assert!(!super::has_spoofing_chars(&cleaned));
        assert!(super::has_spoofing_chars(malicious));
    }

    #[test]
    fn test_neutralize_spoofing_zero_width() {
        let malicious = "pass\u{200B}word"; // ZWSP inserted
        let cleaned = super::neutralize_spoofing(malicious);
        assert_eq!(cleaned, "pass[U+200B]word");
    }

    #[test]
    fn test_neutralize_spoofing_bom() {
        let with_bom = "\u{FEFF}hello";
        let cleaned = super::neutralize_spoofing(with_bom);
        assert_eq!(cleaned, "[U+FEFF]hello");
    }

    #[test]
    fn test_neutralize_spoofing_preserves_normal_text() {
        let normal = "Hello 世界 🌍 'path/to/file.rs'";
        let cleaned = super::neutralize_spoofing(normal);
        assert_eq!(cleaned, normal);
        assert!(!super::has_spoofing_chars(normal));
    }

    #[test]
    fn test_neutralize_spoofing_isolates() {
        let with_isolate = "safe\u{2066}evil\u{2069}end";
        let cleaned = super::neutralize_spoofing(with_isolate);
        assert_eq!(cleaned, "safe[U+2066]evil[U+2069]end");
    }

    #[test]
    fn test_extract_yaml_bool() {
        let yaml =
            "user_invocable: true\nagent_invocable: false\nother: yes\nfoo: no\nbar: on\nbaz: off";
        assert_eq!(extract_yaml_bool(yaml, "user_invocable"), Some(true));
        assert_eq!(extract_yaml_bool(yaml, "agent_invocable"), Some(false));
        assert_eq!(extract_yaml_bool(yaml, "other"), Some(true));
        assert_eq!(extract_yaml_bool(yaml, "foo"), Some(false));
        assert_eq!(extract_yaml_bool(yaml, "bar"), Some(true));
        assert_eq!(extract_yaml_bool(yaml, "baz"), Some(false));
        assert_eq!(extract_yaml_bool(yaml, "missing"), None);
        let yaml2 = "flag: notabool";
        assert_eq!(extract_yaml_bool(yaml2, "flag"), None);
    }
}
