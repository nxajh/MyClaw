//! Agent Loader — 从 workspace/agents/ 目录加载 AGENT.md 文件。
//!
//! AGENT.md 使用 YAML front matter 格式：
//! ```markdown
//! ---
//! name: coder
//! description: "Expert programmer for writing and editing code"
//! tools: [shell, file_read, file_write, file_edit]
//! max_tool_calls: 30
//! model: null
//! ---
//!
//! # Coder Agent
//!
//! You are an expert programmer. Write clean, idiomatic code.
//! ```
//!
//! YAML front matter 定义结构化元数据，Markdown body 就是 system_prompt。
//! 这与 `skill_loader` 使用相同的解析基础设施（`str_utils`）。

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use crate::config::sub_agent::{AgentIsolation, SubAgentConfig};
use crate::str_utils::{extract_yaml_list, extract_yaml_string, parse_front_matter};

/// 解析 AGENT.md 文件为 SubAgentConfig。
pub fn parse_agent_file(path: &Path) -> Result<SubAgentConfig> {
    let content = std::fs::read_to_string(path)?;

    let (front_matter, body) = parse_front_matter(&content);

    let name = extract_yaml_string(&front_matter, "name").unwrap_or_else(|| {
        // Fallback: use directory name
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });

    let description = extract_yaml_string(&front_matter, "description").unwrap_or_default();

    let tools = extract_yaml_list(&front_matter, "tools");

    let max_tool_calls = extract_yaml_string(&front_matter, "max_tool_calls")
        .and_then(|s| s.parse::<usize>().ok());

    let model_raw = extract_yaml_string(&front_matter, "model").unwrap_or_default();
    let model = if model_raw.is_empty() || model_raw == "null" || model_raw == "~" {
        None
    } else {
        Some(model_raw)
    };

    let isolation = extract_yaml_string(&front_matter, "isolation")
        .map(|s| match s.to_lowercase().as_str() {
            "worktree" => AgentIsolation::Worktree,
            _ => AgentIsolation::Shared,
        })
        .unwrap_or_default();

    Ok(SubAgentConfig {
        name,
        system_prompt: body.trim().to_string(),
        tools,
        max_tool_calls,
        description: if description.is_empty() {
            None
        } else {
            Some(description)
        },
        model,
        isolation,
    })
}

/// 扫描 agents 目录，加载所有 AGENT.md 文件。
///
/// 目录结构：
/// ```text
/// workspace/agents/
/// ├── coder/
/// │   └── AGENT.md
/// └── researcher/
///     └── AGENT.md
/// ```
pub fn load_agents_from_dir(agents_dir: &Path) -> Vec<SubAgentConfig> {
    let mut agents = Vec::new();

    if !agents_dir.exists() {
        return agents;
    }

    let entries = match std::fs::read_dir(agents_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %agents_dir.display(), err = %e, "failed to read agents directory");
            return agents;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let agent_md = path.join("AGENT.md");
        if !agent_md.exists() {
            continue;
        }

        match parse_agent_file(&agent_md) {
            Ok(agent) => {
                info!(
                    name = %agent.name,
                    tools = ?agent.tools,
                    path = %agent_md.display(),
                    "sub-agent loaded"
                );
                agents.push(agent);
            }
            Err(e) => {
                warn!(path = %agent_md.display(), err = %e, "failed to parse AGENT.md");
            }
        }
    }

    // Sort by name for deterministic ordering
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    agents
}

/// Validate loaded agents: check for duplicate names and unknown tool references.
///
/// Returns a list of warnings (non-fatal issues).
pub fn validate_agents(agents: &[SubAgentConfig], known_tools: &[&str]) -> Vec<String> {
    let mut warnings = Vec::new();

    // Check for duplicate names
    let mut seen_names = std::collections::HashSet::new();
    for agent in agents {
        if !seen_names.insert(agent.name.clone()) {
            warnings.push(format!("duplicate agent name: '{}'", agent.name));
        }
    }

    // Check for unknown tool references
    let known: std::collections::HashSet<&str> = known_tools.iter().copied().collect();
    for agent in agents {
        for tool in &agent.tools {
            if !known.contains(tool.as_str()) {
                warnings.push(format!(
                    "agent '{}' references unknown tool '{}'",
                    agent.name, tool
                ));
            }
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_agent_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("coder");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let content = r#"---
name: coder
description: "Expert programmer"
tools: [shell, file_read, file_write, file_edit]
max_tool_calls: 30
---

# Coder Agent

You are an expert programmer. Write clean, idiomatic code.

## Rules
- Always verify compilation
"#;
        std::fs::write(agent_dir.join("AGENT.md"), content).unwrap();

        let agent = parse_agent_file(&agent_dir.join("AGENT.md")).unwrap();
        assert_eq!(agent.name, "coder");
        assert_eq!(agent.description.as_deref(), Some("Expert programmer"));
        assert_eq!(
            agent.tools,
            vec!["shell", "file_read", "file_write", "file_edit"]
        );
        assert_eq!(agent.max_tool_calls, Some(30));
        assert!(agent.model.is_none());
        assert!(agent.system_prompt.contains("# Coder Agent"));
        assert!(agent.system_prompt.contains("## Rules"));
    }

    #[test]
    fn test_parse_agent_file_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("simple");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let content = "---\nname: simple\n---\n\nYou are a simple agent.";
        std::fs::write(agent_dir.join("AGENT.md"), content).unwrap();

        let agent = parse_agent_file(&agent_dir.join("AGENT.md")).unwrap();
        assert_eq!(agent.name, "simple");
        assert!(agent.tools.is_empty());
        assert!(agent.max_tool_calls.is_none());
        assert!(agent.model.is_none());
        assert_eq!(agent.system_prompt, "You are a simple agent.");
    }

    #[test]
    fn test_parse_agent_file_with_model() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("researcher");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let content = "---\nname: researcher\nmodel: gpt-4o-mini\n---\n\nResearch stuff.";
        std::fs::write(agent_dir.join("AGENT.md"), content).unwrap();

        let agent = parse_agent_file(&agent_dir.join("AGENT.md")).unwrap();
        assert_eq!(agent.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn test_parse_agent_file_model_null() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let content = "---\nname: agent\nmodel: null\n---\n\nDo things.";
        std::fs::write(agent_dir.join("AGENT.md"), content).unwrap();

        let agent = parse_agent_file(&agent_dir.join("AGENT.md")).unwrap();
        assert!(agent.model.is_none());
    }

    #[test]
    fn test_parse_agent_fallback_name() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        // No "name:" in front matter — fallback to directory name
        let content = "---\n---\n\nDo things.";
        std::fs::write(agent_dir.join("AGENT.md"), content).unwrap();

        let agent = parse_agent_file(&agent_dir.join("AGENT.md")).unwrap();
        assert_eq!(agent.name, "my-agent");
    }

    #[test]
    fn test_load_agents_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(agents_dir.join("agent-a")).unwrap();
        std::fs::create_dir_all(agents_dir.join("agent-b")).unwrap();

        std::fs::write(
            agents_dir.join("agent-a").join("AGENT.md"),
            "---\nname: alpha\n---\n# Alpha",
        )
        .unwrap();
        std::fs::write(
            agents_dir.join("agent-b").join("AGENT.md"),
            "---\nname: beta\n---\n# Beta",
        )
        .unwrap();

        let agents = load_agents_from_dir(&agents_dir);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "alpha");
        assert_eq!(agents[1].name, "beta");
    }

    #[test]
    fn test_load_agents_missing_dir() {
        let agents = load_agents_from_dir(Path::new("/nonexistent"));
        assert!(agents.is_empty());
    }

    #[test]
    fn test_validate_agents_duplicate() {
        let agents = vec![
            SubAgentConfig {
                name: "coder".into(),
                system_prompt: "a".into(),
                tools: vec![],
                max_tool_calls: None,
                description: None,
                model: None,
                isolation: AgentIsolation::default(),
            },
            SubAgentConfig {
                name: "coder".into(),
                system_prompt: "b".into(),
                tools: vec![],
                max_tool_calls: None,
                description: None,
                model: None,
                isolation: AgentIsolation::default(),
            },
        ];
        let warnings = validate_agents(&agents, &[]);
        assert!(warnings.iter().any(|w| w.contains("duplicate")));
    }

    #[test]
    fn test_validate_agents_unknown_tool() {
        let agents = vec![SubAgentConfig {
            name: "coder".into(),
            system_prompt: "a".into(),
            tools: vec!["shell".into(), "nonexistent_tool".into()],
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::default(),
        }];
        let warnings = validate_agents(&agents, &["shell"]);
        assert!(warnings
            .iter()
            .any(|w| w.contains("nonexistent_tool")));
    }
}
