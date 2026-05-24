//! Startup recovery types and helpers.
//!
//! F37: unified recovery — uses SessionManager.list_all_sessions() + parent_session_id
//! instead of subagent_running_*.json marker files.

/// Info about a sub-agent that was still running when the daemon was killed.
///
/// Populated on startup by scanning sessions with parent_session_id set.
#[derive(Debug, Clone)]
pub struct UnfinishedSubAgent {
    pub agent_name: String,
    pub task_id: String,
    pub task_preview: String,
    pub parent_session_id: String,
    pub sub_session_id: String,
    /// The parent session key (e.g. "telegram:12345").
    pub session_key: String,
    /// The reply_target stored when the parent session last received a message.
    pub reply_target: String,
}

/// Scan for sub-sessions that have a parent_session_id.
///
/// F37: uses backend.list_all_sessions() + parent_session_id filter
/// instead of subagent_running_*.json marker files.
pub fn scan_unfinished_subagents(backend: &dyn crate::storage::SessionBackend) -> Vec<UnfinishedSubAgent> {
    let all = backend.list_all_sessions();
    all.into_iter()
        .filter_map(|s| {
            let parent = backend.load_parent_session_id(&s.id)?;
            Some(UnfinishedSubAgent {
                agent_name: s.display_name.unwrap_or_else(|| s.owner.clone()),
                task_id: String::new(),
                task_preview: String::new(),
                parent_session_id: parent,
                sub_session_id: s.id.clone(),
                session_key: String::new(),
                reply_target: String::new(),
            })
        })
        .collect()
}

/// Legacy: scan marker files. Kept for backward compatibility during migration.
pub fn scan_unfinished_subagents_from_markers(sessions_root: &std::path::Path) -> Vec<UnfinishedSubAgent> {
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

/// Remove all stale `subagent_running_*.json` marker files.
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
