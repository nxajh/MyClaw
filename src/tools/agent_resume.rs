//! Agent resume tool — revives a timed-out sub-agent with a fresh budget.
//!
//! Layer 3 of the sub-agent timeout fix: a timeout kills the *task*, not the
//! *work*. The sub-session history survives on disk, so the parent can
//! resume the delegation from where it was interrupted instead of
//! re-delegating from scratch (losing all context).

use serde_json::json;
use std::sync::Arc;

use crate::api::agent_lifecycle::AgentLifecycle;
use crate::providers::{Tool, ToolResult};
use crate::api::agent_lifecycle::ResumableAgent;
use crate::str_utils::UNKNOWN_ID_LISTING_CAP;

/// issue #134 (P2): build the "here's what can actually be resumed" listing
/// appended to agent_resume's not-found/not-resumable errors. Only
/// `timed_out` checkpoints are ever accepted by `resume_timed_out` — a
/// listing of *running* sub-agents here would mislead the model into trying
/// to resume something that's still going (that's `agent_kill`'s job).
/// Newest-first (`started_at` descending), matching #133's reviewed
/// convention: a copied or hallucinated id is most likely to reference
/// something that timed out recently.
fn format_unknown_resumable_listing(mut checkpoints: Vec<ResumableAgent>) -> String {
    if checkpoints.is_empty() {
        return " No timed-out sub-agents are currently resumable.".to_string();
    }
    checkpoints.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let total = checkpoints.len();
    let shown: Vec<String> = checkpoints
        .into_iter()
        .take(UNKNOWN_ID_LISTING_CAP)
        .map(|cp| {
            format!(
                "  {} agent={:?} started_at={}",
                cp.sub_session_id,
                cp.agent_name,
                cp.started_at_rfc3339
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
        " Resumable (timed-out) sub-agents:\n{}{}",
        shown.join("\n"),
        omitted_note
    )
}

pub struct AgentResumeTool {
    delegator: Arc<dyn AgentLifecycle>,
}

impl AgentResumeTool {
    pub fn new<D: AgentLifecycle + 'static>(delegator: Arc<D>) -> Self {
        Self { delegator }
    }
}

#[async_trait::async_trait]
impl Tool for AgentResumeTool {
    fn name(&self) -> &str {
        "agent_resume"
    }

    fn description(&self) -> &str {
        "Resume a timed-out sub-agent from where it was interrupted, with a fresh wall-clock budget. \
         The sub-agent's session history and partial work are preserved — it continues the same task \
         instead of restarting from scratch. Only delegations that ended in `timed_out` can be resumed. \
         The tool returns immediately; the resumed sub-agent's result arrives later as a delegation \
         completion notice (the current turn suspends awaiting it)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "sub_session_id": {
                    "type": "string",
                    "description": "The session id of the timed-out sub-agent (from the timeout notice)."
                },
                "extra_secs": {
                    "type": "integer",
                    "description": "Optional fresh budget in seconds. Defaults to double the delegation's original timeout (minimum 600s), since resume is only reachable after that budget already ran out; clamped to the global 1800s ceiling."
                }
            },
            "required": ["sub_session_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        4_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let sub_session_id = args["sub_session_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'sub_session_id' is required"))?;
        let extra_secs = args["extra_secs"].as_u64();

        match self.delegator.resume_timed_out(sub_session_id, extra_secs) {
            Ok(resumed_id) => Ok(ToolResult {
                success: true,
                output: format!(
                    r#"{{"status": "resumed", "sub_session_id": "{}"}}"#,
                    resumed_id
                ),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "resume failed: {:#}.{}",
                    e,
                    format_unknown_resumable_listing(self.delegator.timed_out_resumables())
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(id: &str, started_at: chrono::DateTime<chrono::Utc>) -> ResumableAgent {
        ResumableAgent {
            sub_session_id: id.to_string(),
            agent_name: "coder".to_string(),
            started_at_rfc3339: started_at.to_rfc3339(),
        }
    }

    /// issue #134 (P2): an unknown/non-resumable id lists what's actually
    /// resumable instead of a bare failure.
    #[test]
    fn lists_resumable_checkpoints() {
        let listing = format_unknown_resumable_listing(vec![checkpoint("s1", chrono::Utc::now())]);
        assert!(listing.contains("s1"));
        assert!(listing.contains("coder"));
    }

    /// No resumable checkpoints at all must say so plainly.
    #[test]
    fn empty_says_so() {
        let listing = format_unknown_resumable_listing(vec![]);
        assert!(listing.contains("No timed-out sub-agents are currently resumable"));
    }

    /// #133's reviewed convention: newest (`started_at`) first, and the cap
    /// keeps the newest entries.
    #[test]
    fn newest_first_and_capped() {
        let base = chrono::Utc::now();
        let checkpoints: Vec<ResumableAgent> = (0..25)
            .map(|i| checkpoint(&format!("s{i}"), base - chrono::Duration::seconds(i)))
            .collect();
        let listing = format_unknown_resumable_listing(checkpoints);
        assert!(listing.contains("and 5 more"));
        assert!(listing.contains("s0"), "newest must survive the cap: {listing}");
        assert!(!listing.contains("s24"), "oldest must be the one dropped: {listing}");
    }
}
