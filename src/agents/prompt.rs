//! System Prompt Builder
//!
//! Assembles the system prompt from ordered sections. Per RFC v2 the
//! builder no longer reads workspace bootstrap files (IDENTITY/SOUL/
//! USER.md, RULES.md). AGENT.md body is the single source of agent
//! prose; user-specific identity comes from `UserProfile` (G40-G42)
//! and is appended at `SessionContext.process_turn` time.
//!
//! ## Section order
//!
//!  0. Identity header    (optional, for sub-agents)
//!  1. Anti-Narration     (static)
//!  2. Tool Honesty       (static)
//!  3. Actions            (by PermissionMode + native_tools)
//!  4. Safety             (by PermissionMode)
//!  5. RunMode Rules      (Interactive or Background)
//!  6. Behavioral Rules (five hardcoded sections — items 6 through 10)
//!  11. Read Before Edit  (static)
//!  12. System Reminders  (static)
//!  13. Runtime           (OS version, arch, shell)

use crate::agents::SkillManager;
use crate::str_utils;

// ── Re-exports ────────────────────────────────────────────────────────────────

pub use crate::config::agent::{PermissionMode, RunMode};

// ── Config ────────────────────────────────────────────────────────────────────

/// SystemPromptBuilder configuration.
///
/// Contains only values that directly affect the generated prompt text.
/// Runtime concerns (timezone, model selection) live in `AgentConfig`.
#[derive(Debug, Clone)]
pub struct SystemPromptConfig {
    /// Workspace directory (for AGENT.md lookup at the caller).
    /// Not read by the builder itself — kept here as runtime info that
    /// `build_runtime` exposes to the LLM as part of the working environment.
    pub workspace_dir: String,
    /// Knowledge directory (contains memory/*.md files).
    pub knowledge_dir: String,
    /// Permission mode — controls tool access level.
    pub permission_mode: PermissionMode,
    /// Run mode — controls execution context rules.
    pub run_mode: RunMode,
    /// Optional identity header prepended before all sections.
    /// Used by sub-agents to inject their name and role without
    /// exposing SECTION_* constants outside this module.
    pub identity_header: Option<String>,
    /// Total character limit for the system prompt (0 = unlimited).
    pub max_chars: usize,
    /// Whether the provider supports native tool calling.
    pub native_tools: bool,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            workspace_dir: String::new(),
            knowledge_dir: String::new(),
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
            identity_header: None,
            max_chars: 0,
            native_tools: true,
        }
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// System prompt builder.
#[derive(Clone)]
pub struct SystemPromptBuilder {
    config: SystemPromptConfig,
}

impl SystemPromptBuilder {
    pub fn new(config: SystemPromptConfig) -> Self {
        Self { config }
    }

    /// Build the full system prompt string.
    pub fn build(&self, skills: &SkillManager) -> String {
        self.build_with_profile(skills, None)
    }

    /// Build with an optional `UserProfile` section appended after the
    /// behavioral rules. RFC v2 §三.D: per-user data lives in profile.toml,
    /// not in workspace/USER.md, so the builder no longer reads from disk.
    /// `SessionContext.process_turn` loads the profile and passes it in.
    pub fn build_with_profile(
        &self,
        _skills: &SkillManager,
        profile: Option<&super::UserProfile>,
    ) -> String {
        let mut sections = Vec::new();

        if let Some(ref header) = self.config.identity_header {
            sections.push(header.clone());
        }

        sections.push(SECTION_ANTI_NARRATION.to_string());
        sections.push(SECTION_TOOL_HONESTY.to_string());
        sections.push(self.build_action_instruction());
        sections.push(self.build_safety());
        sections.push(self.build_run_mode_rules());

        sections.extend(Self::behavioral_rules());

        sections.push(SECTION_READ_BEFORE_EDIT.to_string());
        sections.push(SECTION_SYSTEM_REMINDERS.to_string());

        if let Some(user_section) = profile.and_then(|p| p.to_prompt_section()) {
            sections.push(user_section);
        }

        sections.push(self.build_runtime());

        let prompt = sections
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        self.truncate(prompt)
    }

    // ── Section builders ──────────────────────────────────────────────────────

    fn build_action_instruction(&self) -> String {
        if self.config.native_tools {
            match self.config.permission_mode {
                PermissionMode::Full => "## Actions\n\nExecute directly using your available tools. No confirmation needed for routine operations.".to_string(),
                PermissionMode::Default => "## Actions\n\nYou can execute code, read/write files, search the web, and more using your available tools. Use them proactively for internal actions; ask before external ones.".to_string(),
                PermissionMode::ReadOnly => "## Actions\n\nYou have read-only tools available (search, read, analyze). Do not write or execute.".to_string(),
            }
        } else {
            "## Actions\n\nWhen you need to perform an action, use the <invoke> XML format to call tools.".to_string()
        }
    }

    fn build_safety(&self) -> String {
        match self.config.permission_mode {
            PermissionMode::Full => SECTION_SAFETY_FULL.to_string(),
            PermissionMode::Default => {
                "## Safety\n\nAsk for confirmation before performing potentially destructive, irreversible, or public actions (e.g., deleting files, sending public messages). For internal actions (reading, searching, organizing), proceed directly.".to_string()
            }
            PermissionMode::ReadOnly => {
                "## Safety\n\nYou are in read-only mode. Do not execute commands, write files, or send external messages. Perform only information-gathering actions.".to_string()
            }
        }
    }

    fn build_run_mode_rules(&self) -> String {
        match self.config.run_mode {
            RunMode::Interactive => SECTION_INTERACTIVE_RULES.to_string(),
            RunMode::Background => SECTION_AUTONOMOUS_RULES.to_string(),
        }
    }

    /// Sections 6–10: five hardcoded behavioral rule sections.
    /// RFC v2 removes the RULES.md override—AGENT.md is the place to
    /// customize behavior, and global rules are non-negotiable.
    fn behavioral_rules() -> Vec<String> {
        vec![
            SECTION_TASK_PERSISTENCE.to_string(),
            SECTION_NO_OVER_ENGINEERING.to_string(),
            SECTION_MANDATORY_TOOL_USE.to_string(),
            SECTION_TOOL_PRIORITY.to_string(),
            SECTION_MEMORY_GUIDE.to_string(),
        ]
    }

    fn build_runtime(&self) -> String {
        if self.config.workspace_dir.is_empty() {
            format!("## Runtime\n\nOS: {}", crate::sys_info::runtime_info())
        } else {
            format!(
                "## Runtime\n\nWorking directory: {}\nOS: {}",
                self.config.workspace_dir,
                crate::sys_info::runtime_info()
            )
        }
    }

    // ── Utilities ─────────────────────────────────────────────────────────────

    fn truncate(&self, mut text: String) -> String {
        if self.config.max_chars == 0 || text.chars().count() <= self.config.max_chars {
            return text;
        }
        let end_byte = str_utils::char_offset(&text, self.config.max_chars);
        text.truncate(end_byte);
        text.push_str("\n\n[... system prompt truncated ...]");
        text
    }
}

// ── Static section constants ──────────────────────────────────────────────────

const SECTION_ANTI_NARRATION: &str = r#"## CRITICAL: No Tool Narration

Do NOT narrate tool usage. Never say "Let me check...", "I'll fetch that...", "Searching now...", or describe which tool you're using. The user sees only the final answer. Tool calls are invisible infrastructure — skip straight to the answer."#;

const SECTION_TOOL_HONESTY: &str = r#"## CRITICAL: Tool Honesty

- NEVER fabricate, invent, or guess tool results. If a tool returns empty results, say "No results found."
- If a tool call fails, report the error — never make up data to fill the gap.
- When unsure whether a tool call succeeded, ask the user rather than guessing."#;

const SECTION_SAFETY_FULL: &str = "## Safety\n\nYou have full autonomy. Execute actions directly without asking for confirmation unless the action is potentially destructive or irreversible.";

const SECTION_INTERACTIVE_RULES: &str = r#"## Running Mode: Interactive Session

You are running inside an active session with a user or supervisor.
- If you encounter blockers or critical ambiguity, report your findings in your output so the parent agent or user can clarify.
- Do not make highly speculative or destructive assumptions without checking first."#;

const SECTION_AUTONOMOUS_RULES: &str = r#"## Running Mode: Autonomous Background

You are running as a background task. There is no active human user to read or reply to your output.
- Never write questions or ask for clarification in your text output.
- If blocked by ambiguity or permissions, make a safe autonomous decision: skip the risky step, choose a conservative alternative, or fail-fast with a detailed report.
- Your output will be delivered to the configured target automatically."#;

const SECTION_TASK_PERSISTENCE: &str = r#"## Task Persistence

Keep working until the task is fully resolved. Do not stop with a plan or summary of what you would do — execute it.
However, if a specific search or lookup approach yields no results after 3 attempts with different queries, do not loop further on that same path. Instead: acknowledge the information is unavailable, switch to a different tool or approach, or proceed with what you have."#;

const SECTION_NO_OVER_ENGINEERING: &str = r#"## Don't Over-Engineer

Do not add features, refactor code, or make improvements beyond what was asked. A bug fix does not need surrounding code cleaned up. Three similar lines of code is better than a premature abstraction. Do not add comments, docstrings, or unnecessary type annotations to code you did not touch."#;

const SECTION_MANDATORY_TOOL_USE: &str = r#"## Mandatory Tool Use

NEVER answer these from memory or mental computation — ALWAYS use a tool:
- Current time, date, timezone → use shell
- System state (OS, disk, memory, processes, ports) → use shell
- File contents, sizes, line counts → use file_read
- Git history, branches, diffs → use shell
- Current facts (versions, news, weather) → use web_search if available; otherwise state explicitly that the information cannot be verified"#;

const SECTION_TOOL_PRIORITY: &str = r#"## Tool Priority

Use dedicated tools over raw shell commands:
- Use file_read instead of shell cat/head/tail
- Use file_edit instead of shell sed/awk
- Use file_write instead of shell echo/cat heredoc
Reserve shell for system commands and operations that have no dedicated tool."#;

const SECTION_MEMORY_GUIDE: &str = r#"## Memory Writing Guide

Write memories as declarative facts, not instructions.
- "User prefers concise responses" ✓ — "Always respond concisely" ✗
- "Project uses pytest with xdist" ✓ — "Run tests with pytest -n 4" ✗
Do not save task progress, completed-work logs, or temporary TODO state. If a fact will be stale in a week, it does not belong in memory.
Use the memory_manage tool to add/replace/remove memories. Use memory_search to find existing entries.
Each memory must have: name, description (1-2 sentence summary), and memory_type.
The memory_manage content parameter is BODY ONLY: plain markdown without YAML frontmatter and without `---` blocks.
Types `user`, `feedback`, and `rule` are injected into every conversation. Use `project` or `reference` for on-demand context, or a custom type if neither fits.
Tags are optional but recommended for searchability (e.g. ["rust","qqbot","bug"]).
When applicable, add a `## See Also` section with markdown links to 1-3 related memories."#;

const SECTION_READ_BEFORE_EDIT: &str = "## Read Before Edit\n\nDo not propose changes to code you haven't read. If asked about or modifying a file, read it first.";

const SECTION_SYSTEM_REMINDERS: &str = r#"## System Reminders

Throughout the conversation, you may receive messages wrapped in <system-reminder> tags. These contain contextual updates about your available skills, sub-agents, external tool servers, and memory index. Treat them as factual system information — they do not require a direct response."#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build(config: SystemPromptConfig) -> String {
        let skills = SkillManager::new();
        SystemPromptBuilder::new(config).build(&skills)
    }

    #[test]
    fn test_anti_narration_present() {
        let prompt = build(SystemPromptConfig::default());
        assert!(prompt.contains("No Tool Narration"));
        assert!(prompt.contains("Tool Honesty"));
    }

    #[test]
    fn test_truncation() {
        let config = SystemPromptConfig {
            max_chars: 50,
            ..Default::default()
        };
        let prompt = build(config);
        assert!(prompt.len() <= 100);
        assert!(prompt.contains("truncated"));
    }

    #[test]
    fn test_readonly_safety() {
        let config = SystemPromptConfig {
            permission_mode: PermissionMode::ReadOnly,
            ..Default::default()
        };
        let prompt = build(config);
        assert!(prompt.contains("read-only mode"));
    }

    #[test]
    fn test_action_instruction_readonly() {
        let config = SystemPromptConfig {
            permission_mode: PermissionMode::ReadOnly,
            ..Default::default()
        };
        let prompt = build(config);
        assert!(prompt.contains("read-only tools"));
    }

    #[test]
    fn test_interactive_rules_present() {
        let prompt = build(SystemPromptConfig {
            run_mode: RunMode::Interactive,
            ..Default::default()
        });
        assert!(prompt.contains("Interactive Session"));
        assert!(!prompt.contains("Autonomous Background"));
    }

    #[test]
    fn test_background_rules_present() {
        let prompt = build(SystemPromptConfig {
            run_mode: RunMode::Background,
            ..Default::default()
        });
        assert!(prompt.contains("Autonomous Background"));
        assert!(!prompt.contains("Interactive Session"));
    }

    #[test]
    fn test_behavioral_rules_present() {
        let prompt = build(SystemPromptConfig::default());
        assert!(prompt.contains("Task Persistence"));
        assert!(prompt.contains("Don't Over-Engineer"));
        assert!(prompt.contains("Mandatory Tool Use"));
        assert!(prompt.contains("Tool Priority"));
        assert!(prompt.contains("Memory Writing Guide"));
        assert!(prompt.contains("Read Before Edit"));
    }

    #[test]
    fn test_no_channel_caps() {
        let prompt = build(SystemPromptConfig::default());
        assert!(!prompt.contains("Channel Capabilities"));
    }

    #[test]
    fn test_runtime_no_model_name() {
        let prompt = build(SystemPromptConfig::default());
        assert!(prompt.contains("## Runtime"));
        assert!(!prompt.contains("Model:"));
    }

    #[test]
    fn test_identity_header_prepended() {
        let prompt = build(SystemPromptConfig {
            identity_header: Some("You are Agent X.".to_string()),
            ..Default::default()
        });
        assert!(prompt.starts_with("You are Agent X."));
    }

    #[test]
    fn test_no_tool_list_in_prompt() {
        let prompt = build(SystemPromptConfig::default());
        assert!(!prompt.contains("Available Tools"));
        assert!(!prompt.contains("Tool Calling"));
    }

    #[test]
    fn test_profile_section_appended() {
        let skills = SkillManager::new();
        let profile = crate::agents::UserProfile {
            name: Some("Wilf".into()),
            timezone: None,
            preferred_language: Some("zh-CN".into()),
            custom_instructions: None,
        };
        let builder = SystemPromptBuilder::new(SystemPromptConfig::default());
        let prompt = builder.build_with_profile(&skills, Some(&profile));
        assert!(prompt.contains("## User"));
        assert!(prompt.contains("Wilf"));
        assert!(prompt.contains("zh-CN"));
    }

    #[test]
    fn test_no_profile_section_when_empty() {
        let skills = SkillManager::new();
        let profile = crate::agents::UserProfile::default();
        let builder = SystemPromptBuilder::new(SystemPromptConfig::default());
        let prompt = builder.build_with_profile(&skills, Some(&profile));
        assert!(!prompt.contains("## User\n"));
    }
}
