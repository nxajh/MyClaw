//! Agent kill tool — terminates a running sub-agent and returns partial result.

use serde_json::json;
use std::sync::Arc;

use crate::agents::{DelegationCoordinator, RunningAgentInfo};
use crate::providers::{Tool, ToolResult};
use crate::str_utils::{UNKNOWN_ID_LISTING_CAP, UNKNOWN_ID_PREVIEW_CHARS};

/// issue #134 (P2): build the "here's what's actually running" listing
/// appended to agent_kill's not-found error — same source and convention as
/// `agent_list` (`running_records()`, unscoped by session, same as that
/// tool already is) so the two never disagree. Newest-first (least elapsed
/// time first), mirroring #133's reviewed convention: a copied or
/// hallucinated id is most likely to reference something spawned recently.
fn format_unknown_agent_listing(mut records: Vec<RunningAgentInfo>) -> String {
    if records.is_empty() {
        return " No sub-agents currently running.".to_string();
    }
    records.sort_by_key(|r| r.elapsed_secs);
    let total = records.len();
    let shown: Vec<String> = records
        .into_iter()
        .take(UNKNOWN_ID_LISTING_CAP)
        .map(|r| {
            let name = crate::str_utils::truncate_line(&r.agent_name, UNKNOWN_ID_PREVIEW_CHARS);
            format!(
                "  {} agent={:?} status={} elapsed_secs={}",
                r.sub_session_id,
                name,
                r.status.as_str(),
                r.elapsed_secs
            )
        })
        .collect();
    let omitted = total.saturating_sub(shown.len());
    let omitted_note = if omitted > 0 {
        format!("\n  ... and {omitted} more")
    } else {
        String::new()
    };
    format!(
        " Currently running sub-agents (use agent_list for the full view):\n{}{}",
        shown.join("\n"),
        omitted_note
    )
}

/// The agent_kill tool — terminates a running sub-agent.
pub struct AgentKillTool {
    delegator: Arc<DelegationCoordinator>,
}

impl AgentKillTool {
    pub fn new(delegator: Arc<DelegationCoordinator>) -> Self {
        Self { delegator }
    }
}

#[async_trait::async_trait]
impl Tool for AgentKillTool {
    fn name(&self) -> &str {
        "agent_kill"
    }

    fn description(&self) -> &str {
        "Terminate a running sub-agent by its session id. Returns the partial result \
         captured before termination. Use agent_list first to find the session id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "sub_session_id": {
                    "type": "string",
                    "description": "The session id of the sub-agent to terminate."
                }
            },
            "required": ["sub_session_id"]
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
        let sub_session_id = args["sub_session_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'sub_session_id' is required"))?;

        let cancelled = self.delegator.cancel(sub_session_id).await;

        if cancelled {
            Ok(ToolResult {
                success: true,
                output: format!(
                    r#"{{"status": "cancelled", "sub_session_id": "{}"}}"#,
                    sub_session_id
                ),
                error: None,
            })
        } else {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Sub-agent '{}' not found or already completed.{}",
                    sub_session_id,
                    format_unknown_agent_listing(self.delegator.running_records())
                )),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::DelegationStatus;

    fn record(id: &str, elapsed_secs: u64) -> RunningAgentInfo {
        RunningAgentInfo {
            sub_session_id: id.to_string(),
            agent_name: "coder".to_string(),
            status: DelegationStatus::Running,
            elapsed_secs,
        }
    }

    /// issue #134 (P2): an unknown id lists what's actually running instead
    /// of a bare not-found.
    #[test]
    fn lists_running_agents() {
        let listing = format_unknown_agent_listing(vec![record("s1", 5)]);
        assert!(listing.contains("s1"));
        assert!(listing.contains("coder"));
    }

    /// No sub-agents at all must say so plainly, not print an empty list.
    #[test]
    fn empty_says_so() {
        let listing = format_unknown_agent_listing(vec![]);
        assert!(listing.contains("No sub-agents currently running"));
    }

    /// #133's reviewed convention: newest (least elapsed) first, and the cap
    /// keeps the newest entries, not the oldest.
    #[test]
    fn newest_first_and_capped() {
        let records: Vec<RunningAgentInfo> = (0..25)
            .map(|i| record(&format!("s{i}"), i as u64))
            .collect();
        let listing = format_unknown_agent_listing(records);
        assert!(listing.contains("and 5 more"));
        assert!(listing.contains("s0"), "newest (elapsed=0) must survive the cap: {listing}");
        assert!(!listing.contains("s24"), "oldest (elapsed=24) must be the one dropped: {listing}");
    }
}
