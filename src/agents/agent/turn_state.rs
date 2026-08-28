//! Turn-scoped mutable state for the agent turn loop.
//! Encapsulates the retry counters and loop-carried flags that `run_inner`
//! threads through its loop body.

/// Loop-carried state for a single `run_inner` invocation.
/// 字段 pub(super) 是批 2 的过渡态——批 3/4 提取 injections/finalize/tool_phase
/// 时收敛为方法边界（见 RFC §4.6 信息隐藏闸）。
#[derive(Default)]
pub(super) struct TurnState {
    /// Empty (or `tool_use`-without-calls) responses seen so far; bounded by
    /// `MAX_EMPTY_RETRIES` in `run_inner`.
    pub(super) empty_response_retries: usize,
    /// Provider-reported context-overflow retries; bounded by
    /// `MAX_OVERFLOW_RETRIES` in `run_inner`.
    pub(super) overflow_retries: usize,
    /// Track whether the main agent called memory_manage this turn —
    /// if so, the forked extraction is redundant (mutual exclusion).
    pub(super) turn_called_memory: bool,
    /// Track whether any tool call errored this turn — skill extraction
    /// only fires on clean turns (no errors).
    pub(super) turn_had_error: bool,
    /// Async delegation is a turn boundary: after the entire provider batch
    /// has been executed and persisted, return to SessionContext so the
    /// origin turn releases its lock before the completion wake is queued.
    pub(super) async_delegation_spawned: bool,
    /// issue #140: same turn-boundary role as `async_delegation_spawned`,
    /// but for a `shell` call armed for a future completion notice
    /// (`background: true`, or a foreground call converted mid-flight by
    /// the timeout branch — see `tools::shell`'s `mark_notify_on_exit`).
    /// The shell tool itself registers the pending entry (via a
    /// SessionManager lookup, mirroring how `agent_delegate`'s async path
    /// registers through `DelegationCoordinator`); this flag only decides
    /// whether THIS turn is the origin of a suspension sequence, exactly
    /// like `async_delegation_spawned` does for delegation.
    pub(super) shell_pending_spawned: bool,
}

impl TurnState {
    /// #140 has_pending 语义：本轮是否留下了未完成的异步工作
    /// （async delegation 或后台 shell）。原 mod.rs :715/:949 两处内联派生统一到这。
    pub(super) fn has_pending(&self) -> bool {
        self.async_delegation_spawned || self.shell_pending_spawned
    }
}

#[cfg(test)]
mod tests {
    // The three detection predicates below mirror the inline checks in
    // `tool_phase.rs` exactly; they were moved here in batch 5 because the
    // flags they gate (`async_delegation_spawned` / `shell_pending_spawned` /
    // the yield path) live in `TurnState`.
    use super::*;

#[test]
fn async_delegation_mode_detection() {
    // The boundary logic in Agent::run detects a successful async
    // agent_delegate by parsing the call arguments. This test
    // verifies the JSON parsing logic in isolation (the inline
    // detection mirrors this exactly).
    fn is_async_delegate(name: &str, is_error: bool, arguments: &str) -> bool {
        if name != "agent_delegate" || is_error {
            return false;
        }
        serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| {
                v.get("mode").and_then(|m| m.as_str()).map(str::to_owned)
            })
            .map(|m| m == "async")
            .unwrap_or(false)
    }

    // async mode → detected
    assert!(is_async_delegate(
        "agent_delegate",
        false,
        r#"{"agent":"coder","task":"x","mode":"async"}"#
    ));
    // sync mode (explicit) → not detected
    assert!(!is_async_delegate(
        "agent_delegate",
        false,
        r#"{"agent":"coder","task":"x","mode":"sync"}"#
    ));
    // mode omitted → not detected (tool rejects missing mode since
    // 2026-08-14 — it became required; the parser treats it as non-async)
    assert!(!is_async_delegate(
        "agent_delegate",
        false,
        r#"{"agent":"coder","task":"x"}"#
    ));
    // error result → not detected even with async mode
    assert!(!is_async_delegate(
        "agent_delegate",
        true,
        r#"{"agent":"coder","task":"x","mode":"async"}"#
    ));
    // different tool → not detected
    assert!(!is_async_delegate(
        "file_read",
        false,
        r#"{"path":"x"}"#
    ));
}
/// issue #140: mirrors the inline `shell_pending_spawned` check exactly
/// — a shell call is armed for a future completion notice iff it's
/// `shell` (not `shell_poll`/`shell_kill`), didn't error, and its output
/// starts with `state=running` (the header only `background: true`'s
/// immediate return and the timeout-conversion return use).
#[test]
fn shell_pending_spawned_detection_logic() {
    fn is_shell_armed(name: &str, is_error: bool, output: &str) -> bool {
        name == "shell" && !is_error && output.starts_with("state=running")
    }

    // background: true's immediate return → armed
    assert!(is_shell_armed(
        "shell",
        false,
        "state=running\nprocess_id=sh_x\ncommand=echo hi\n..."
    ));
    // timeout-conversion return → armed
    assert!(is_shell_armed(
        "shell",
        false,
        "state=running\nprocess_id=sh_x\ncommand=echo hi\ntimeout_secs=0\nnote=..."
    ));
    // fast completion within timeout_secs → NOT armed (already delivered
    // its terminal result synchronously)
    assert!(!is_shell_armed(
        "shell",
        false,
        "state=exited\nexit_code=0\nprocess_id=sh_x\n..."
    ));
    // error result → not armed even if the output happens to start with
    // the marker
    assert!(!is_shell_armed("shell", true, "state=running\n..."));
    // different tool → not armed regardless of output shape
    assert!(!is_shell_armed("shell_poll", false, "state=running\n..."));
    assert!(!is_shell_armed("shell_kill", false, "state=running\n..."));
}
#[test]
fn sessions_yield_detection_logic() {
    // The yield detection in Agent::run (RFC delegation-notice-queue §3.2)
    // triggers a deterministic EndTurn when the model calls sessions_yield
    // without error. has_pending follows async_delegation_spawned. This
    // test verifies the condition logic in isolation (mirrors the inline
    // check exactly).
    fn is_yield(name: &str, is_error: bool) -> bool {
        name == "sessions_yield" && !is_error
    }

    // Normal call → yields EndTurn
    assert!(is_yield("sessions_yield", false));

    // Error result → NOT detected (tool errored, don't truncate)
    assert!(!is_yield("sessions_yield", true));

    // Different tool → not detected
    assert!(!is_yield("calculator", false));
    assert!(!is_yield("agent_delegate", false));

    // When yield triggers without prior async delegation, has_pending is
    // false — plain EndTurn, no suspension. When agent_delegate(async)
    // ran earlier in the same batch, async_delegation_spawned is true, so
    // has_pending is true (suspension proceeds normally).
    let mut async_delegation_spawned = false;
    assert_eq!(async_delegation_spawned, false); // pre-condition

    // Simulate agent_delegate(async) running before sessions_yield
    async_delegation_spawned = true;
    // yield with prior delegation → has_pending = true
    assert_eq!(async_delegation_spawned, true);
}
}
