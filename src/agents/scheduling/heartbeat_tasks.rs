//! Heartbeat structured task parser and scheduler.
//!
//! Parses HEARTBEAT.md with structured task format:
//!
//! ```markdown
//! # HEARTBEAT
//!
//! Optional general instructions (added as context).
//!
//! ## Tasks
//!
//! - [check-email] Check email for important messages
//! - [check-calendar] Check calendar for upcoming events
//! - [review-pr] Check GitHub PR status
//! - [paused-task:paused] This task will not execute
//! ```
//!
//! Format: `- [name] description` or `- [name:paused] description`
//! - `name`: task identifier (alphanumeric, hyphens)
//! - `paused`: optional keyword to skip this task
//! - Frequency is controlled by the heartbeat `every` config, not per-task
//! - Backward compatible: `- plain text` generates a hash-based name
//!
//! Task state is persisted to `HEARTBEAT_STATE.json` in workspace dir.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A parsed heartbeat task.
#[derive(Debug, Clone)]
pub struct HeartbeatTask {
    pub name: String,
    pub interval: Duration,
    pub description: String,
    pub is_paused: bool,
}

/// Persisted task run state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatState {
    /// Map of task name -> last run timestamp (Unix millis).
    pub last_run: HashMap<String, u64>,
}

impl HeartbeatState {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Parse a duration string like "30m", "1h", "2h", "1d" into `Duration`.
fn parse_interval_str(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, unit) = s.split_at(s.len() - 1);
    let n: u64 = num_part.parse().ok()?;
    match unit {
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        "h" => Some(Duration::from_secs(n * 3600)),
        "d" => Some(Duration::from_secs(n * 86400)),
        _ => None,
    }
}

/// Generate a short deterministic name from plain text (for backward compat).
fn text_to_name(text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:08x}", hasher.finish())
}

/// Parse HEARTBEAT.md content into (general_context, tasks).
///
/// The general context is everything before the first `## Tasks` section.
/// Tasks are parsed from `- [name] description` lines.
/// The optional `:paused` suffix marks a task as paused: `- [name:paused] description`
///
/// Backward compat: if no `## Tasks` section exists but there are `- ` lines,
/// they are treated as tasks (plain text format with auto-generated name).
pub fn parse_heartbeat(content: &str) -> (String, Vec<HeartbeatTask>) {
    let mut context_lines: Vec<&str> = Vec::new();
    let mut tasks: Vec<HeartbeatTask> = Vec::new();
    let mut in_tasks_section = false;
    let mut has_tasks_section = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect "## Tasks" section header
        if trimmed.eq_ignore_ascii_case("## tasks")
            || trimmed.eq_ignore_ascii_case("## tasks:")
        {
            in_tasks_section = true;
            has_tasks_section = true;
            continue;
        }

        // Detect another ## section after tasks → stop (new section)
        if in_tasks_section && trimmed.starts_with("## ") && !trimmed.eq_ignore_ascii_case("## tasks") {
            in_tasks_section = false;
            // Don't continue — fall through to context collection
        }

        if in_tasks_section && trimmed.starts_with("- ") {
            let item = &trimmed[2..]; // strip "- "
            if item.is_empty() {
                continue;
            }
            if let Some(task) = parse_task_line(item) {
                tasks.push(task);
            }
            continue;
        }

        if !in_tasks_section {
            context_lines.push(line);
        }
    }

    // Backward compat: no "## Tasks" section → collect `- ` lines from context as tasks
    if !has_tasks_section {
        let mut remaining_context: Vec<&str> = Vec::new();
        for line in context_lines {
            let trimmed = line.trim();
            if trimmed.starts_with("- ") && trimmed.len() > 2 {
                if let Some(task) = parse_task_line(&trimmed[2..]) {
                    tasks.push(task);
                    continue;
                }
            }
            remaining_context.push(line);
        }
        context_lines = remaining_context;
    }

    // Strip markdown headers (# ...) and leading/trailing whitespace from context
    let context = context_lines
        .into_iter()
        .filter(|line| !line.trim().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    (context, tasks)
}

/// Parse a single task line: `[name] description` or `[name:paused] description` or plain text.
fn parse_task_line(text: &str) -> Option<HeartbeatTask> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    // Try structured format: [name:interval] description
    if text.starts_with('[') {
        if let Some(bracket_end) = text.find(']') {
            let meta = &text[1..bracket_end];
            let description = text[bracket_end + 1..].trim().to_string();

            // Check if it's paused
            let is_paused = meta.contains("paused") || meta.contains("pause");

            // Parse name:interval
            let (name, interval) = if let Some(colon_pos) = meta.find(':') {
                let name = meta[..colon_pos].trim().to_string();
                let interval_str = meta[colon_pos + 1..].trim();
                if interval_str == "paused" || interval_str == "pause" {
                    // e.g. `[task-name:paused]`
                    (name, Duration::from_secs(30 * 60)) // default, won't run anyway
                } else {
                    (
                        name,
                        parse_interval_str(interval_str)
                            .unwrap_or(Duration::from_secs(30 * 60)),
                    )
                }
            } else {
                // Just `[name]` with no interval
                (meta.trim().to_string(), Duration::from_secs(30 * 60))
            };

            if name.is_empty() {
                return None;
            }

            return Some(HeartbeatTask {
                name,
                interval,
                description,
                is_paused,
            });
        }
    }

    // Backward compat: plain text → auto-generate name, 30m default
    Some(HeartbeatTask {
        name: text_to_name(text),
        interval: Duration::from_secs(30 * 60),
        description: text.to_string(),
        is_paused: false,
    })
}

/// Filter tasks to only those that are not paused.
///
/// With config-controlled heartbeat interval (the `every` field), all non-paused
/// tasks run on every heartbeat tick. Per-task intervals are no longer used —
/// frequency is controlled at the config level, matching OpenClaw's approach.
pub fn due_tasks<'a>(
    tasks: &'a [HeartbeatTask],
    _state: &HeartbeatState,
) -> Vec<&'a HeartbeatTask> {
    tasks
        .iter()
        .filter(|task| !task.is_paused)
        .collect()
}

/// Build a prompt for the given due tasks.
pub fn build_heartbeat_prompt(context: &str, due: &[&HeartbeatTask]) -> String {
    let mut prompt = String::new();

    if !context.is_empty() {
        prompt.push_str(context);
        prompt.push_str("\n\n");
    }

    prompt.push_str("Execute the following heartbeat tasks:\n\n");

    for (i, task) in due.iter().enumerate() {
        prompt.push_str(&format!("{}. [{}] {}\n", i + 1, task.name, task.description));
    }

    prompt.push_str(
        "\nIf all tasks have nothing to report, reply ONLY with: HEARTBEAT_OK\n\
         If any task produced results, report them normally. Do NOT include HEARTBEAT_OK in that case.\n\
         Do not infer or repeat old tasks from prior chats.",
    );

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_structured_tasks() {
        let content = "# HEARTBEAT\n\nGeneral instructions here.\n\n## Tasks\n\n- [check-email] 检查邮件\n- [check-cal] 检查日历\n- [paused:paused] 不执行\n";
        let (ctx, tasks) = parse_heartbeat(content);
        assert_eq!(ctx, "General instructions here.");
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].name, "check-email");
        assert!(!tasks[0].is_paused);
        assert_eq!(tasks[1].name, "check-cal");
        assert!(tasks[2].is_paused);
    }

    #[test]
    fn parse_backward_compat() {
        let content = "- Check email\n- Review PRs\n";
        let (ctx, tasks) = parse_heartbeat(content);
        assert!(ctx.is_empty());
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].interval, Duration::from_secs(1800));
        assert!(!tasks[0].is_paused);
    }

    #[test]
    fn parse_no_tasks_section() {
        let content = "# HEARTBEAT\n\nJust some instructions.\n";
        let (ctx, tasks) = parse_heartbeat(content);
        assert_eq!(ctx, "Just some instructions.");
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_interval_str_variants() {
        assert_eq!(parse_interval_str("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_interval_str("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_interval_str("2d"), Some(Duration::from_secs(172800)));
        assert_eq!(parse_interval_str("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_interval_str("abc"), None);
    }

    #[test]
    fn due_tasks_skips_paused_only() {
        let tasks = vec![
            HeartbeatTask {
                name: "a".into(),
                interval: Duration::from_secs(1800),
                description: "task a".into(),
                is_paused: false,
            },
            HeartbeatTask {
                name: "b".into(),
                interval: Duration::from_secs(3600),
                description: "task b".into(),
                is_paused: true,
            },
        ];
        let state = HeartbeatState::default();
        let due = due_tasks(&tasks, &state);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "a");
    }

    #[test]
    fn due_tasks_skips_paused() {
        let tasks = vec![HeartbeatTask {
            name: "x".into(),
            interval: Duration::from_secs(60),
            description: "task x".into(),
            is_paused: true,
        }];
        let state = HeartbeatState::default();
        assert!(due_tasks(&tasks, &state).is_empty());
    }

    #[test]
    fn build_prompt_includes_context_and_tasks() {
        let tasks = vec![
            HeartbeatTask {
                name: "email".into(),
                interval: Duration::from_secs(1800),
                description: "Check email".into(),
                is_paused: false,
            },
        ];
        let due: Vec<&HeartbeatTask> = tasks.iter().collect();
        let prompt = build_heartbeat_prompt("Be concise.", &due);
        assert!(prompt.contains("Be concise."));
        assert!(prompt.contains("[email] Check email"));
        assert!(prompt.contains("HEARTBEAT_OK"));
    }
}
