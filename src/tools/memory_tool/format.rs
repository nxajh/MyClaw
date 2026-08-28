//! Memory file formatting: name validation, merged scan, frontmatter build, and linting.

use std::path::Path;

use crate::memory::{LinkRef, MemoryFile};

const MAX_NAME_LENGTH: usize = 64;

/// Validate a memory file name.
pub(super) fn validate_name(name: &str) -> Result<(), String> {
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

/// Scan memory files visible to a user: all agent-scope entries plus the
/// user's own user-scope entries, from the single flat memory root
/// (ownership via frontmatter, not path). Dedup by name, agent layer wins.
pub(super) fn scan_merged(memory_root: &Path, user_id: &str) -> Vec<crate::memory::MemoryFile> {
    let mut files: Vec<crate::memory::MemoryFile> = crate::memory::scan_memory_files(memory_root)
        .into_iter()
        .filter(|f| f.scope.as_deref().unwrap_or("agent") == "agent" || f.user_id.as_deref() == Some(user_id))
        .collect();
    files.sort_by(|a, b| (&a.mem_type, &a.name).cmp(&(&b.mem_type, &b.name)));
    files
}

pub(super) fn atomic_write(target: &Path, content: &str) -> std::io::Result<()> {
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}

pub(super) fn validate_body_only(content: &str) -> Result<(), String> {
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

pub(super) fn link_values(links: &[LinkRef]) -> Vec<serde_json::Value> {
    links
        .iter()
        .map(|l| serde_json::json!({ "target": l.target, "label": l.label }))
        .collect()
}

pub(super) fn lint_memory_content(name: &str, content: &str, files: &[MemoryFile]) -> Vec<String> {
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
/// P1-B2: ownership (`scope` + `user_id`) is always written — files without a
/// `scope` field would fall back to the agent layer on read.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_frontmatter(
    name: &str,
    description: &str,
    tags: &[String],
    mem_type: &str,
    inject: &str,
    created_at: &str,
    updated_at: Option<&str>,
    scope: &str,
    user_id: Option<&str>,
) -> String {
    let mut fm = format!(
        "---
name: {}
scope: {}
type: {}
inject: {}
created_at: {}",
        yaml_scalar(name),
        scope,
        yaml_scalar(mem_type),
        inject,
        yaml_scalar(created_at)
    );
    if scope == "user" {
        match user_id {
            Some(uid) => fm.push_str(&format!("\nuser_id: {}", yaml_scalar(uid))),
            // Callers validate this before writing; belt-and-braces default.
            None => fm.push_str("\nuser_id: unknown"),
        }
    }
    fm.push_str(&format!(
        "
description: {}",
        yaml_scalar(description)
    ));
    if let Some(ua) = updated_at {
        fm.push_str(&format!(
            "
updated_at: {}",
            yaml_scalar(ua)
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
