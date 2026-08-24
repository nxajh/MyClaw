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

#[cfg(test)]
mod tests {
    use super::{
        exec_marker_clear, exec_marker_read, exec_marker_write, ExecMarkerGuard,
    };

#[test]
fn exec_marker_write_read_clear_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions_dir = tmp.path();
    let session_id = "test_session_abc";

    // Initially no marker.
    assert!(exec_marker_read(Some(sessions_dir), session_id).is_none());

    // Write a marker.
    std::fs::create_dir_all(sessions_dir.join(crate::ids::bare_dir_name(session_id))).unwrap();
    exec_marker_write(Some(sessions_dir), session_id, "call_xyz");

    // Read it back.
    assert_eq!(
        exec_marker_read(Some(sessions_dir), session_id).as_deref(),
        Some("call_xyz")
    );

    // Clear it.
    exec_marker_clear(Some(sessions_dir), session_id);
    assert!(exec_marker_read(Some(sessions_dir), session_id).is_none());
}
#[test]
fn exec_marker_none_sessions_dir_is_noop() {
    // When sessions_dir is None (tests / CLI), all operations silently
    // do nothing — no panic, no file system access.
    exec_marker_write(None, "any_session", "any_call");
    assert!(exec_marker_read(None, "any_session").is_none());
    exec_marker_clear(None, "any_session");
}
#[test]
fn exec_marker_guard_clears_on_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions_dir = tmp.path();
    let session_id = "test_guard_session";

    std::fs::create_dir_all(sessions_dir.join(crate::ids::bare_dir_name(session_id))).unwrap();
    exec_marker_write(Some(sessions_dir), session_id, "call_guard");

    {
        let _guard = ExecMarkerGuard {
            sessions_dir: Some(sessions_dir.to_path_buf()),
            session_id: session_id.to_string(),
        };
        // Marker still present inside the scope.
        assert!(exec_marker_read(Some(sessions_dir), session_id).is_some());
    }
    // Guard dropped — marker cleared.
    assert!(exec_marker_read(Some(sessions_dir), session_id).is_none());
}
/// issue #106: the reported "interrupted by a daemon restart" wording
/// only appears in `run_recovery`'s Case A when `exec_marker_read`
/// returns a value matching the pending tool_call's id — the
/// hypothesis was that a delegation-timeout cancellation (via
/// `tokio::time::timeout` dropping the sub-agent's turn future
/// mid-tool-execution) might leave a stale marker behind, same as an
/// actual daemon crash, and wrongly trigger that wording.
///
/// This test exercises the ACTUAL cancellation mechanism a delegation
/// timeout uses (not just ordinary scope exit, which
/// `exec_marker_guard_clears_on_drop` already covers) — the guard is
/// held across an await inside a future that `tokio::time::timeout`
/// cuts off. If Drop still runs correctly under real async
/// cancellation, the marker must be gone afterward — ruling out this
/// code path as the source of the mislabeling.
#[tokio::test]
async fn exec_marker_guard_clears_on_timeout_cancellation() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions_dir = tmp.path().to_path_buf();
    let session_id = "test_timeout_cancel_session".to_string();

    std::fs::create_dir_all(sessions_dir.join(crate::ids::bare_dir_name(&session_id)))
        .unwrap();
    exec_marker_write(Some(&sessions_dir), &session_id, "call_timeout");
    assert!(exec_marker_read(Some(&sessions_dir), &session_id).is_some());

    let sd = sessions_dir.clone();
    let sid = session_id.clone();
    let result = tokio::time::timeout(std::time::Duration::from_millis(20), async move {
        let _guard = ExecMarkerGuard {
            sessions_dir: Some(sd),
            session_id: sid,
        };
        // A tool call that outlives the timeout, same shape as a
        // sub-agent's turn future being cut off by DelegationTimeout.
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    })
    .await;

    assert!(result.is_err(), "expected the timeout to fire first");
    assert!(
        exec_marker_read(Some(&sessions_dir), &session_id).is_none(),
        "exec marker must be cleared even when the guard's scope is exited via \
         tokio::time::timeout cancellation, not just ordinary drop — if this holds, \
         a delegation timeout can never leave a stale marker behind, so run_recovery's \
         \"daemon restart\" wording is unreachable via this path for a timed-out delegation"
    );
}
}
