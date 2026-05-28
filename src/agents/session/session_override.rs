//! Per-session runtime overrides applied by slash commands.

use crate::providers::capability_chat::ChatMessage;

// ── SessionOverride ───────────────────────────────────────────────────────────

/// Per-session runtime overrides applied by slash commands.
///
/// Each field is `None` = use global config default.
/// Persisted in `meta.json` so overrides survive restarts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionOverride {
    /// Force a specific model ID instead of the routing default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override thinking/reasoning mode. None = use model's `reasoning` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// Thinking effort level when thinking is enabled ("low"/"medium"/"high").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Override permission mode for this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::config::agent::PermissionMode>,
    /// Override run mode for this session (Interactive vs Background).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_mode: Option<crate::config::agent::RunMode>,
    /// Override max tool calls per turn (0 = unlimited).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<usize>,
    /// Override compaction trigger threshold (0.0..1.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<f64>,
    /// Override number of recent work units to retain during compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retain_work_units: Option<usize>,
    /// Replace the assembled system prompt with this string. Used by
    /// `DelegationCoordinator` so sub-agent turns can run through
    /// `SessionContext::process_turn` while still seeing their
    /// minimal AGENT.md identity prompt instead of the full builder
    /// output. Not persisted — set per-context-lifecycle.
    #[serde(skip)]
    pub system_prompt_override: Option<String>,
}

impl SessionOverride {
    /// Returns true if all fields are None (no active overrides).
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.thinking.is_none()
            && self.effort.is_none()
            && self.permission_mode.is_none()
            && self.run_mode.is_none()
            && self.max_tool_calls.is_none()
            && self.compact_threshold.is_none()
            && self.retain_work_units.is_none()
            && self.system_prompt_override.is_none()
    }

    /// Resolve the optional thinking/effort fields into a `ThinkingConfig`.
    /// `None` = use the model's own reasoning config (no override).
    pub fn to_thinking_config(&self) -> Option<crate::providers::ThinkingConfig> {
        match self.thinking {
            Some(true) => Some(crate::providers::ThinkingConfig {
                enabled: true,
                effort: self.effort.clone(),
            }),
            Some(false) => Some(crate::providers::ThinkingConfig {
                enabled: false,
                effort: None,
            }),
            None => None,
        }
    }
}

/// Remove orphan tool results (tool messages whose tool_call_id has no matching
/// assistant tool_call in the history). Also removes trailing assistant messages
/// with tool_calls that have no corresponding tool results (incomplete round,
/// e.g. process crashed during tool execution).
pub fn sanitize_history(history: &mut Vec<ChatMessage>) {
    // Step 1: Collect all tool_call_ids declared by assistant messages.
    let mut known_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in history.iter() {
        if msg.role == "assistant" {
            if let Some(ref tcs) = msg.tool_calls {
                for tc in tcs {
                    known_tool_ids.insert(tc.id.clone());
                }
            }
        }
    }

    // Step 2: Remove orphan tool results (no matching assistant tool_call).
    let before = history.len();
    history.retain(|msg| {
        if msg.role == "tool" {
            if let Some(ref tc_id) = msg.tool_call_id {
                return known_tool_ids.contains(tc_id);
            }
            return false;
        }
        true
    });

    let removed = before - history.len();
    if removed > 0 {
        tracing::warn!(removed, "sanitized orphan tool results from history");
    }

    // Step 3: Collect tool_call_ids that actually have a tool result.
    let fulfilled_ids: std::collections::HashSet<String> = history
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Step 4: Trim trailing assistant messages whose tool_calls are all unfulfilled.
    // These are left behind when the process crashes during tool execution.
    let mut trimmed = 0;
    while let Some(last) = history.last() {
        if last.role == "assistant" {
            if let Some(ref tcs) = last.tool_calls {
                if !tcs.is_empty() && tcs.iter().all(|tc| !fulfilled_ids.contains(&tc.id)) {
                    history.pop();
                    trimmed += 1;
                    continue;
                }
            }
        }
        break;
    }
    if trimmed > 0 {
        tracing::warn!(trimmed, "sanitized trailing assistant messages with unfulfilled tool_calls");
    }
}

/// Same as `sanitize_history` but keeps IDs paired with their messages throughout,
/// so the returned IDs correctly correspond to the surviving messages.
///
/// `sanitize_history` uses `retain()` which may remove messages from arbitrary
/// positions (not just the tail), so slicing the IDs array after the fact gives
/// wrong IDs. This variant avoids that by filtering both vecs together.
pub(super) fn sanitize_paired(pairs: Vec<(i64, ChatMessage)>) -> Vec<(i64, ChatMessage)> {
    // Step 1: Remove orphan tool results.
    let known_tool_ids: std::collections::HashSet<String> = pairs
        .iter()
        .filter(|(_, m)| m.role == "assistant")
        .flat_map(|(_, m)| m.tool_calls.iter().flatten().map(|tc| tc.id.clone()))
        .collect();

    let before = pairs.len();
    let mut result: Vec<_> = pairs
        .into_iter()
        .filter(|(_, msg)| {
            if msg.role == "tool" {
                return msg
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| known_tool_ids.contains(id));
            }
            true
        })
        .collect();

    // Step 2: Collect fulfilled tool_call_ids (those with actual tool results).
    let fulfilled_ids: std::collections::HashSet<String> = result
        .iter()
        .filter(|(_, m)| m.role == "tool")
        .filter_map(|(_, m)| m.tool_call_id.clone())
        .collect();

    // Step 3: Trim trailing assistant messages with all-unfulfilled tool_calls.
    let mut trimmed = 0;
    while let Some((_, last)) = result.last() {
        if last.role == "assistant" {
            if let Some(ref tcs) = last.tool_calls {
                if !tcs.is_empty() && tcs.iter().all(|tc| !fulfilled_ids.contains(&tc.id)) {
                    result.pop();
                    trimmed += 1;
                    continue;
                }
            }
        }
        break;
    }
    if trimmed > 0 {
        tracing::warn!(trimmed, "sanitize_paired: trimmed trailing assistant messages with unfulfilled tool_calls");
    }

    let removed = before - result.len();
    if removed > 0 {
        tracing::warn!(removed, "sanitized orphan tool results from history");
    }
    result
}
