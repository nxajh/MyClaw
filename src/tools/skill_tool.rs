//! SkillTool — loads skill full text or auxiliary file on demand via the LLM.

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::SkillManager;
use crate::providers::{Tool, ToolResult};

pub struct SkillTool {
    skills: Arc<RwLock<SkillManager>>,
}

impl SkillTool {
    pub fn new(skills: Arc<RwLock<SkillManager>>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Load a skill's full instructions or a supporting file. Use this when the task matches \
         a skill you see listed in system reminders. Returns the skill's complete behavioral \
         guidance for you to follow."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the skill."
                },
                "file_path": {
                    "type": "string",
                    "description": "Optional. Relative path to a supporting file within the skill directory, e.g. 'scripts/run.py'. Omit to load the main SKILL.md."
                }
            },
            "required": ["name"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'name' is required"))?;
        let file_path = args["file_path"].as_str();

        let skills = self.skills.read();

        let skill = match skills.get(name) {
            Some(s) => s,
            None => {
                let available: Vec<&str> = skills.skills_iter().map(|(n, _)| n).collect();
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "success": false,
                        "error": format!("Skill '{}' not found.", name),
                        "available_skills": available,
                        "hint": "Use skills_list to see all available skills"
                    })
                    .to_string(),
                    error: None,
                });
            }
        };

        if !skill.agent_invocable {
            return Ok(ToolResult {
                success: false,
                output: json!({
                    "success": false,
                    "error": format!("Skill '{}' is not agent-invocable.", name)
                })
                .to_string(),
                error: None,
            });
        }

        match file_path {
            None => {
                // Load main SKILL.md
                if skill.prompt_body.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: json!({
                            "success": true,
                            "name": name,
                            "content": "",
                            "message": "Skill has no instructions."
                        })
                        .to_string(),
                        error: None,
                    });
                }

                // Substitute ${SKILL_DIR} with the actual absolute path so
                // the LLM sees concrete paths like /home/user/.myclaw/workspace/skills/foo/scripts/run.sh
                // instead of having to guess or reconstruct the directory.
                let rendered_content = skill
                    .skill_dir
                    .as_deref()
                    .map(|dir| substitute_template_vars(&skill.prompt_body, dir))
                    .unwrap_or_else(|| skill.prompt_body.clone());

                let linked_files = skill
                    .skill_dir
                    .as_deref()
                    .map(scan_skill_files)
                    .filter(|m| !m.is_empty());

                let usage_hint = linked_files.as_ref().map(|_| {
                    format!(
                        "To view linked files, call skill_view(name='{}', file_path=...) \
                         where file_path is e.g. 'references/api.md' or 'scripts/run.py'",
                        name
                    )
                });

                Ok(ToolResult {
                    success: true,
                    output: json!({
                        "success": true,
                        "name": skill.name,
                        "description": skill.description,
                        "content": rendered_content,
                        "skill_dir": skill.skill_dir.as_deref().map(|p| p.display().to_string()),
                        "linked_files": linked_files,
                        "usage_hint": usage_hint,
                    })
                    .to_string(),
                    error: None,
                })
            }
            Some(fp) => {
                // Validate path before anything else.
                if fp.contains("..") {
                    return Ok(ToolResult {
                        success: false,
                        output: json!({
                            "success": false,
                            "error": "Path traversal not allowed."
                        })
                        .to_string(),
                        error: None,
                    });
                }

                let skill_dir = match &skill.skill_dir {
                    Some(d) => d.clone(),
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: json!({
                                "success": false,
                                "error": "Skill has no directory (cannot read supporting files)."
                            })
                            .to_string(),
                            error: None,
                        });
                    }
                };

                let target = skill_dir.join(fp);
                if !target.starts_with(&skill_dir) {
                    return Ok(ToolResult {
                        success: false,
                        output: json!({
                            "success": false,
                            "error": "Path traversal not allowed."
                        })
                        .to_string(),
                        error: None,
                    });
                }

                if !target.exists() {
                    let available = scan_skill_files(&skill_dir);
                    return Ok(ToolResult {
                        success: false,
                        output: json!({
                            "success": false,
                            "error": format!("File '{}' not found in skill '{}'.", fp, name),
                            "available_files": available,
                            "hint": "Use one of the available file paths listed above"
                        })
                        .to_string(),
                        error: None,
                    });
                }

                match std::fs::read_to_string(&target) {
                    Ok(text) => Ok(ToolResult {
                        success: true,
                        output: json!({
                            "success": true,
                            "name": name,
                            "file": fp,
                            "content": text
                        })
                        .to_string(),
                        error: None,
                    }),
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        let size = target.metadata().map(|m| m.len()).unwrap_or(0);
                        let ext = target
                            .extension()
                            .map(|e| e.to_string_lossy().to_string())
                            .unwrap_or_default();
                        Ok(ToolResult {
                            success: true,
                            output: json!({
                                "success": true,
                                "name": name,
                                "file": fp,
                                "content": "[Binary file, cannot display as text]",
                                "is_binary": true,
                                "file_type": ext,
                                "file_size": size
                            })
                            .to_string(),
                            error: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: json!({
                            "success": false,
                            "error": format!("Failed to read file: {}", e)
                        })
                        .to_string(),
                        error: None,
                    }),
                }
            }
        }
    }
}

/// Recursively collect all files under `dir`, returned as paths relative to `dir`.
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

/// Replace `${SKILL_DIR}` template variables in skill content with the
/// actual absolute path.  Tokens that cannot be resolved are left as-is
/// so the skill author can spot them.
///
/// Example: `${SKILL_DIR}/scripts/run.sh` →
///          `/home/user/.myclaw/workspace/skills/foo/scripts/run.sh`
fn substitute_template_vars(content: &str, skill_dir: &Path) -> String {
    let dir_str = skill_dir.display().to_string();
    content.replace("${SKILL_DIR}", &dir_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Skill;

    fn make_skill(name: &str, agent_invocable: bool) -> Skill {
        Skill {
            name: name.to_string(),
            description: "test desc".to_string(),
            keywords: vec![],
            prompt_body: "## Instructions\nDo the thing.".to_string(),
            version: None,
            when_to_use: None,
            argument_hint: None,
            arguments: vec![],
            user_invocable: true,
            agent_invocable,
            skill_dir: None,
        }
    }

    #[test]
    fn test_skill_tool_spec() {
        let mgr = Arc::new(RwLock::new(SkillManager::new()));
        let tool = SkillTool::new(mgr);
        assert_eq!(tool.name(), "skill_view");
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"][0], "name");
    }

    #[tokio::test]
    async fn test_execute_known_skill() {
        let mut mgr = SkillManager::new();
        mgr.register(make_skill("test", true));
        let tool = SkillTool::new(Arc::new(RwLock::new(mgr)));

        let result = tool
            .execute(
                json!({"name": "test"}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
            )
            .await
            .unwrap();
        assert!(result.success);
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(v["success"], true);
        assert!(v["content"].as_str().unwrap().contains("Instructions"));
    }

    #[tokio::test]
    async fn test_execute_unknown_skill() {
        let tool = SkillTool::new(Arc::new(RwLock::new(SkillManager::new())));
        let result = tool
            .execute(
                json!({"name": "nonexistent"}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
            )
            .await
            .unwrap();
        assert!(!result.success);
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(v["success"], false);
    }

    #[tokio::test]
    async fn test_execute_non_agent_invocable() {
        let mut mgr = SkillManager::new();
        mgr.register(make_skill("private", false));
        let tool = SkillTool::new(Arc::new(RwLock::new(mgr)));

        let result = tool
            .execute(
                json!({"name": "private"}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
            )
            .await
            .unwrap();
        assert!(!result.success);
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert!(v["error"].as_str().unwrap().contains("not agent-invocable"));
    }

    #[tokio::test]
    async fn test_path_traversal_rejected() {
        let mut mgr = SkillManager::new();
        mgr.register(make_skill("test", true));
        let tool = SkillTool::new(Arc::new(RwLock::new(mgr)));

        let result = tool
            .execute(
                json!({"name": "test", "file_path": "../../etc/passwd"}),
                &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None },
            )
            .await
            .unwrap();
        assert!(!result.success);
        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert!(v["error"].as_str().unwrap().contains("traversal"));
    }
}
