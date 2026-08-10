//! Startup recovery types and helpers.
//!
//! Extracted from `daemon.rs` (the Composition Root) so that Application-layer
//! types such as `Orchestrator` can reference `UnfinishedSubAgent` without
//! importing the Composition Root.
//!
//! F37 + H50: the old `subagent_running_*.json` marker mechanism is gone.
//! `scan_unfinished_subagents` now reads `SessionManager.list_all_sessions`
//! and reconstructs `UnfinishedSubAgent` records from session metadata
//! (`parent_session_id`, `agent_name`, parent's `owner` / `last_message`).
//! This works because B15 made sub-sessions top-level peers of regular
//! sessions, distinguished only by `meta.parent_session_id`.

use crate::agents::session::SessionManager;

/// Info about a sub-agent that was still running when the daemon was killed.
///
/// Reconstructed from session metadata on startup — no on-disk marker file
/// is required. `task_id` reuses the sub-session id (which is the durable
/// identifier post-restart); `task_preview` is derived from the first user
/// message in the sub-session history.
#[derive(Debug, Clone)]
pub struct UnfinishedSubAgent {
    pub agent_name: String,
    pub task_id: String,
    pub task_preview: String,
    pub parent_session_id: String,
    pub sub_session_id: String,
    /// The parent session key (e.g. "telegram:default:12345") — routing
    /// context only. Startup recovery emits `DelegationEvent`s keyed by the
    /// parent SESSION ID (`parent_session_id`), not this routing key.
    pub session_key: String,
    /// The reply_target stored on the parent session's last_message.
    pub reply_target: String,
}

/// Walk `SessionManager` for sub-sessions (those with a `parent_session_id`)
/// whose history ends mid-turn. Each match is reported as one
/// `UnfinishedSubAgent`.
///
/// "Mid-turn" matches the same shape `agent_impl::recover_incomplete_turn`
/// looks for: the trailing user / assistant-tool_calls / dangling tool-result
/// block. We don't re-encode that logic here — we just check the cheap
/// approximation that `incomplete_turn` is true OR the last message is a
/// `user` / `tool` role (the orchestrator's per-session recovery does the
/// authoritative check before re-executing).
pub fn scan_unfinished_subagents(session_manager: &SessionManager) -> Vec<UnfinishedSubAgent> {
    let mut unfinished = Vec::new();
    for info in session_manager.list_all_sessions() {
        let session = match session_manager.get_by_id(&info.id) {
            Some(s) => s,
            None => continue,
        };
        let parent_id = match &session.parent_session_id {
            Some(p) if !p.is_empty() => p.clone(),
            _ => continue,
        };
        let needs_recovery = session.incomplete_turn
            || session
                .history
                .last()
                .is_some_and(|m| m.role == "user" || m.role == "tool");
        if !needs_recovery {
            continue;
        }

        let parent_session = session_manager.get_by_id(&parent_id);
        let session_key = parent_session
            .as_ref()
            .map(|p| p.owner.clone())
            .unwrap_or_default();
        let reply_target = parent_session
            .as_ref()
            .and_then(|p| p.reply_target().map(|s| s.to_string()))
            .unwrap_or_default();

        let task_preview = session
            .history
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.text_content())
            .map(|s| s.chars().take(200).collect::<String>())
            .unwrap_or_default();

        unfinished.push(UnfinishedSubAgent {
            agent_name: session.agent_name.clone(),
            // P1-1: prefer the persisted task FQID (matches the parent's
            // suspension `pending` entry) over the opaque sub-session id.
            // Legacy sub-sessions without the field fall back to the hex id.
            task_id: session
                .sub_agent_task_id
                .clone()
                .unwrap_or_else(|| session.id.clone()),
            task_preview,
            parent_session_id: parent_id,
            sub_session_id: session.id.clone(),
            session_key,
            reply_target,
        });
    }
    unfinished
}
