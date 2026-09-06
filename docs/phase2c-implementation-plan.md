# Phase 2c (issue #256) — sessions_yield wait moved into the tool frame
Status: WIP analysis (coder sub-agent, budget exhausted during planning). Baseline 1f08e8e5.

Derived edit plan (verified against 1f08e8e5 sources):

1. src/agents/turn.rs — TurnContext gains
   `pub yield_park: Option<std::sync::Weak<crate::agents::session_context::SessionContext>>`
   (weak handle back to owning ctx; ~19 literal construction sites get `yield_park: None,`,
   the 3 sites in session_context/mod.rs get `Some(Arc::downgrade(self))`).
2. src/agents/agent/mod.rs — `Agent::run`, `run_inner`, `run_recovery` take
   `session_slot: &mut Option<OwnedMutexGuard<Session>>` + `turn_guard_slot: &mut Option<OwnedMutexGuard<()>>`
   (#258 Option A shape). run_inner: per-iteration shadow
   `let session = session_slot.as_mut().expect(...)` (NLL releases before take).
   execute_tool_batch call wraps: take slots -> local Options -> call -> put back.
   New `ToolBatchOutcome::Superseded` => return TurnResult{text: response.text, EndTurn, has_pending: turn_state.has_pending()}.
3. src/agents/agent/tool_phase.rs — execute_tool_batch takes both slots + `turn_ctx: &TurnContext`
   (replaces permission_mode param; body uses turn_ctx.permission_mode). Yield branch:
   set pending_yield + strip trailing calls (keep); if yield_park upgrades: take guards,
   register YieldWaiter on ctx.yield_waiter, drop guards, rx.await, reacquire
   turn_lock -> session (same order as park_for_yield), put guards back:
   Ok+id match => clear pending_yield, add_tool_result, persist, Continue;
   Ok+mismatch => enqueue PendingYieldEvent::fresh(content), Superseded;
   Err => Superseded. No stub ToolResult event, no channel End event, no EndTurn on this path.
   No-park fallback (CLI/tests): legacy deterministic EndTurn (run_and_deliver tail parks).
4. src/agents/session_context/mod.rs — run_and_deliver: wrap owned guards into slots before loop,
   shadow after agent.run, tail park: read pending id into local, park_for_yield(slots.take()...),
   put returned guards back. park_for_yield / try_fill_pending_yield / waiter branch / cold path untouched.
5. Tests (tokio::test, no LLM): (a) park+try_fill signal => Continue+result; (b) waiter tx dropped => no history write, Superseded; (c) send after supersede => content re-enqueued; (d) event queued before park => immediate fill.
   Existing tests: agent/tests.rs run call + 4 execute_tool_batch call sites need slot args.
