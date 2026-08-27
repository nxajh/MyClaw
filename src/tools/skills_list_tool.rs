//! SkillsListTool — list metadata of all skills.

use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::api::skill_registry::SkillRegistry;
use crate::providers::{Tool, ToolResult};

pub struct SkillsListTool {
    skills: Arc<dyn SkillRegistry>,
}

impl SkillsListTool {
    pub fn new<R: SkillRegistry + 'static>(skills: Arc<R>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl Tool for SkillsListTool {
    fn name(&self) -> &str {
        "skills_list"
    }

    fn description(&self) -> &str {
        "List all available skills with metadata. Returns more detail than the auto-injected \
         skill index. Use this when you need to browse skills, check if a specific skill exists, \
         or see supporting files within a skill."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let mut entries: Vec<serde_json::Value> = self
            .skills
            .list()
            .into_iter()
            .map(|skill| {
                let mut entry = json!({
                    "name": skill.name,
                    "description": skill.description,
                });
                if let Some(v) = skill.when_to_use {
                    entry["when_to_use"] = json!(v);
                }
                if let Some(v) = skill.argument_hint {
                    entry["argument_hint"] = json!(v);
                }
                if let Some(v) = skill.version {
                    entry["version"] = json!(v);
                }
                if !skill.agent_invocable {
                    entry["agent_invocable"] = json!(false);
                }
                if !skill.user_invocable {
                    entry["user_invocable"] = json!(false);
                }
                if let Some(dir) = skill.skill_dir {
                    entry["skill_dir"] = json!(dir.display().to_string());
                    let files = scan_skill_files(dir);
                    if !files.is_empty() {
                        entry["files"] = json!(files);
                    }
                }
                entry
            })
            .collect();

        entries.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });

        let count = entries.len();
        Ok(ToolResult {
            success: true,
            output: json!({
                "success": true,
                "count": count,
                "skills": entries,
                "hint": "Use skill_view(name) to load full instructions, or skill_view(name, file_path) to read supporting files."
            }).to_string(),
            error: None,
        })
    }
}

fn scan_skill_files(dir: &Path) -> HashMap<String, Vec<String>> {
    const ALLOWED_SUBDIRS: &[&str] = &["references", "scripts", "templates", "assets"];
    let mut files = HashMap::new();
    for subdir in ALLOWED_SUBDIRS {
        let d = dir.join(subdir);
        if d.exists() {
            let mut sub_files: Vec<String> = collect_files_recursive(&d)
                .into_iter()
                .filter_map(|p| {
                    p.strip_prefix(dir)
                        .ok()
                        .map(|rel| rel.to_string_lossy().to_string())
                })
                .collect();
            if !sub_files.is_empty() {
                sub_files.sort();
                files.insert(subdir.to_string(), sub_files);
            }
        }
    }
    files
}

fn collect_files_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            result.extend(collect_files_recursive(&path));
        } else if path.is_file() {
            result.push(path);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::skill_registry::{InMemorySkillRegistry, SkillSummary, SkillView};

    fn make_skill(name: &str, desc: &str) -> SkillView {
        SkillView {
            name: name.to_string(),
            description: desc.to_string(),
            prompt_body: String::new(),
            agent_invocable: true,
            skill_dir: None,
        }
    }

    fn registry() -> Arc<InMemorySkillRegistry> {
        Arc::new(InMemorySkillRegistry::new())
    }

    #[tokio::test]
    async fn test_empty_skills() {
        let tool = SkillsListTool::new(registry());
        let result = tool
            .execute(
                json!({}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
            )
            .await
            .unwrap();
        assert!(result.success);
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(v["count"], 0);
        assert!(v["skills"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_lists_all_skills() {
        let reg = registry();
        reg.upsert(make_skill("beta", "Beta skill"));
        reg.upsert(make_skill("alpha", "Alpha skill"));
        let tool = SkillsListTool::new(reg);

        let result = tool
            .execute(
                json!({}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
            )
            .await
            .unwrap();
        assert!(result.success);
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(v["count"], 2);
        // Should be sorted by name
        assert_eq!(v["skills"][0]["name"], "alpha");
        assert_eq!(v["skills"][1]["name"], "beta");
    }

    #[tokio::test]
    async fn test_non_invocable_fields_included() {
        let reg = registry();
        reg.upsert_summary(SkillSummary {
            name: "private".to_string(),
            description: "Private".to_string(),
            version: None,
            when_to_use: None,
            argument_hint: None,
            agent_invocable: false,
            user_invocable: false,
        });
        let tool = SkillsListTool::new(reg);

        let result = tool
            .execute(
                json!({}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let entry = &v["skills"][0];
        assert_eq!(entry["agent_invocable"], false);
        assert_eq!(entry["user_invocable"], false);
    }
}
