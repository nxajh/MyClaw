//! Skill Loader — 从 workspace/skills/ 目录加载 SKILL.md 文件。
//!
//! SKILL.md 使用 YAML front matter 格式：
//! ```markdown
//! ---
//! name: weather
//! description: "Get current weather conditions and forecasts."
//! metadata:
//!   keywords: [weather, forecast, temperature, rain]
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
//!
//! Only `name`/`description`/`metadata` are standard top-level frontmatter
//! keys. Everything else this loader reads (`keywords`, `version`,
//! `when_to_use`, `argument_hint`, `arguments`, `user_invocable`,
//! `agent_invocable`, `status`) belongs under `metadata:` (issue #125 —
//! #123 established the "no local top-level fields" principle for
//! `summary`; this converges the remaining 7 pre-existing non-standard
//! top-level fields the same way, plus fixes the one field among them that
//! was a real *behavioral* divergence, not just a portability smell: a
//! standard-compliant reader has no idea `status: draft` means "don't
//! inject this", so a shared draft skill silently behaved differently per
//! agent implementation). For a transitional period the loader still reads
//! these fields at the top level too (with a deprecation WARN identifying
//! the skill and field) so existing unmigrated SKILL.md files keep
//! working; `scripts/migrate_skill_frontmatter.py` moves them into
//! `metadata` in place.
//!
//! `name` is also validated against the standard's constraints (1-64
//! chars, lowercase letters/digits/hyphens, no leading/trailing/double
//! hyphen, and must equal the parent directory name) — violations are
//! logged, not rejected (the loader stays permissive), except a
//! name/directory mismatch, where the directory name wins as the
//! authoritative value (matching how every other loader function already
//! keys skills by directory).

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::str_utils::{
    extract_yaml_block, extract_yaml_bool, extract_yaml_list, extract_yaml_string,
    parse_front_matter,
};

/// Read a string field, preferring the top-level occurrence (with a
/// deprecation WARN) over the `metadata:` block — issue #125's transitional
/// dual-read.
fn dual_string(
    front_matter: &str,
    metadata: Option<&str>,
    key: &str,
    skill_name: &str,
) -> Option<String> {
    if let Some(v) = extract_yaml_string(front_matter, key) {
        warn_deprecated_top_level(skill_name, key);
        return Some(v);
    }
    metadata.and_then(|m| extract_yaml_string(m, key))
}

/// List-field counterpart of [`dual_string`].
fn dual_list(front_matter: &str, metadata: Option<&str>, key: &str, skill_name: &str) -> Vec<String> {
    let v = extract_yaml_list(front_matter, key);
    if !v.is_empty() {
        warn_deprecated_top_level(skill_name, key);
        return v;
    }
    metadata.map(|m| extract_yaml_list(m, key)).unwrap_or_default()
}

/// Bool-field counterpart of [`dual_string`].
fn dual_bool(
    front_matter: &str,
    metadata: Option<&str>,
    key: &str,
    skill_name: &str,
) -> Option<bool> {
    if let Some(v) = extract_yaml_bool(front_matter, key) {
        warn_deprecated_top_level(skill_name, key);
        return Some(v);
    }
    metadata.and_then(|m| extract_yaml_bool(m, key))
}

fn warn_deprecated_top_level(skill_name: &str, field: &str) {
    warn!(
        skill = %skill_name,
        field,
        "skill frontmatter: top-level `{field}` is deprecated, move it under `metadata:` \
         (issue #125) — run scripts/migrate_skill_frontmatter.py to migrate in place"
    );
}

/// Validate `name` against the Agent Skills spec's constraints and resolve
/// it against the skill's parent directory name. Non-fatal: violations are
/// logged, and only the directory-mismatch case (constraint 4) overrides
/// the returned name — the other three are just WARNed against whatever
/// name was parsed.
fn validate_and_resolve_name(front_matter_name: Option<String>, path: &Path) -> String {
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let name = front_matter_name.unwrap_or_else(|| dir_name.clone());

    let valid_chars = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid_chars {
        warn!(
            name = %name,
            path = %path.display(),
            "skill name violates Agent Skills spec: must be 1-64 chars, lowercase letters/digits/hyphens only"
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        warn!(name = %name, path = %path.display(), "skill name violates Agent Skills spec: must not start or end with a hyphen");
    }
    if name.contains("--") {
        warn!(name = %name, path = %path.display(), "skill name violates Agent Skills spec: must not contain consecutive hyphens");
    }
    if name != dir_name {
        warn!(
            name = %name,
            dir_name = %dir_name,
            path = %path.display(),
            "skill name does not match its parent directory name (Agent Skills spec requires them to match); using directory name as authoritative"
        );
        return dir_name;
    }
    name
}

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

    // 解析 YAML front matter — name/description 是标准字段，metadata 是唯一
    // 允许的扩展命名空间（issue #123/#125）。
    let raw_name = extract_yaml_string(&front_matter, "name");
    let name = validate_and_resolve_name(raw_name, path);

    let description = extract_yaml_string(&front_matter, "description").unwrap_or_default();

    let metadata = extract_yaml_block(&front_matter, "metadata");
    let metadata = metadata.as_deref();

    let keywords = dual_list(&front_matter, metadata, "keywords", &name);
    let version = dual_string(&front_matter, metadata, "version", &name);
    let when_to_use = dual_string(&front_matter, metadata, "when_to_use", &name);
    let argument_hint = dual_string(&front_matter, metadata, "argument_hint", &name);
    let arguments = dual_list(&front_matter, metadata, "arguments", &name);
    let user_invocable = dual_bool(&front_matter, metadata, "user_invocable", &name).unwrap_or(true);
    let agent_invocable = dual_bool(&front_matter, metadata, "agent_invocable", &name).unwrap_or(true);
    let status = dual_string(&front_matter, metadata, "status", &name);

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

    /// issue #125: the same 7 non-standard fields, read from under
    /// `metadata:` instead of the top level — the spec-compliant form.
    #[test]
    fn test_parse_skill_file_reads_fields_from_metadata_block() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("flight");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = r#"---
name: flight
description: "Search for flights"
metadata:
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
        assert_eq!(skill.keywords, vec!["flights", "机票"]);
        assert!(skill.user_invocable);
        assert!(!skill.agent_invocable);
    }

    /// issue #125: a top-level occurrence of a migratable field still wins
    /// over a `metadata:` value for the same key — the deprecation path
    /// reads correctly during the transitional dual-read period, it
    /// doesn't just silently prefer metadata and orphan the top-level
    /// value.
    #[test]
    fn test_parse_skill_file_top_level_wins_over_metadata_during_transition() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("dual");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = r#"---
name: dual
description: "x"
version: "top-level"
metadata:
  version: "under-metadata"
---
body"#;
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.version, Some("top-level".to_string()));
    }

    /// issue #125: `status: draft` under `metadata:` must still be
    /// filtered out of normal loading — the whole point of the migration
    /// is that this field's behavior doesn't change, only its location.
    #[test]
    fn test_draft_status_under_metadata_is_still_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("draft-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: draft-skill\ndescription: \"x\"\nmetadata:\n  status: draft\n---\nbody",
        )
        .unwrap();

        let drafts = list_draft_skill_names(&skills_dir);
        assert_eq!(drafts, vec!["draft-skill".to_string()]);
        assert!(load_skills_from_dir(&skills_dir).is_empty());
    }

    /// issue #125: `name` matching its parent directory is the common
    /// case and must be a complete no-op — no override, no warning path
    /// taken that would matter for behavior.
    #[test]
    fn test_name_validation_matching_directory_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("weather");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: weather\ndescription: \"x\"\n---\nbody",
        )
        .unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.name, "weather");
    }

    /// issue #125: a `name` that doesn't match its parent directory is
    /// resolved to the directory name — the loader's own view (every other
    /// function here already keys/dedupes skills by directory) becomes the
    /// authoritative one instead of silently trusting a stale/typo'd
    /// frontmatter value.
    #[test]
    fn test_name_validation_overrides_mismatched_name_with_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("ctrip-flights");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: ctrip-flight\ndescription: \"x\"\n---\nbody",
        )
        .unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.name, "ctrip-flights");
    }

    /// issue #125: charset/length, leading/trailing-hyphen, and
    /// double-hyphen violations are all non-fatal (parsing still
    /// succeeds and the name is returned as-is) — only the
    /// directory-mismatch case above actually changes the returned value.
    #[test]
    fn test_name_validation_other_violations_are_non_fatal() {
        let dir = tempfile::tempdir().unwrap();
        for (folder, bad_name) in [
            ("Has-Upper", "Has-Upper"),
            ("-leading", "-leading"),
            ("trailing-", "trailing-"),
            ("double--hyphen", "double--hyphen"),
        ] {
            let skill_dir = dir.path().join(folder);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {bad_name}\ndescription: \"x\"\n---\nbody"),
            )
            .unwrap();

            let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
            // Name matches its own (equally non-compliant) directory, so
            // parsing succeeds and returns the name unchanged — the
            // violations are logged, not corrected.
            assert_eq!(skill.name, bad_name);
        }
    }

    /// issue #125: round-trip sanity check against the exact output
    /// `scripts/migrate_skill_frontmatter.py` produces for a file that had
    /// a pre-existing `metadata:` block, a multi-line list, and several
    /// scalar fields needing quoting — nested 4 spaces deep (2 for the
    /// block + 2 for the list items under `arguments:`).
    #[test]
    fn test_parse_skill_file_reads_migration_script_output() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("flight");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = "---\nname: flight\ndescription: \"Search for flights\"\nmetadata:\n  \
            extra_note: \"already here\"\n  version: \"1.2.0\"\n  when_to_use: \"用户查机票时\"\n  \
            arguments:\n    - from_city\n    - to_city\n  status: \"draft\"\n---\n\n\
            # Flight Skill\n\nSearch flights.\n";
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.name, "flight");
        assert_eq!(skill.version, Some("1.2.0".to_string()));
        assert_eq!(skill.when_to_use, Some("用户查机票时".to_string()));
        assert_eq!(skill.arguments, vec!["from_city", "to_city"]);
        assert_eq!(skill.status, Some("draft".to_string()));
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
