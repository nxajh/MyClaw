//! System Prompt Builder
//!
//! Assembles the system prompt from ordered sections per §10 architecture.
//!
//! ## Section order
//!
//! - 0. Anti-narration
//! - 0b. Tool Honesty
//! - 1c. Action instruction (native vs XML protocol)
//! - 2. Safety (autonomy_level)
//! - 3. Skills (Full or Compact)
//! - 4. Workspace
//! - 5. Bootstrap files (OpenClaw format)
//! - 6. Runtime
//! - 7. Channel Capabilities (skip in compact)
//! - 9. Truncation (max_system_prompt_chars)

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::agents::SkillManager;
use crate::str_utils;

// ── Config types ─────────────────────────────────────────────────────────────

/// Autonomy level controls safety section.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    /// Full autonomy — execute directly.
    Full,
    /// Default — ask for confirmation.
    #[default]
    Default,
    /// Read-only — no external actions.
    ReadOnly,
}

/// Skill injection mode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillsPromptInjectionMode {
    /// Full: name + description.
    Full,
    /// Compact: name only.
    #[default]
    Compact,
}

/// SystemPromptBuilder configuration.
#[derive(Debug, Clone)]
pub struct SystemPromptConfig {
    /// Workspace directory (contains SOUL.md, USER.md, etc.).
    pub workspace_dir: String,
    /// Current model name (e.g. "minimax-cn/MiniMax-M2.7").
    pub model_name: String,
    /// Autonomy level (safety section).
    pub autonomy: AutonomyLevel,
    /// Skill injection mode.
    pub skills_mode: SkillsPromptInjectionMode,
    /// Compact context (tools/skills name-only, skip channel caps).
    pub compact: bool,
    /// Total character limit (0 = unlimited).
    pub max_chars: usize,
    /// Per-bootstrap-file character limit.
    pub bootstrap_max_chars: usize,
    /// Whether the provider supports native tool calling.
    pub native_tools: bool,
    /// Channel name (e.g. "wechat", "telegram").
    pub channel_name: Option<String>,
    /// Host information for Runtime section.
    pub host_info: Option<String>,
    /// Timezone offset in hours (e.g. 8 for UTC+8). Used for date injection.
    pub timezone_offset: i32,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            workspace_dir: String::new(),
            model_name: String::new(),
            autonomy: AutonomyLevel::Default,
            skills_mode: SkillsPromptInjectionMode::Compact,
            compact: false,
            max_chars: 0,
            bootstrap_max_chars: 20_000,
            native_tools: true,
            channel_name: None,
            host_info: None,
            timezone_offset: 8,
        }
    }
}

// ── Builder ──────────────────────────────────────────────────────────────────

/// System prompt builder with fluent API.
#[derive(Clone)]
pub struct SystemPromptBuilder {
    config: SystemPromptConfig,
}

impl SystemPromptBuilder {
    pub fn new(config: SystemPromptConfig) -> Self {
        Self { config }
    }

    /// Build the full system prompt string.
    pub fn build(
        &self,
        skills: &SkillManager,
    ) -> String {
        let mut sections = vec![
            SECTION_ANTI_NARRATION.to_string(),
            SECTION_TOOL_HONESTY.to_string(),
            self.build_action_instruction(),
            self.build_safety(),
            SECTION_SYSTEM_REMINDERS.to_string(),
            self.build_skills(skills),
            self.build_workspace(),
            self.build_bootstrap_files(),
            self.build_memory_section(),
            self.build_runtime(),
        ];

        if !self.config.compact {
            sections.push(self.build_channel_caps());
        }

        let prompt = sections
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        // 9. Truncation
        self.truncate(prompt)
    }

    // ── Section builders ────────────────────────────────────────────────────

    fn build_action_instruction(&self) -> String {
        if self.config.native_tools {
            match self.config.autonomy {
                AutonomyLevel::Full => "## Actions\n\nExecute directly using your available tools. No confirmation needed for routine operations.".to_string(),
                AutonomyLevel::Default => "## Actions\n\nYou can execute code, read/write files, search the web, and more using your available tools. Use them proactively for internal actions; ask before external ones.".to_string(),
                AutonomyLevel::ReadOnly => "## Actions\n\nYou have read-only tools available (search, read, analyze). Do not write or execute.".to_string(),
            }
        } else {
            "## Actions\n\nWhen you need to perform an action, use the <invoke> XML format to call tools.".to_string()
        }
    }

    fn build_safety(&self) -> String {
        match self.config.autonomy {
            AutonomyLevel::Full => {
                "## Safety\n\nYou have full autonomy. Execute actions directly without asking for confirmation unless the action is potentially destructive or irreversible.".to_string()
            }
            AutonomyLevel::Default => {
                "## Safety\n\nAsk for confirmation before performing potentially destructive, irreversible, or public actions (e.g., deleting files, sending public messages). For internal actions (reading, searching, organizing), proceed directly.".to_string()
            }
            AutonomyLevel::ReadOnly => {
                "## Safety\n\nYou are in read-only mode. Do not execute commands, write files, or send external messages. Perform only information-gathering actions.".to_string()
            }
        }
    }

    fn build_skills(&self, _skills: &SkillManager) -> String {
        // Skills 通过 attachment 增量注入，不再嵌入 system prompt。
        // AttachmentManager 在每 turn 的 build_messages() 中生成
        // <system-reminder> 消息，包含 skill 列表。
        String::new()
    }

    fn build_workspace(&self) -> String {
        if self.config.workspace_dir.is_empty() {
            return String::new();
        }
        format!(
            "## Workspace\n\nWorking directory: {}\n\nYour workspace files (SOUL.md, USER.md, AGENTS.md, etc.) are pre-loaded below.",
            self.config.workspace_dir
        )
    }

    fn build_bootstrap_files(&self) -> String {
        if self.config.workspace_dir.is_empty() {
            return String::new();
        }

        let dir = Path::new(&self.config.workspace_dir);
        let files = [
            "SOUL.md",
            "USER.md",
            "AGENTS.md",
            "TOOLS.md",
            "IDENTITY.md",
            "BOOTSTRAP.md",
        ];

        let mut sections = Vec::new();
        for filename in files {
            let path = dir.join(filename);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let truncated = Self::truncate_str(trimmed, self.config.bootstrap_max_chars);
                sections.push(format!("### {}\n\n{}", filename, truncated));
            }
        }

        if sections.is_empty() {
            String::new()
        } else {
            format!("## Workspace Bootstrap Files\n\n{}", sections.join("\n\n"))
        }
    }

    fn build_memory_section(&self) -> String {
        crate::memory::build_memory_section(&self.config.workspace_dir)
    }

    fn build_runtime(&self) -> String {
        let host = self.config.host_info.as_deref().unwrap_or("unknown");
        let model = &self.config.model_name;
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        format!(
            "## Runtime\n\nHost: {} | OS: {} | Arch: {} | Model: {}",
            host, os, arch, model
        )
    }

    fn build_channel_caps(&self) -> String {
        let channel = self.config.channel_name.as_deref().unwrap_or("unknown");
        let caps = match channel {
            "wechat" => "- Markdown fully supported (tables, code blocks, bold, etc.)",
            "telegram" => "- Markdown formatting is supported.\n- Code blocks, tables, and bold text are rendered correctly.",
            "discord" | "whatsapp" => "- No markdown tables — use bullet lists instead.\n- No headers — use **bold** or CAPS for emphasis.",
            _ => "- Markdown formatting is supported.",
        };
        format!(
            "## Channel Capabilities\n\nYou are responding via {} channel.\n\n{}\n- TTS is handled by the channel transport — do not synthesize speech yourself.",
            channel, caps
        )
    }

    // ── Utilities ──────────────────────────────────────────────────────────

    fn truncate(&self, mut text: String) -> String {
        if self.config.max_chars == 0 || text.chars().count() <= self.config.max_chars {
            return text;
        }
        let end_byte = str_utils::char_offset(&text, self.config.max_chars);
        text.truncate(end_byte);
        text.push_str("\n\n[... system prompt truncated ...]");
        text
    }

    fn truncate_str(s: &str, max_chars: usize) -> String {
        if s.chars().count() <= max_chars {
            s.to_string()
        } else {
            let mut r = str_utils::truncate_chars(s, max_chars).to_string();
            r.push_str("\n\n[... truncated ...]");
            r
        }
    }
}

// ── Static section strings ─────────────────────────────────────────────────────

const SECTION_ANTI_NARRATION: &str = r#"## CRITICAL: No Tool Narration

Do NOT narrate tool usage. Never say "Let me check...", "I'll fetch that...", "Searching now...", or describe which tool you're using. The user sees only the final answer. Tool calls are invisible infrastructure — skip straight to the answer."#;

const SECTION_TOOL_HONESTY: &str = r#"## CRITICAL: Tool Honesty

- NEVER fabricate, invent, or guess tool results. If a tool returns empty results, say "No results found."
- If a tool call fails, report the error — never make up data to fill the gap.
- When unsure whether a tool call succeeded, ask the user rather than guessing."#;

const SECTION_SYSTEM_REMINDERS: &str = r#"## System Reminders

Throughout the conversation, you may receive messages wrapped in <system-reminder> tags. These contain contextual updates about your available skills, sub-agents, and external tool servers. Treat them as factual system information — they do not require a direct response."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anti_narration_present() {
        let config = SystemPromptConfig::default();
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(prompt.contains("No Tool Narration"));
        assert!(prompt.contains("Tool Honesty"));
    }

    #[test]
    fn test_compact_skips_channel_caps() {
        let config = SystemPromptConfig {
            compact: true,
            channel_name: Some("wechat".to_string()),
            ..SystemPromptConfig::default()
        };
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(!prompt.contains("Channel Capabilities"));
    }

    #[test]
    fn test_truncation() {
        let config = SystemPromptConfig {
            max_chars: 50,
            ..SystemPromptConfig::default()
        };
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(prompt.len() <= 100);
        assert!(prompt.contains("truncated"));
    }

    #[test]
    fn test_readonly_safety() {
        let config = SystemPromptConfig {
            autonomy: AutonomyLevel::ReadOnly,
            ..SystemPromptConfig::default()
        };
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(prompt.contains("read-only mode"));
    }

    #[test]
    fn test_channel_caps_wechat_has_tables() {
        let config = SystemPromptConfig {
            channel_name: Some("wechat".to_string()),
            ..SystemPromptConfig::default()
        };
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(prompt.contains("Markdown fully supported"));
    }

    #[test]
    fn test_channel_caps_discord_no_tables() {
        let config = SystemPromptConfig {
            channel_name: Some("discord".to_string()),
            ..SystemPromptConfig::default()
        };
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(prompt.contains("No markdown tables"));
    }

    #[test]
    fn test_action_instruction_readonly() {
        let config = SystemPromptConfig {
            autonomy: AutonomyLevel::ReadOnly,
            ..SystemPromptConfig::default()
        };
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(prompt.contains("read-only tools"));
    }

    #[test]
    fn test_no_tool_list_in_prompt() {
        let config = SystemPromptConfig {
            compact: true,
            ..SystemPromptConfig::default()
        };
        let builder = SystemPromptBuilder::new(config);
        let skills = SkillManager::new();
        let prompt = builder.build(&skills);
        assert!(!prompt.contains("Available Tools"));
        assert!(!prompt.contains("Tool Calling"));
    }
}
