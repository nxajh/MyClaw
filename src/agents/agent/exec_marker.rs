//! Session persistence helpers and the exec-marker crash-loop guard
//! (extracted verbatim from the former `agent.rs`).

use crate::agents::session::Session;
use crate::providers::capability_chat::ChatMessageUsage;

use super::stream_collect::CollectedResponse;

pub(super) fn persist_last(session: &mut Session) {
    let hook = match session.persist.clone() {
        Some(h) => h,
        None => return,
    };
    let msg = match session.history.last().cloned() {
        Some(m) => m,
        None => return,
    };
    if let Some(id) = hook.persist_message(&session.id, &msg) {
        if let Some(slot) = session.message_ids.last_mut() {
            *slot = id;
        }
    }
}

// ── Exec marker ─────────────────────────────────────────────────────────────
//
// When a tool call kills the daemon (e.g. `shell("myclaw update")` triggers
// `systemctl restart`), `execute()` never returns and the tool result is
// never persisted. On restart, recovery sees an orphan tool_call and blindly
// re-executes it — killing the daemon again in an infinite loop.
//
// The exec-marker breaks this cycle: before executing any tool we write a
// tiny file `sessions/<id>/.exec_marker` containing the call_id. If the
// daemon dies during execution the file survives. On recovery, any pending
// call whose id matches the marker is treated as "interrupted" — a synthetic
// error result is appended instead of re-executing.

/// Write the call_id to `.exec_marker` so recovery can detect an
/// interrupted execution. Silently no-ops if `sessions_dir` is None.
pub(super) fn exec_marker_write(sessions_dir: Option<&std::path::Path>, session_id: &str, call_id: &str) {
    let Some(dir) = sessions_dir else {
        return;
    };
    let path = dir.join(crate::ids::bare_dir_name(session_id)).join(".exec_marker");
    let _ = std::fs::write(&path, call_id);
}

/// Read the call_id from `.exec_marker`, or `None` if absent.
pub(super) fn exec_marker_read(sessions_dir: Option<&std::path::Path>, session_id: &str) -> Option<String> {
    let dir = sessions_dir?;
    let path = dir.join(crate::ids::bare_dir_name(session_id)).join(".exec_marker");
    std::fs::read_to_string(&path).ok()
}

/// Remove `.exec_marker`. Silently no-ops if the file doesn't exist.
pub(super) fn exec_marker_clear(sessions_dir: Option<&std::path::Path>, session_id: &str) {
    let Some(dir) = sessions_dir else {
        return;
    };
    let path = dir.join(crate::ids::bare_dir_name(session_id)).join(".exec_marker");
    let _ = std::fs::remove_file(&path);
}

/// RAII guard that clears the exec marker when dropped. Created after
/// `execute()` returns so the marker is removed as soon as the tool
/// finishes — even on early returns from the loop.
pub(super) struct ExecMarkerGuard {
    pub(super) sessions_dir: Option<std::path::PathBuf>,
    pub(super) session_id: String,
}

impl Drop for ExecMarkerGuard {
    fn drop(&mut self) {
        exec_marker_clear(self.sessions_dir.as_deref(), &self.session_id);
    }
}

/// Pull the text of the most recent user message from history, if any.
/// Used for `TurnResult.pending_retry` so the orchestrator can surface
pub(super) fn last_user_text(session: &Session) -> String {
    session
        .history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.text_content())
        .unwrap_or_default()
}

pub(super) fn llm_usage(
    response: &CollectedResponse,
    provider: Option<String>,
    model: &str,
) -> Option<ChatMessageUsage> {
    let has_usage = response.usage.is_some();
    let usage = response.usage.as_ref();
    if !has_usage && provider.is_none() && model.is_empty() {
        return None;
    }
    Some(ChatMessageUsage {
        provider,
        model: Some(model.to_string()),
        input_tokens: usage.and_then(|u| u.input_tokens),
        cached_input_tokens: usage.and_then(|u| u.cached_input_tokens),
        output_tokens: usage.and_then(|u| u.output_tokens),
        reasoning_tokens: usage.and_then(|u| u.reasoning_tokens),
        cache_write_tokens: usage.and_then(|u| u.cache_write_tokens),
        stop_reason: Some(format!("{:?}", response.stop_reason)),
    })
}
