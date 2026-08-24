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
