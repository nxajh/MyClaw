//! Skill Loader — 从 workspace/skills/ 目录加载 SKILL.md 文件。
//!
//! SKILL.md 使用 YAML front matter 格式：
//! ```markdown
//! ---
//! name: weather
//! description: "Get current weather conditions and forecasts."
//! keywords: [weather, forecast, temperature, rain]
//! ---
//!
//! # Weather Skill
//!
//! Use curl to fetch weather from wttr.in.
//! ```

use std::path::{Path, PathBuf};
use anyhow::Result;
use tracing::{info, warn};

/// 从 SKILL.md 解析的 Skill 定义
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub prompt_body: String,  // Markdown body（注入到 system prompt）
    pub source_path: PathBuf,
}

/// 解析 SKILL.md 文件
pub fn parse_skill_file(path: &Path) -> Result<SkillDefinition> {
    let content = std::fs::read_to_string(path)?;

    // 分离 YAML front matter 和 Markdown body
    let (front_matter, body) = parse_front_matter(&content);

    // 解析 YAML front matter
    let name = extract_yaml_string(&front_matter, "name")
        .unwrap_or_else(|| {
            // fallback: 用目录名
            path.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        });

    let description = extract_yaml_string(&front_matter, "description")
        .unwrap_or_default();

    let keywords = extract_yaml_list(&front_matter, "keywords");

    Ok(SkillDefinition {
        name,
        description,
        keywords,
        prompt_body: body.trim().to_string(),
        source_path: path.to_path_buf(),
    })
}

/// 扫描 skills 目录，加载所有 SKILL.md
pub fn load_skills_from_dir(skills_dir: &Path) -> Vec<SkillDefinition> {
    let mut skills = Vec::new();

    if !skills_dir.exists() {
        return skills;
    }

    let entries = match std::fs::read_dir(skills_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %skills_dir.display(), error = %e, "failed to read skills directory");
            return skills;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        match parse_skill_file(&skill_md) {
            Ok(skill) => {
                info!(name = %skill.name, path = %skill_md.display(), "skill loaded");
                skills.push(skill);
            }
            Err(e) => {
                warn!(path = %skill_md.display(), error = %e, "failed to parse SKILL.md");
            }
        }
    }

    // 按 name 排序
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

// ── YAML Front Matter 解析 ──────────────────────────────────────────

/// 分离 YAML front matter 和 Markdown body
fn parse_front_matter(content: &str) -> (String, String) {
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return (String::new(), trimmed.to_string());
    }

    // 找第二个 ---
    if let Some(end) = trimmed[3..].find("\n---") {
        let front_matter = trimmed[3..3 + end].trim().to_string();
        let body = trimmed[3 + end + 4..].trim().to_string();  // skip "\n---\n"
        return (front_matter, body);
    }

    (String::new(), trimmed.to_string())
}

/// 从 YAML 中提取字符串值
fn extract_yaml_string(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&format!("{}:", key)) {
            let value = rest.trim();
            // 去掉引号
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

/// 从 YAML 中提取列表值
fn extract_yaml_list(yaml: &str, key: &str) -> Vec<String> {
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
                // 列表结束
                break;
            }
        } else if let Some(rest) = line.trim().strip_prefix(&format!("{}:", key)) {
            let rest = rest.trim();
            if rest.starts_with('[') {
                // 内联列表: keywords: [a, b, c]
                let inner = rest.trim_matches(|c| c == '[' || c == ']');
                items = inner.split(',')
                    .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                break;
            } else if rest.is_empty() {
                // 多行列表
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
        let content = r#"---
name: weather
description: "Get weather"
keywords: [weather, forecast]
---

# Weather Skill

Use curl."#;

        let (fm, body) = parse_front_matter(content);
        assert!(fm.contains("name: weather"));
        assert!(body.contains("# Weather Skill"));
    }

    #[test]
    fn test_parse_no_front_matter() {
        let content = "# Just a skill\n\nNo front matter.";
        let (fm, body) = parse_front_matter(content);
        assert!(fm.is_empty());
        assert_eq!(body, "# Just a skill\n\nNo front matter.");
    }

    #[test]
    fn test_extract_yaml_string() {
        let yaml = "name: weather\ndescription: \"Get weather\"";
        assert_eq!(extract_yaml_string(yaml, "name"), Some("weather".to_string()));
        assert_eq!(extract_yaml_string(yaml, "description"), Some("Get weather".to_string()));
        assert_eq!(extract_yaml_string(yaml, "missing"), None);
    }

    #[test]
    fn test_extract_yaml_list_inline() {
        let yaml = "keywords: [weather, forecast, \"temperature\"]";
        let items = extract_yaml_list(yaml, "keywords");
        assert_eq!(items, vec!["weather", "forecast", "temperature"]);
    }

    #[test]
    fn test_extract_yaml_list_multiline() {
        let yaml = "keywords:\n  - weather\n  - forecast\n  - temperature";
        let items = extract_yaml_list(yaml, "keywords");
        assert_eq!(items, vec!["weather", "forecast", "temperature"]);
    }

    #[test]
    fn test_parse_skill_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("weather");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = r#"---
name: weather
description: "Get weather"
keywords: [weather]
---

# Weather

Use curl."#;

        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.name, "weather");
        assert_eq!(skill.description, "Get weather");
        assert_eq!(skill.keywords, vec!["weather"]);
        assert!(skill.prompt_body.contains("# Weather"));
    }

    #[test]
    fn test_load_skills_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("skill-a")).unwrap();
        std::fs::create_dir_all(skills_dir.join("skill-b")).unwrap();

        std::fs::write(
            skills_dir.join("skill-a").join("SKILL.md"),
            "---\nname: skill-a\n---\n# Skill A"
        ).unwrap();
        std::fs::write(
            skills_dir.join("skill-b").join("SKILL.md"),
            "---\nname: skill-b\n---\n# Skill B"
        ).unwrap();

        let skills = load_skills_from_dir(&skills_dir);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "skill-a");
        assert_eq!(skills[1].name, "skill-b");
    }

    #[test]
    fn test_load_skills_missing_dir() {
        let skills = load_skills_from_dir(Path::new("/nonexistent"));
        assert!(skills.is_empty());
    }
}
