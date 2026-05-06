//! SkillTool — LLM 通过此工具按需加载 skill 全文指令。
//!
//! Tool schema 是静态的（不含 skill 列表）。
//! LLM 从 attachment 中获取可用 skill 名称，调用 `use_skill` 加载全文。
//! Skill body 通过标准 tool call/result 自然进入 session history。

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::agents::SkillManager;
use crate::providers::{Tool, ToolResult};

/// The `use_skill` tool — loads a skill's full instructions on demand.
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
        "use_skill"
    }

    fn description(&self) -> &str {
        "Load a skill's full instructions by name. Use this when the task matches \
         a skill you see listed in system reminders. Returns the skill's complete \
         behavioral guidance for you to follow."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the skill to activate."
                }
            },
            "required": ["name"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'name' is required"))?;

        let skills = self.skills.read();
        let skill = skills.get(name).ok_or_else(|| {
            let available: Vec<&str> = skills.skills_iter().map(|(n, _)| n).collect();
            anyhow::anyhow!(
                "Unknown skill '{}'. Available: {}",
                name,
                available.join(", ")
            )
        })?;

        if skill.prompt_body.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("Skill '{}' has no additional instructions.", name),
                error: None,
            });
        }

        Ok(ToolResult {
            success: true,
            output: skill.prompt_body.clone(),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Skill;

    #[test]
    fn test_skill_tool_spec() {
        let mgr = Arc::new(RwLock::new(SkillManager::new()));
        let tool = SkillTool::new(mgr);
        assert_eq!(tool.name(), "use_skill");
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"][0], "name");
    }

    #[tokio::test]
    async fn test_execute_known_skill() {
        let mut mgr = SkillManager::new();
        mgr.register(Skill {
            name: "test".to_string(),
            description: "test desc".to_string(),
            keywords: vec![],
            prompt_body: "## Test Instructions\nDo the thing.".to_string(),
        });
        let tool = SkillTool::new(Arc::new(RwLock::new(mgr)));

        let result = tool.execute(json!({"name": "test"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Test Instructions"));
    }

    #[tokio::test]
    async fn test_execute_unknown_skill() {
        let tool = SkillTool::new(Arc::new(RwLock::new(SkillManager::new())));

        let result = tool.execute(json!({"name": "nonexistent"})).await;
        assert!(result.is_err());
    }
}
