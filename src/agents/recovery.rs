//! Startup recovery types and helpers.
//!
//! Extracted from `daemon.rs` (the Composition Root) so that Application-layer
//! types such as `Orchestrator` can reference `UnfinishedSubAgent` without
//! importing the Composition Root — which would violate the DDD layering rule
//! that Application must not depend on Infrastructure / Composition Root.

/// Info about a sub-agent that was still running when the daemon was killed.
///
/// Populated on startup by scanning the session directory for
/// `subagent_running_*.json` marker files left by a previous process.
#[derive(Debug, Clone)]
pub struct UnfinishedSubAgent {
    pub agent_name: String,
    pub task_id: String,
    pub task_preview: String,
    pub parent_session_id: String,
    pub sub_session_id: String,
    /// The parent session key (e.g. "telegram:12345") used to look up the
    /// main agent's session and emit a DelegationEvent when recovery completes.
    pub session_key: String,
    /// The reply_target stored when the parent session last received a message.
    pub reply_target: String,
}

/// Scan `sessions_root` for `subagent_running_*.json` marker files left behind
/// by a previous daemon that was killed while sub-agents were executing.
pub fn scan_unfinished_subagents(sessions_root: &std::path::Path) -> Vec<UnfinishedSubAgent> {
    let mut unfinished = Vec::new();
    let entries = match std::fs::read_dir(sessions_root) {
        Ok(e) => e,
        Err(_) => return unfinished,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("subagent_running_") && name.ends_with(".json") {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                    unfinished.push(UnfinishedSubAgent {
                        agent_name: state["agent_name"].as_str().unwrap_or("unknown").to_string(),
                        task_id: state["task_id"].as_str().unwrap_or("unknown").to_string(),
                        task_preview: state["task_preview"].as_str().unwrap_or("").to_string(),
                        parent_session_id: state["parent_session_id"].as_str().unwrap_or("").to_string(),
                        sub_session_id: state["sub_session_id"].as_str().unwrap_or("").to_string(),
                        session_key: state["session_key"].as_str().unwrap_or("").to_string(),
                        reply_target: state["reply_target"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
        }
    }
    unfinished
}

/// Remove all stale `subagent_running_*.json` marker files so they do not
/// accumulate across restarts.  Called once after the Orchestrator has been
/// informed about unfinished sub-agents.
pub fn cleanup_stale_subagent_markers(sessions_root: &std::path::Path) {
    let entries = match std::fs::read_dir(sessions_root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("subagent_running_") && name.ends_with(".json") {
            let _ = std::fs::remove_file(entry.path());
            tracing::info!(file = %name, "cleaned up stale sub-agent marker");
        }
    }
}
