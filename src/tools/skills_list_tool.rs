//! SkillsListTool — list metadata of all skills.

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::SkillManager;
use crate::providers::{Tool, ToolResult};

pub struct SkillsListTool {
    skills: Arc<RwLock<SkillManager>>,
}

impl SkillsListTool {
    pub fn new(skills: Arc<RwLock<SkillManager>>) -> Self {
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
        let skills = self.skills.read();

        let mut entries: Vec<serde_json::Value> = skills
            .skills_iter()
            .map(|(_, skill)| {
                let mut entry = json!({
                    "name": skill.name,
                    "description": skill.description,
                });
                if let Some(ref v) = skill.when_to_use {
                    entry["when_to_use"] = json!(v);
                }
                if let Some(ref v) = skill.argument_hint {
                    entry["argument_hint"] = json!(v);
                }
                if let Some(ref v) = skill.version {
                    entry["version"] = json!(v);
                }
                if !skill.agent_invocable {
                    entry["agent_invocable"] = json!(false);
                }
                if !skill.user_invocable {
                    entry["user_invocable"] = json!(false);
                }
                if let Some(ref dir) = skill.skill_dir {
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
    use crate::agents::Skill;

    fn make_skill(name: &str, desc: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            keywords: vec![],
            prompt_body: String::new(),
            version: None,
            when_to_use: None,
            argument_hint: None,
            arguments: vec![],
            user_invocable: true,
            agent_invocable: true,
            skill_dir: None,
        }
    }

    #[tokio::test]
    async fn test_empty_skills() {
        let tool = SkillsListTool::new(Arc::new(RwLock::new(SkillManager::new())));
        let result = tool
            .execute(
                json!({}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
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
        let mut mgr = SkillManager::new();
        mgr.register(make_skill("beta", "Beta skill"));
        mgr.register(make_skill("alpha", "Alpha skill"));
        let tool = SkillsListTool::new(Arc::new(RwLock::new(mgr)));

        let result = tool
            .execute(
                json!({}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
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
        let mut mgr = SkillManager::new();
        let mut skill = make_skill("private", "Private");
        skill.agent_invocable = false;
        skill.user_invocable = false;
        mgr.register(skill);
        let tool = SkillsListTool::new(Arc::new(RwLock::new(mgr)));

        let result = tool
            .execute(
                json!({}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let entry = &v["skills"][0];
        assert_eq!(entry["agent_invocable"], false);
        assert_eq!(entry["user_invocable"], false);
    }
}
