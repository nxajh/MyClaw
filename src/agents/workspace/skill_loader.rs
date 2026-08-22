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
//!
//! `description` is injected into the system prompt every time this skill
//! is announced — per the Agent Skills standard (agentskills.io), this is
//! deliberately the *only* top-level text field; no local extension like a
//! separate `summary` field (issue #123 — reverts #119's non-standard
//! split, kept skills portable across agent implementations that share
//! `~/.agents/skills`, issue #83). Keep `description` compact: third
//! person, what + when to use it, a handful of trigger phrases — long-tail
//! trigger words belong in the skill body instead (loaded on demand via
//! `skill_view`, not injected on every turn). `injected_summary()` applies
//! a hard length cap regardless of author input, so a runaway description
//! has a bounded worst case.

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
    /// Skill status — "draft" skills are filtered out of normal loading.
    pub status: Option<String>,
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
    let status = extract_yaml_string(&front_matter, "status");

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
        status,
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
                // Draft skills (auto-extracted, not yet reviewed) are hidden
                // from normal loading — they don't appear in the system prompt.
                if skill.status.as_deref() == Some("draft") {
                    info!(name = %skill.name, "skill skipped (status: draft)");
                    continue;
                }
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

/// Scan `skills_dir` and return the names of skills with `status: draft` —
/// the ones `load_skills_from_dir` filters out of normal loading. Used by
/// `myclaw status`/`myclaw doctor` and the daily backlog reminder (issue
/// #89) to surface drafts that would otherwise be invisible.
pub fn list_draft_skill_names(skills_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return names;
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
        if let Ok(skill) = parse_skill_file(&skill_md) {
            if skill.status.as_deref() == Some("draft") {
                names.push(skill.name);
            }
        }
    }
    names.sort();
    names
}

/// Load skills across two layers: the local skills root and — when
/// present — the cross-agent shared library `~/.agents/skills` (issue
/// #83). Local skills always win a same-`name` conflict (front-matter
/// `name`, not directory name); a conflict is logged once so users can
/// tell why an installed shared-library skill didn't take effect.
///
/// `agents_dir: None` (config opted out, or the caller doesn't want the
/// shared layer at all) behaves exactly like `load_skills_from_dir(local_dir)`.
pub fn load_skills_layered(
    local_dir: &Path,
    agents_dir: Option<&Path>,
) -> Vec<SkillDefinition> {
    let local_defs = load_skills_from_dir(local_dir);

    let agents_defs = match agents_dir {
        Some(dir) => load_skills_from_dir(dir),
        None => Vec::new(),
    };
    if agents_defs.is_empty() {
        return local_defs;
    }

    let local_names: std::collections::HashSet<&str> =
        local_defs.iter().map(|d| d.name.as_str()).collect();

    let mut merged: Vec<SkillDefinition> = agents_defs
        .into_iter()
        .filter(|d| {
            if local_names.contains(d.name.as_str()) {
                warn!(
                    name = %d.name,
                    local_path = %local_dir.display(),
                    agents_path = %d.source_path.display(),
                    "skill name conflict: local skills root overrides ~/.agents/skills"
                );
                false
            } else {
                true
            }
        })
        .collect();
    merged.extend(local_defs);
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    merged
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

    /// issue #123: a stray `summary:` key (e.g. a not-yet-migrated
    /// SKILL.md from before the #119 revert) must be silently ignored, not
    /// choke parsing or leak into any field.
    #[test]
    fn test_parse_skill_file_ignores_stray_summary_key() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("weather2");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = r#"---
name: weather2
summary: "Get the weather."
description: "Get current weather conditions. Trigger words: weather, forecast, rain."
---

# Weather"#;
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.name, "weather2");
        assert!(skill.description.contains("Trigger words"));
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

    fn write_skill(dir: &Path, folder: &str, name: &str, body: &str) {
        let skill_dir = dir.join(folder);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn test_load_skills_layered_merges_both_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let agents = dir.path().join("agents");
        write_skill(&local, "skill-a", "skill-a", "# A (local)");
        write_skill(&agents, "skill-b", "skill-b", "# B (shared)");

        let merged = load_skills_layered(&local, Some(&agents));
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "skill-a");
        assert_eq!(merged[1].name, "skill-b");
    }

    #[test]
    fn test_load_skills_layered_local_wins_name_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let agents = dir.path().join("agents");
        write_skill(&local, "skill-a", "skill-a", "# local version");
        write_skill(&agents, "skill-a", "skill-a", "# shared version");

        let merged = load_skills_layered(&local, Some(&agents));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].prompt_body, "# local version");
    }

    #[test]
    fn test_load_skills_layered_none_agents_dir_is_local_only() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        write_skill(&local, "skill-a", "skill-a", "# A");

        let merged = load_skills_layered(&local, None);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "skill-a");
    }

    #[test]
    fn test_load_skills_layered_missing_agents_dir_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        write_skill(&local, "skill-a", "skill-a", "# A");

        let merged = load_skills_layered(&local, Some(Path::new("/nonexistent")));
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_list_draft_skill_names_finds_only_drafts() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("active-skill")).unwrap();
        std::fs::write(
            skills_dir.join("active-skill/SKILL.md"),
            "---\nname: active-skill\ndescription: \"x\"\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(skills_dir.join("draft-skill")).unwrap();
        std::fs::write(
            skills_dir.join("draft-skill/SKILL.md"),
            "---\nname: draft-skill\ndescription: \"x\"\nstatus: draft\n---\nbody",
        )
        .unwrap();

        let drafts = list_draft_skill_names(&skills_dir);
        assert_eq!(drafts, vec!["draft-skill".to_string()]);

        // Active skill still loads normally and drafts stay excluded from it.
        let loaded = load_skills_from_dir(&skills_dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "active-skill");
    }

    #[test]
    fn test_list_draft_skill_names_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_draft_skill_names(&dir.path().join("nonexistent")).is_empty());
    }
}
