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

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::str_utils::{
    extract_yaml_bool, extract_yaml_list, extract_yaml_string, parse_front_matter,
};

/// 从 SKILL.md 解析的 Skill 定义
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub prompt_body: String,
    pub source_path: PathBuf,
    pub version: Option<String>,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub arguments: Vec<String>,
    pub user_invocable: bool,
    pub agent_invocable: bool,
}

/// 解析 SKILL.md 文件
pub fn parse_skill_file(path: &Path) -> Result<SkillDefinition> {
    let content = std::fs::read_to_string(path)?;

    // 分离 YAML front matter 和 Markdown body
    let (front_matter, body) = parse_front_matter(&content);

    // 解析 YAML front matter
    let name = extract_yaml_string(&front_matter, "name").unwrap_or_else(|| {
        // fallback: 用目录名
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });

    let description = extract_yaml_string(&front_matter, "description").unwrap_or_default();

    let keywords = extract_yaml_list(&front_matter, "keywords");
    let version = extract_yaml_string(&front_matter, "version");
    let when_to_use = extract_yaml_string(&front_matter, "when_to_use");
    let argument_hint = extract_yaml_string(&front_matter, "argument_hint");
    let arguments = extract_yaml_list(&front_matter, "arguments");
    let user_invocable = extract_yaml_bool(&front_matter, "user_invocable").unwrap_or(true);
    let agent_invocable = extract_yaml_bool(&front_matter, "agent_invocable").unwrap_or(true);

    Ok(SkillDefinition {
        name,
        description,
        keywords,
        prompt_body: body.trim().to_string(),
        source_path: path.to_path_buf(),
        version,
        when_to_use,
        argument_hint,
        arguments,
        user_invocable,
        agent_invocable,
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
            warn!(dir = %skills_dir.display(), err = %e, "failed to read skills directory");
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
                warn!(path = %skill_md.display(), err = %e, "failed to parse SKILL.md");
            }
        }
    }

    // 按 name 排序
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_parse_skill_file_new_fields() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("flight");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = r#"---
name: flight
description: "Search for flights"
version: "1.2.0"
when_to_use: "用户查机票时"
argument_hint: "[出发城市] [到达城市]"
arguments: [from_city, to_city]
user_invocable: true
agent_invocable: false
keywords: [flights, 机票]
---

# Flight Skill

Search flights."#;

        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.name, "flight");
        assert_eq!(skill.version, Some("1.2.0".to_string()));
        assert_eq!(skill.when_to_use, Some("用户查机票时".to_string()));
        assert_eq!(
            skill.argument_hint,
            Some("[出发城市] [到达城市]".to_string())
        );
        assert_eq!(skill.arguments, vec!["from_city", "to_city"]);
        assert!(skill.user_invocable);
        assert!(!skill.agent_invocable);
    }

    #[test]
    fn test_parse_skill_file_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("simple");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = "---\nname: simple\ndescription: \"Simple\"\n---\n# Simple";
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert!(skill.user_invocable);
        assert!(skill.agent_invocable);
        assert!(skill.version.is_none());
        assert!(skill.when_to_use.is_none());
        assert!(skill.arguments.is_empty());
    }

    #[test]
    fn test_load_skills_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("skill-a")).unwrap();
        std::fs::create_dir_all(skills_dir.join("skill-b")).unwrap();

        std::fs::write(
            skills_dir.join("skill-a").join("SKILL.md"),
            "---\nname: skill-a\n---\n# Skill A",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("skill-b").join("SKILL.md"),
            "---\nname: skill-b\n---\n# Skill B",
        )
        .unwrap();

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
