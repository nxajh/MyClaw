//! Attachment manager — 增量注入 skills/agents/MCP 列表。
//!
//! 每轮 turn 最多生成一条 `<system-reminder>` user 消息，不存 history。
//! Compaction / restart / reload 后全量重建。
//!
//! 设计参照 Claude Code 的统一增量 delta 模式：
//! - 三类信息（skills, agents, MCP）分别 diff，合并为一条消息
//! - `announced_*` sets 追踪已通知 LLM 的内容
//! - `first_turn` 控制首次全量 vs 后续增量

use std::collections::{HashMap, HashSet};

use crate::providers::ChatMessage;
use super::skills::SkillManager;

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AttachmentKind {
    SkillListing,
    AgentListing,
    McpInstructions,
}

/// 单类增量。
///
/// `added` / `removed` 的每个元素是格式化好的文本行。
#[derive(Default)]
struct Delta {
    added: Vec<String>,
    removed: Vec<String>,
}

// ── AttachmentManager ─────────────────────────────────────────────────────

/// 附件管理器 — 每个 AgentLoop 持有一个。
///
/// 追踪已通知 LLM 的 skills / agents / MCP 列表，每 turn 计算差异，
/// 合并渲染为一条 `<system-reminder>` 消息。
pub struct AttachmentManager {
    /// 已通知 LLM 的 skill 名称集合
    announced_skills: HashSet<String>,
    /// 已通知 LLM 的 agent 名称集合
    announced_agents: HashSet<String>,
    /// 已通知 LLM 的 MCP server 名称集合
    announced_mcp: HashSet<String>,
    /// 本轮待发送增量
    pending: HashMap<AttachmentKind, Delta>,
    /// 首轮标记 — 首次全量发送
    first_turn: bool,
}

impl Default for AttachmentManager {
    fn default() -> Self {
        Self {
            announced_skills: HashSet::new(),
            announced_agents: HashSet::new(),
            announced_mcp: HashSet::new(),
            pending: HashMap::new(),
            first_turn: true,
        }
    }
}

impl AttachmentManager {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Diff ────────────────────────────────────────────────────────────

    /// 与当前 SkillManager 做 diff，生成 skill listing delta。
    pub fn diff_skills(&mut self, skills: &SkillManager) {
        let current: HashSet<String> =
            skills.skills_iter().map(|(n, _)| n.to_string()).collect();

        if self.first_turn {
            self.pending.insert(
                AttachmentKind::SkillListing,
                Delta {
                    added: current.iter().cloned().collect(),
                    removed: vec![],
                },
            );
        } else {
            let added: Vec<String> = current.difference(&self.announced_skills).cloned().collect();
            let removed: Vec<String> = self.announced_skills.difference(&current).cloned().collect();
            if !added.is_empty() || !removed.is_empty() {
                self.pending
                    .insert(AttachmentKind::SkillListing, Delta { added, removed });
            }
        }

        self.announced_skills = current;
    }

    /// 与当前 agent 列表做 diff。
    ///
    /// `agents`: `Vec<(name, description)>`
    pub fn diff_agents(&mut self, agents: &[(String, String)]) {
        let current: HashSet<String> = agents.iter().map(|(n, _)| n.clone()).collect();

        if self.first_turn {
            self.pending.insert(
                AttachmentKind::AgentListing,
                Delta {
                    added: agents
                        .iter()
                        .map(|(name, desc)| {
                            if desc.is_empty() {
                                name.clone()
                            } else {
                                format!("{}: {}", name, desc)
                            }
                        })
                        .collect(),
                    removed: vec![],
                },
            );
        } else {
            let added_names: Vec<String> =
                current.difference(&self.announced_agents).cloned().collect();
            let removed: Vec<String> =
                self.announced_agents.difference(&current).cloned().collect();

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
                self.pending
                    .insert(AttachmentKind::AgentListing, Delta { added, removed });
            }
        }

        self.announced_agents = current;
    }

    /// 与当前 MCP server instructions 做 diff。
    ///
    /// `servers`: `Vec<(server_name, instructions)>`
    pub fn diff_mcp(&mut self, servers: &[(String, String)]) {
        let current: HashSet<String> = servers.iter().map(|(n, _)| n.clone()).collect();

        if self.first_turn {
            self.pending.insert(
                AttachmentKind::McpInstructions,
                Delta {
                    added: servers
                        .iter()
                        .filter(|(_, inst)| !inst.is_empty())
                        .map(|(name, inst)| format!("### {}\n{}", name, inst))
                        .collect(),
                    removed: vec![],
                },
            );
        } else {
            let added: Vec<String> = servers
                .iter()
                .filter(|(name, _)| !self.announced_mcp.contains(name))
                .filter(|(_, inst)| !inst.is_empty())
                .map(|(name, inst)| format!("### {}\n{}", name, inst))
                .collect();
            let removed: Vec<String> =
                self.announced_mcp.difference(&current).cloned().collect();
            if !added.is_empty() || !removed.is_empty() {
                self.pending
                    .insert(AttachmentKind::McpInstructions, Delta { added, removed });
            }
        }

        self.announced_mcp = current;
    }

    // ── Render ──────────────────────────────────────────────────────────

    /// 将所有 pending delta 合并为一条 ChatMessage。
    ///
    /// 无 delta 时返回 `None`。
    /// 返回的消息**不**存入 session history。
    pub fn build_message(&self, skills: &SkillManager) -> Option<ChatMessage> {
        let mut sections = Vec::new();

        if let Some(delta) = self.pending.get(&AttachmentKind::SkillListing) {
            sections.push(Self::render_skills(delta, skills));
        }
        if let Some(delta) = self.pending.get(&AttachmentKind::AgentListing) {
            sections.push(Self::render_agents(delta));
        }
        if let Some(delta) = self.pending.get(&AttachmentKind::McpInstructions) {
            sections.push(Self::render_mcp(delta));
        }

        if sections.is_empty() {
            return None;
        }

        Some(ChatMessage::user_text(format!(
            "<system-reminder>\n{}\n</system-reminder>",
            sections.join("\n\n")
        )))
    }

    /// 清空 pending（每 turn 结算后调用）。
    pub fn clear_pending(&mut self) {
        self.pending.clear();
        self.first_turn = false;
    }

    /// 是否尚未执行过首次 diff。
    pub fn is_fresh(&self) -> bool {
        self.first_turn
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    /// Compaction 后全量重建 — 下一 turn 重新发送完整列表。
    pub fn on_compaction(&mut self) {
        self.announced_skills.clear();
        self.announced_agents.clear();
        self.announced_mcp.clear();
        self.first_turn = true;
    }

    /// /reload 后全量重建。
    pub fn reset_all(&mut self) {
        self.on_compaction();
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
                r#"Use `delegate_task(agent="name", task="...")` to delegate."#.to_string(),
            );
            for entry in &delta.added {
                lines.push(format!("- **{}**", entry));
            }
        }

        lines.join("\n")
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

    #[test]
    fn first_turn_sends_all_skills() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a", "b"]);
        am.diff_skills(&skills);

        let msg = am.build_message(&skills).unwrap();
        let text = msg.text_content();
        assert!(text.contains("- **a**"));
        assert!(text.contains("- **b**"));
    }

    #[test]
    fn no_change_no_message() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);
        am.diff_skills(&skills);
        am.clear_pending();

        // Second diff with same skills → no pending
        am.diff_skills(&skills);
        assert!(am.build_message(&skills).is_none());
    }

    #[test]
    fn added_skill_appears_in_delta() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);
        am.diff_skills(&skills);
        am.clear_pending();

        let skills2 = make_skills(&["a", "b"]);
        am.diff_skills(&skills2);

        let msg = am.build_message(&skills2).unwrap();
        let text = msg.text_content();
        assert!(text.contains("- **b**"));
        assert!(!text.contains("- **a**")); // a was already announced
    }

    #[test]
    fn removed_skill_appears_in_delta() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a", "b"]);
        am.diff_skills(&skills);
        am.clear_pending();

        let skills2 = make_skills(&["a"]);
        am.diff_skills(&skills2);

        let msg = am.build_message(&skills2).unwrap();
        let text = msg.text_content();
        assert!(text.contains("- ~~b~~"));
    }

    #[test]
    fn on_compaction_resets_to_full() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);
        am.diff_skills(&skills);
        am.clear_pending();

        am.on_compaction();

        // Should behave like first turn
        assert!(am.is_fresh());
        am.diff_skills(&skills);
        let msg = am.build_message(&skills).unwrap();
        assert!(msg.text_content().contains("- **a**"));
    }

    #[test]
    fn agents_diff_works() {
        let mut am = AttachmentManager::new();
        am.diff_agents(&[("coder".into(), "expert programmer".into())]);
        am.clear_pending();

        am.diff_agents(&[
            ("coder".into(), "expert programmer".into()),
            ("researcher".into(), "research specialist".into()),
        ]);

        let skills = SkillManager::new();
        let msg = am.build_message(&skills).unwrap();
        let text = msg.text_content();
        assert!(text.contains("researcher"));
        assert!(!text.contains("coder")); // already announced
    }

    #[test]
    fn merged_sections_in_single_message() {
        let mut am = AttachmentManager::new();
        let skills = make_skills(&["a"]);
        am.diff_skills(&skills);
        am.diff_agents(&[("coder".into(), "programmer".into())]);

        let msg = am.build_message(&skills).unwrap();
        let text = msg.text_content();
        assert!(text.contains("## Skills"));
        assert!(text.contains("## Available Sub-Agents"));
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.ends_with("</system-reminder>"));
    }
}
