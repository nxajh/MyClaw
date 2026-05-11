//! Attachment manager — 增量注入 skills/agents/MCP 列表。
//!
//! 每轮 turn 最多生成一条 `<system-reminder>` user 消息，存入 history。
//!
//! 设计参照 Claude Code 的增量 delta 模式：
//! - 三类信息（skills, agents, MCP）分别 diff，合并为一条消息
//! - 从 history 中的 `<system-reminder>` 重建 announced 状态
//! - Compaction 后旧 attachment 自然消失 → 自动触发全量重建

use std::collections::{HashMap, HashSet};

use crate::providers::ChatMessage;
use super::skills::SkillManager;

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AttachmentKind {
    SkillListing,
    AgentListing,
    McpInstructions,
    MemoryListing,
    DateInjection,
    AutonomyNotice,
}

/// 单类增量。
///
/// `added` / `removed` 的每个元素是格式化好的文本行。
#[derive(Default)]
struct Delta {
    added: Vec<String>,
    removed: Vec<String>,
}

/// Reconstructed announced state from history.
struct AnnouncedState {
    skills: HashSet<String>,
    agents: HashSet<String>,
    mcp: HashSet<String>,
}

// ── AttachmentManager ─────────────────────────────────────────────────────

/// 附件管理器 — 每个 AgentLoop 持有一个。
///
/// 不维护内存中的 announced 状态。
/// 每轮 turn 从 session history 中重建，然后与当前状态做 diff，
/// 合并渲染为一条 `<system-reminder>` 消息。
#[derive(Default)]
pub struct AttachmentManager {
    /// 本轮待发送增量
    pending: HashMap<AttachmentKind, Delta>,
    /// 当前注入过的 memory 索引文本（用于跨 turn diff）
    memory_index: Option<String>,
    /// 上次注入的日期 "YYYY-MM-DD"（用于区分首次 vs 日期变化）
    last_injected_date: Option<String>,
}

impl AttachmentManager {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Rebuild ──────────────────────────────────────────────────────

    /// 从 session history 中的 `<system-reminder>` 消息重建 announced 状态。
    ///
    /// 解析规则：
    /// - `- **name**` / `- **name**: desc` → added skill/agent
    /// - `- ~~name~~` → removed skill/agent
    /// - `### ServerName` → added MCP server
    fn rebuild_from_history(history: &[ChatMessage]) -> AnnouncedState {
        let mut skills = HashSet::new();
        let mut agents = HashSet::new();
        let mut mcp = HashSet::new();

        for msg in history {
            let text = msg.text_content();
            if !text.starts_with("<system-reminder>") || msg.role != "user" {
                continue;
            }

            let mut current_section: Option<&str> = None;

            for line in text.lines() {
                let trimmed = line.trim();

                // Section headers
                if trimmed.starts_with("## Skills") {
                    current_section = Some("skills");
                    continue;
                }
                if trimmed.starts_with("## Available Sub-Agents") {
                    current_section = Some("agents");
                    continue;
                }
                if trimmed.starts_with("## MCP Server Instructions") {
                    current_section = Some("mcp");
                    continue;
                }

                // Removed: - ~~name~~
                if trimmed.starts_with("- ~~") && trimmed.ends_with("~~") {
                    let inner = trimmed
                        .strip_prefix("- ~~")
                        .unwrap()
                        .strip_suffix("~~")
                        .unwrap()
                        .trim();
                    if !inner.is_empty() {
                        match current_section {
                            Some("skills") => { skills.remove(inner); }
                            Some("agents") => { agents.remove(inner); }
                            Some("mcp") => { mcp.remove(inner); }
                            _ => {}
                        }
                    }
                    continue;
                }

                // Added: - **name** or - **name: desc**
                if trimmed.starts_with("- **") {
                    if let Some(rest) = trimmed.strip_prefix("- **") {
                        if let Some(end) = rest.find("**") {
                            let raw = &rest[..end];
                            if !raw.is_empty() {
                                match current_section {
                                    Some("skills") => { skills.insert(raw.to_string()); }
                                    Some("agents") => {
                                        // Agent lines render as "- **name: desc**"
                                        // Extract just the name part before the colon.
                                        let name = raw.split(':').next().unwrap_or(raw).trim();
                                        agents.insert(name.to_string());
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    continue;
                }

                // MCP: ### ServerName
                if current_section == Some("mcp") && trimmed.starts_with("### ") {
                    let name = trimmed.strip_prefix("### ").unwrap().trim();
                    if !name.is_empty() {
                        mcp.insert(name.to_string());
                    }
                    continue;
                }
            }
        }

        AnnouncedState { skills, agents, mcp }
    }

    // ── Diff ────────────────────────────────────────────────────────────

    /// 与当前 SkillManager 做 diff，生成 skill listing delta。
    /// 从 history 重建 announced 状态。
    pub fn diff_skills(&mut self, skills: &SkillManager, history: &[ChatMessage]) {
        let announced = Self::rebuild_from_history(history);
        let current: HashSet<String> =
            skills.skills_iter().map(|(n, _)| n.to_string()).collect();

        let added: Vec<String> = current.difference(&announced.skills).cloned().collect();
        let removed: Vec<String> = announced.skills.difference(&current).cloned().collect();

        tracing::debug!(
            announced = ?announced.skills,
            current = ?current,
            added = ?added,
            removed = ?removed,
            "diff_skills: incremental diff from history"
        );

        if !added.is_empty() || !removed.is_empty() {
            self.pending.insert(
                AttachmentKind::SkillListing,
                Delta { added, removed },
            );
        }
    }

    /// 与当前 agent 列表做 diff。
    /// 从 history 重建 announced 状态。
    ///
    /// `agents`: `Vec<(name, description)>`
    pub fn diff_agents(&mut self, agents: &[(String, String)], history: &[ChatMessage]) {
        let announced = Self::rebuild_from_history(history);
        let current: HashSet<String> = agents.iter().map(|(n, _)| n.clone()).collect();

        let added_names: Vec<String> = current.difference(&announced.agents).cloned().collect();
        let removed: Vec<String> = announced.agents.difference(&current).cloned().collect();

        if !added_names.is_empty() || !removed.is_empty() {
            let desc_map: HashMap<&str, &str> =
                agents.iter().map(|(n, d)| (n.as_str(), d.as_str())).collect();
            let added: Vec<String> = added_names
                .iter()
                .map(|name| {
                    let desc = desc_map.get(name.as_str()).copied().unwrap_or("");
                    if desc.is_empty() {
                        name.clone()
                    } else {
                        format!("{}: {}", name, desc)
                    }
                })
                .collect();
            self.pending.insert(
                AttachmentKind::AgentListing,
                Delta { added, removed },
            );
        }
    }

    /// 与当前 MCP server instructions 做 diff。
    /// 从 history 重建 announced 状态。
    ///
    /// `servers`: `Vec<(server_name, instructions)>`
    pub fn diff_mcp(&mut self, servers: &[(String, String)], history: &[ChatMessage]) {
        let announced = Self::rebuild_from_history(history);
        let current: HashSet<String> = servers.iter().map(|(n, _)| n.clone()).collect();

        let added: Vec<String> = servers
            .iter()
            .filter(|(name, _)| !announced.mcp.contains(name))
            .filter(|(_, inst)| !inst.is_empty())
            .map(|(name, inst)| format!("### {}\n{}", name, inst))
            .collect();
        let removed: Vec<String> = announced.mcp.difference(&current).cloned().collect();

        if !added.is_empty() || !removed.is_empty() {
            self.pending.insert(
                AttachmentKind::McpInstructions,
                Delta { added, removed },
            );
        }
    }

    /// 与 memory 索引做 diff，变更时生成 system-reminder。
    /// 与 diff_skills 一致：始终检查 history，首次/compaction 后自动全量注入。
    pub fn diff_memory(
        &mut self,
        entries: &[crate::memory::IndexEntry],
        history: &[ChatMessage],
    ) {
        let new_text = crate::memory::format_memory_index(entries);

        // 检查 history 中是否已有相同的 memory 索引
        let marker = format!("## Memory\n\n{}", &new_text);
        let already_injected = history.iter().any(|msg| {
            msg.text_content().contains(&marker)
        });

        if already_injected {
            self.memory_index = Some(new_text);
            return;
        }

        // 首次 / 变更 / compaction 后 → 注入
        self.pending.insert(
            AttachmentKind::MemoryListing,
            Delta {
                added: vec![new_text.clone()],
                removed: vec![],
            },
        );
        self.memory_index = Some(new_text);
    }

    /// Check date injection. Generates a system-reminder with the current date
    /// when needed: first turn, date change, or after compaction (date message
    /// removed from history).
    pub fn diff_date(&mut self, timezone_offset: i32, history: &[ChatMessage]) {
        let now_utc = chrono::Utc::now();
        let local = now_utc + chrono::Duration::hours(timezone_offset as i64);
        let current_date = local.format("%Y-%m-%d").to_string();
        let current_weekday = local.format("%A").to_string();

        // Check if history already contains a date system-reminder for today.
        let date_marker = format!("Current date: {}", current_date);
        let date_in_history = history.iter().any(|msg| {
            let text = msg.text_content();
            text.contains("<system-reminder>") && text.contains(&date_marker)
        });

        if date_in_history {
            self.last_injected_date = Some(current_date);
            return;
        }

        // No date in history — inject.
        let is_date_change = self.last_injected_date.is_some();
        self.last_injected_date = Some(current_date.clone());

        let msg = if is_date_change {
            format!(
                "The date has changed. Today's date is now {} ({}).",
                current_date,
                current_weekday,
            )
        } else {
            let tz_str = if timezone_offset >= 0 {
                format!("+{}", timezone_offset)
            } else {
                format!("{}", timezone_offset)
            };
            format!(
                "Current date: {} ({}, UTC{}). Use this for any date-relative references.",
                current_date,
                current_weekday,
                tz_str,
            )
        };

        self.pending.insert(
            AttachmentKind::DateInjection,
            Delta { added: vec![msg], removed: vec![] },
        );
    }

    // ── Render ──────────────────────────────────────────────────────────

    /// 将 pending delta 合并为 `<system-reminder>` 文本字符串。
    ///
    /// 无 delta 时返回 `None`。
    /// 调用方负责将返回的文本前置到当前 user 消息中再写入 history。
    pub fn build_text(&self, skills: &SkillManager) -> Option<String> {
        let mut sections = Vec::new();

        if let Some(delta) = self.pending.get(&AttachmentKind::DateInjection) {
            sections.push(Self::render_date(delta));
        }
        if let Some(delta) = self.pending.get(&AttachmentKind::AutonomyNotice) {
            sections.push(Self::render_autonomy(delta));
        }
        if let Some(delta) = self.pending.get(&AttachmentKind::SkillListing) {
            sections.push(Self::render_skills(delta, skills));
        }
        if let Some(delta) = self.pending.get(&AttachmentKind::AgentListing) {
            sections.push(Self::render_agents(delta));
        }
        if let Some(delta) = self.pending.get(&AttachmentKind::McpInstructions) {
            sections.push(Self::render_mcp(delta));
        }
        if let Some(delta) = self.pending.get(&AttachmentKind::MemoryListing) {
            sections.push(Self::render_memory(delta));
        }

        if sections.is_empty() {
            return None;
        }

        Some(format!(
            "<system-reminder>\n{}\n</system-reminder>",
            sections.join("\n\n")
        ))
    }

    /// 将 pending delta 合并为一条 ChatMessage（向后兼容旧调用点）。
    pub fn build_message(&self, skills: &SkillManager) -> Option<ChatMessage> {
        self.build_text(skills).map(ChatMessage::user_text)
    }

    /// Notify the model that the autonomy level has changed.
    /// Called from `apply_session_override` when `autonomy` is updated.
    pub fn diff_autonomy(&mut self, new_level: &crate::config::agent::AutonomyLevel) {
        let label = match new_level {
            crate::config::agent::AutonomyLevel::Full => "full",
            crate::config::agent::AutonomyLevel::Default => "default",
            crate::config::agent::AutonomyLevel::ReadOnly => "read_only",
        };
        self.pending.insert(
            AttachmentKind::AutonomyNotice,
            Delta {
                added: vec![label.to_string()],
                removed: vec![],
            },
        );
    }

    /// 清空 pending（每 turn 结算后调用）。
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Debug helper: pending delta kinds.
    pub fn pending_keys(&self) -> Vec<&'static str> {
        self.pending.keys().map(|k| match k {
            AttachmentKind::SkillListing => "skills",
            AttachmentKind::AgentListing => "agents",
            AttachmentKind::McpInstructions => "mcp",
            AttachmentKind::MemoryListing => "memory",
            AttachmentKind::DateInjection => "date",
            AttachmentKind::AutonomyNotice => "autonomy",
        }).collect()
    }

    // ── Private render ──────────────────────────────────────────────────

    fn render_skills(delta: &Delta, skills: &SkillManager) -> String {
        let mut lines = vec!["## Skills".to_string()];

        if !delta.removed.is_empty() {
            lines.push("The following skills are no longer available:".to_string());
            for name in &delta.removed {
                lines.push(format!("- ~~{}~~", name));
            }
            lines.push(String::new());
        }

        if !delta.added.is_empty() {
            lines.push(
                "Skills provide behavioral instructions for specific tasks. \
                 Use the `use_skill` tool to load a skill's full instructions when needed."
                    .to_string(),
            );
            for name in &delta.added {
                let desc = skills.get(name).map(|s| s.description.as_str()).unwrap_or("");
                if desc.is_empty() {
                    lines.push(format!("- **{}**", name));
                } else {
                    lines.push(format!("- **{}**: {}", name, desc));
                }
            }
        }

        lines.join("\n")
    }

    fn render_agents(delta: &Delta) -> String {
        let mut lines = vec!["## Available Sub-Agents".to_string()];

        if !delta.removed.is_empty() {
            lines.push("The following sub-agents are no longer available:".to_string());
            for name in &delta.removed {
                lines.push(format!("- ~~{}~~", name));
            }
            lines.push(String::new());
        }

        if !delta.added.is_empty() {
            lines.push(
                r#"Use `agent_delegate(agent="name", task="...", mode="sync"/"async")` to delegate."#.to_string(),
            );
            for entry in &delta.added {
                lines.push(format!("- **{}**", entry));
            }
        }

        lines.join("\n")
    }

    fn render_memory(delta: &Delta) -> String {
        let mut lines = vec!["## Memory".to_string()];
        for entry in &delta.added {
            lines.push(entry.clone());
        }
        lines.join("\n\n")
    }

    fn render_mcp(delta: &Delta) -> String {
        let mut lines = vec!["## MCP Server Instructions".to_string()];

        for entry in &delta.added {
            lines.push(entry.clone());
        }

        if !delta.removed.is_empty() {
            lines.push(String::new());
            lines.push("The following MCP servers are no longer connected:".to_string());
            for name in &delta.removed {
                lines.push(format!("- ~~{}~~", name));
            }
        }

        lines.join("\n")
    }

    fn render_date(delta: &Delta) -> String {
        delta.added.join("\n")
    }

    fn render_autonomy(delta: &Delta) -> String {
        let level = delta.added.first().map(|s| s.as_str()).unwrap_or("default");
        let desc = match level {
            "full" => "All tools are permitted. No restrictions apply.",
            "read_only" => "Autonomy is now READ-ONLY. \
                You may only use read-only tools (file_read, list_dir, search, web_search, etc.). \
                Shell execution, file writes, HTTP calls, and agent delegation are blocked.",
            _ => "Autonomy is set to default. \
                Safe tools are permitted; avoid destructive operations without user confirmation.",
        };
        format!("## Autonomy Level Changed\n\nCurrent autonomy: **{}**. {}", level, desc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Skill;

    fn make_skills(names: &[&str]) -> SkillManager {
        let mut mgr = SkillManager::new();
        for name in names {
            mgr.register(Skill {
                name: name.to_string(),
                description: format!("{} description", name),
                keywords: vec![],
                prompt_body: String::new(),
            });
        }
        mgr
    }

    fn empty_history() -> Vec<ChatMessage> {
        vec![]
    }

    #[test]
    fn empty_history_sends_all_skills() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a", "b"]);
        am.diff_skills(&skills, &empty_history());

        let msg = am.build_message(&skills).unwrap();
        let text = msg.text_content();
        assert!(text.contains("- **a**"));
        assert!(text.contains("- **b**"));
    }

    #[test]
    fn no_change_no_message() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);

        // First: produce a system-reminder and "persist" it into a fake history.
        am.diff_skills(&skills, &empty_history());
        let msg = am.build_message(&skills).unwrap();
        am.clear_pending();

        let history = vec![msg];

        // Second diff with same skills + history containing the prior reminder → no pending
        am.diff_skills(&skills, &history);
        assert!(am.build_message(&skills).is_none());
    }

    #[test]
    fn added_skill_appears_in_delta() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);
        am.diff_skills(&skills, &empty_history());
        let msg = am.build_message(&skills).unwrap();
        am.clear_pending();

        let history = vec![msg];

        let skills2 = make_skills(&["a", "b"]);
        am.diff_skills(&skills2, &history);

        let msg2 = am.build_message(&skills2).unwrap();
        let text = msg2.text_content();
        assert!(text.contains("- **b**"));
        assert!(!text.contains("- **a**")); // a was already announced
    }

    #[test]
    fn removed_skill_appears_in_delta() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a", "b"]);
        am.diff_skills(&skills, &empty_history());
        let msg = am.build_message(&skills).unwrap();
        am.clear_pending();

        let history = vec![msg];

        let skills2 = make_skills(&["a"]);
        am.diff_skills(&skills2, &history);

        let msg2 = am.build_message(&skills2).unwrap();
        let text = msg2.text_content();
        assert!(text.contains("- ~~b~~"));
    }

    #[test]
    fn compaction_naturally_resets() {
        // After compaction, history is empty → next diff sends full listing.
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);
        am.diff_skills(&skills, &empty_history());
        am.clear_pending();

        // Simulate compaction: history is gone.
        let compacted_history: Vec<ChatMessage> = vec![];

        am.diff_skills(&skills, &compacted_history);
        let msg = am.build_message(&skills).unwrap();
        assert!(msg.text_content().contains("- **a**"));
    }

    #[test]
    fn agents_diff_works() {
        let mut am = AttachmentManager::new();
        am.diff_agents(
            &[("coder".into(), "expert programmer".into())],
            &empty_history(),
        );
        let msg = am.build_message(&SkillManager::new()).unwrap();
        am.clear_pending();

        let history = vec![msg];

        am.diff_agents(
            &[
                ("coder".into(), "expert programmer".into()),
                ("researcher".into(), "research specialist".into()),
            ],
            &history,
        );

        let skills = SkillManager::new();
        let msg2 = am.build_message(&skills).unwrap();
        let text = msg2.text_content();
        assert!(text.contains("researcher"));
        assert!(!text.contains("coder")); // already announced
    }

    #[test]
    fn merged_sections_in_single_message() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);
        am.diff_skills(&skills, &empty_history());
        am.diff_agents(&[("coder".into(), "programmer".into())], &empty_history());

        let msg = am.build_message(&skills).unwrap();
        let text = msg.text_content();
        assert!(text.contains("## Skills"));
        assert!(text.contains("## Available Sub-Agents"));
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.ends_with("</system-reminder>"));
    }

    #[test]
    fn rebuild_parses_removed_items() {
        let history = vec![ChatMessage::user_text(
            "<system-reminder>\n## Skills\n- **a**\n- **b**\n</system-reminder>"
        )];
        let announced = AttachmentManager::rebuild_from_history(&history);
        assert!(announced.skills.contains("a"));
        assert!(announced.skills.contains("b"));

        // Now add a removal
        let history2 = vec![
            ChatMessage::user_text(
                "<system-reminder>\n## Skills\n- **a**\n- **b**\n</system-reminder>"
            ),
            ChatMessage::user_text(
                "<system-reminder>\n## Skills\nThe following skills are no longer available:\n- ~~a~~\n\nSkills provide behavioral instructions for specific tasks.\n- **c**\n</system-reminder>"
            ),
        ];
        let announced2 = AttachmentManager::rebuild_from_history(&history2);
        assert!(!announced2.skills.contains("a")); // removed
        assert!(announced2.skills.contains("b"));   // still there
        assert!(announced2.skills.contains("c"));   // added
    }

    #[test]
    fn date_injection_on_empty_history() {
        let mut am = AttachmentManager::new();
        am.diff_date(8, &empty_history());
        let msg = am.build_message(&SkillManager::new()).unwrap();
        let text = msg.text_content();
        assert!(text.contains("Current date:"));
        assert!(text.contains("UTC+8"));
        assert!(text.contains("<system-reminder>"));
    }

    #[test]
    fn date_injection_skips_if_already_in_history() {
        let mut am = AttachmentManager::new();

        // First injection
        am.diff_date(8, &empty_history());
        let msg = am.build_message(&SkillManager::new()).unwrap();
        am.clear_pending();

        // Simulate the date message in history
        let history = vec![msg];
        am.diff_date(8, &history);
        assert!(am.build_message(&SkillManager::new()).is_none());
    }

    #[test]
    fn date_reinjects_after_compaction() {
        let mut am = AttachmentManager::new();

        // First injection
        am.diff_date(8, &empty_history());
        let first_msg = am.build_message(&SkillManager::new()).unwrap();
        am.clear_pending();

        // last_injected_date is set
        assert!(am.last_injected_date.is_some());
        // Verify first message was a proper date injection
        assert!(first_msg.text_content().contains("Current date:"));

        // Simulate compaction: history is empty but last_injected_date persists
        let compacted_history: Vec<ChatMessage> = vec![];
        am.diff_date(8, &compacted_history);

        // Should re-inject because history doesn't contain the date message
        let msg = am.build_message(&SkillManager::new()).unwrap();
        let text = msg.text_content();
        assert!(text.contains("The date has changed"));
    }
}
