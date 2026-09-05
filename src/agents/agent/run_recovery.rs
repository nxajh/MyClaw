//! `run_recovery` — resume a session whose history ends mid-turn
//! (process crash / hot-switch during tool execution). Extracted
//! verbatim from `mod.rs` (batch 6, RFC §2).

use anyhow::Result;

use super::Agent;
use super::exec_marker::{exec_marker_clear, exec_marker_read, persist_last};
use crate::agents::AgentRuntime;
use crate::agents::session::Session;
use crate::agents::turn::{TurnContext, TurnResult};
use tokio::sync::OwnedMutexGuard;

impl Agent {
    /// Resume a session whose history ends mid-turn (process crash,
    /// hot-switch during tool execution). Three cases handled, matching
    /// the legacy `AgentLoop::recover_interrupted_turn` semantics:
    ///
    /// - **Case A** — assistant tool_calls without matching tool_results:
    ///   synthesize a status result for each orphan call (never re-execute
    ///   it — issue #232), append the results to history, then fall through
    ///   to `run_inner()` so the LLM continues and can decide whether to
    ///   retry.
    /// - **Case B** — trailing tool_results, no LLM response: just call
    ///   `run_inner()`. The chat loop sends the current history to the LLM
    ///   without appending a fresh user message.
    /// - **Case C** — trailing user message, no LLM response: same as
    ///   Case B.
    ///
    /// Returns `None` if the session's history is empty or not in a
    /// mid-turn state (no recovery needed).
    pub async fn run_recovery(
        &self,
        session: &mut OwnedMutexGuard<Session>,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<Option<TurnResult>> {
        use std::collections::HashSet;

        if session.history.is_empty() {
            return Ok(None);
        }

        // Walk backwards collecting completed tool_call_ids and finding
        // any orphan tool_calls in the most recent assistant message.
        let mut completed_ids: HashSet<String> = HashSet::new();
        let mut pending_calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut has_trailing_tool_results = false;
        let mut last_is_user = false;

        for msg in session.history.iter().rev() {
            if msg.role == "tool" {
                if let Some(ref id) = msg.tool_call_id {
                    completed_ids.insert(id.clone());
                }
                has_trailing_tool_results = true;
            } else if msg.role == "assistant" {
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        if !completed_ids.contains(&call.id) {
                            pending_calls.push(call.clone());
                        }
                    }
                }
                break;
            } else if msg.role == "user" {
                last_is_user = true;
                break;
            } else {
                break;
            }
        }

        let needs_case_a = !pending_calls.is_empty();
        let needs_case_b = has_trailing_tool_results && pending_calls.is_empty();
        let needs_case_c = last_is_user;
        if !(needs_case_a || needs_case_b || needs_case_c) {
            return Ok(None);
        }

        // issue #240: a `sessions_yield` orphan whose tool_call_id still
        // matches `session.pending_yield` is a deliberately deferred yield
        // (see `tool_phase.rs`'s `is_deferred_yield` branch), not a genuine
        // mid-turn crash — the #238 mechanism is intentionally waiting to
        // fill its result with whatever completes next (a sub-agent/shell
        // notice, eventually a user interjection), not a synthesized
        // "interrupted" message.
        //
        // `SessionContext::try_fill_pending_yield` already had first crack
        // at it: `OrchestratorCtx::session_context_for` runs it eagerly the
        // moment a `SessionContext` is (re)materialized, specifically so
        // this runs before recovery reaches this point. If anything was
        // already queued (e.g. a sub-agent finished while the daemon was
        // down), that pass already appended the real result — the
        // breakpoint disappears once the tool_call has a result, so
        // `pending_calls` above would no longer contain it and this branch
        // is never reached. Reaching here means genuinely nothing was
        // queued yet (or the eager pass hasn't won the race for the session
        // lock — either way the outcome is the same: nothing to fill it
        // with right now): leave the tool_call unresolved and hand back
        // `Ok(None)`. Recovery has nothing to do; the turn is still waiting
        // exactly like it was before the restart, and whichever of the two
        // tasks reaches the lock second is a no-op against the other's
        // result.
        if pending_calls.len() == 1
            && pending_calls[0].name == "sessions_yield"
            && session
                .pending_yield
                .as_ref()
                .is_some_and(|p| p.tool_call_id == pending_calls[0].id)
        {
            tracing::debug!(
                session = %session.id,
                tool_call_id = %pending_calls[0].id,
                "recovery: sessions_yield still genuinely pending (nothing queued) — leaving it unresolved"
            );
            return Ok(None);
        }

        // Case A: close out orphan tool_calls so history ends well-formed.
        //
        // issue #232: this used to re-execute (blindly replay) any orphan
        // call whose id didn't match the exec marker, on the theory that a
        // non-matching call was presumably just unlucky timing, not the
        // actual cause of death. In production this caused a restart storm:
        // a graceful SIGUSR1 hot switch lets the triggering command (e.g.
        // `shell("myclaw restart")`) return normally well before the daemon
        // actually exits, so by the time the process dies the marker has
        // already been cleared by that call's own `ExecMarkerGuard` — the
        // *next* orphan call (if any) then never matches, gets blindly
        // replayed, and if that replay is itself another restart trigger,
        // the daemon dies again with the same non-matching marker state.
        // Since the replay path here never wrote its own marker either,
        // once the marker was absent it stayed absent for every subsequent
        // cycle — 22 replay cycles, 0 marker matches, one shell session.
        //
        // Fix: never re-execute an orphan call here, regardless of marker
        // state. The marker is now purely informational — when it matches a
        // pending call we can say something specific (e.g. a shell command's
        // tracked process state); when it doesn't, we fall back to a generic
        // "unknown, check before retrying" message. Either way the call is
        // synthesized, never replayed, and the retry decision is left to the
        // LLM (which can re-issue the call itself if warranted).
        if needs_case_a {
            tracing::info!(
                session = %session.id,
                missing_count = pending_calls.len(),
                "recovery: closing interrupted tool calls (no blind replay)"
            );

            // If present, the exec marker names the call_id that was
            // mid-execution when the daemon died (e.g. `myclaw update` →
            // `systemctl restart` → SIGKILL). It only enriches the message
            // below for whichever pending call it matches — it no longer
            // gates whether a call gets re-executed.
            let interrupted_id =
                exec_marker_read(runtime.sessions_dir.as_deref(), &session.id);
            if let Some(ref id) = interrupted_id {
                tracing::warn!(
                    session = %session.id,
                    interrupted_call_id = %id,
                    "recovery: exec marker found — a tool call was interrupted by daemon restart"
                );
            }

            // Only clear the marker once it's actually been matched against
            // a call in this pass — an unmatched marker may still describe
            // an unresolved situation elsewhere, and clearing it blindly
            // would discard that forensic information for no reason.
            let mut marker_consumed = false;

            for call in &pending_calls {
                let matched = interrupted_id.as_deref() == Some(call.id.as_str());
                if matched {
                    marker_consumed = true;
                }

                let shell_detail = if matched && call.name == "shell" {
                    runtime
                        .sessions_dir
                        .as_deref()
                        .and_then(|dir| crate::tools::shell::latest_entry_summary(dir, &session.id))
                } else {
                    None
                };
                // issue #228: for `shell`, the process registry already
                // knows exactly what happened (survived a hot-switch and
                // is still running, survived but its outcome is unknown,
                // or was definitely killed alongside a non-hot-switch
                // restart) — use that state-specific wording verbatim
                // instead of a generic "interrupted by a daemon restart"
                // framing, which is actively wrong for the common case
                // of a command that just kept running across a
                // deployment hot-switch.
                let (msg, is_error) = match shell_detail {
                    Some((d, is_error)) => (format!("[recovery] {}", d), is_error),
                    // No independent process to check (non-shell tool call,
                    // shell with no registry entry, or the marker simply
                    // didn't name this call) — genuinely unknown whether it
                    // completed. Still name the actual restart cause instead
                    // of a blanket "daemon restart", since a deployment
                    // hot-switch is an expected, successful rollout, not an
                    // accident.
                    None => (
                        format!(
                            "[recovery] This tool call was in flight during {} — its \
                             completion status is unknown and it will not be \
                             automatically re-executed, since re-running it could \
                             duplicate side effects it may have already produced. Check \
                             the actual state (e.g. the relevant files, git log, or logs) \
                             before deciding whether to retry.",
                            interrupted_restart_cause(crate::hot_switch::is_hot_switch())
                        ),
                        true,
                    ),
                };
                tracing::warn!(
                    session = %session.id,
                    call_id = %call.id,
                    tool = %call.name,
                    "recovery: interrupted call not re-executed"
                );
                session.add_tool_result(call.id.clone(), &call.name, msg, is_error);
                persist_last(session);
            }

            if marker_consumed {
                exec_marker_clear(runtime.sessions_dir.as_deref(), &session.id);
            }
        }

        // Cases B, C, and tail of A: drive the LLM loop from the now
        // well-formed history. The user message (if any) is already in
        // history. `run_inner` (not `run`): `run` would re-enter
        // `run_recovery` and recurse forever.
        let tr = self.run_inner(session, turn_ctx, runtime).await?;
        Ok(Some(tr))
    }
}

/// issue #228: name the actual restart cause for a non-shell interrupted
/// tool call (no independent process to check, so completion is still
/// unknown either way) — a deployment hot-switch is an expected, successful
/// rollout, not an accident, even though we can't tell whether this
/// particular call finished. Extracted as a pure function so the wording can
/// be tested for both branches without touching the global `MYCLAW_HOT_SWITCH`
/// env var `crate::hot_switch::is_hot_switch()` reads (process-global state,
/// unsafe to mutate from a parallel test).
fn interrupted_restart_cause(is_hot_switch: bool) -> &'static str {
    if is_hot_switch {
        "a deployment hot-switch restart"
    } else {
        "a daemon crash/restart"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent::tests::{bailing_runtime, empty_config};
    use crate::config::agent::{PermissionMode, RunMode};
    use crate::providers::ToolCall;

    /// Wrap a plain test-constructed `Session` in the `Arc<Mutex<_>>` +
    /// owned-guard shape `run_recovery`/`run_inner` now require (Phase 1 of
    /// issue #256's lock-chain refactor).
    async fn owned(session: Session) -> OwnedMutexGuard<Session> {
        std::sync::Arc::new(tokio::sync::Mutex::new(session))
            .lock_owned()
            .await
    }

    #[test]
    fn interrupted_restart_cause_names_hot_switch_not_generic_daemon_restart() {
        assert_eq!(interrupted_restart_cause(true), "a deployment hot-switch restart");
        assert_ne!(interrupted_restart_cause(true), "a daemon crash/restart");
    }

    #[test]
    fn interrupted_restart_cause_keeps_crash_wording_when_not_hot_switch() {
        assert_eq!(interrupted_restart_cause(false), "a daemon crash/restart");
    }

    fn session_with_orphan_shell_call(session_id: &str, call_id: &str) -> Session {
        let mut session = Session::new(session_id.to_string());
        session.add_user("please deploy".to_string());
        session.add_assistant_with_tools(
            String::new(),
            vec![ToolCall {
                id: call_id.to_string(),
                name: "shell".to_string(),
                arguments: r#"{"command":"myclaw update"}"#.to_string(),
            }],
            None,
            None,
            None,
            None,
        );
        session
    }

    fn write_shell_entry(
        sessions_dir: &std::path::Path,
        session_id: &str,
        process_id: &str,
        state: &str,
    ) {
        let dir = sessions_dir
            .join(crate::ids::bare_dir_name(session_id))
            .join(".shell_procs");
        std::fs::create_dir_all(&dir).unwrap();
        let entry = serde_json::json!({
            "process_id": process_id,
            "session_id": session_id,
            "command": "myclaw update",
            "workdir": null,
            "pid": 12345,
            "pid_start_ticks": null,
            "spawned_at_ms": 0,
            "output_path": "/tmp/myclaw-test-does-not-exist.out",
            "state": state,
            "exit_code": null,
            "notify_on_exit": false,
        });
        std::fs::write(
            dir.join(format!("{process_id}.json")),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();
    }

    /// issue #228: a shell call interrupted by a NON-hot-switch restart
    /// (`ProcEntry.state == "lost_on_restart"`) must get a definite "did not
    /// complete" message, not the old generic "interrupted by a daemon
    /// restart ... may have partially or fully completed" hedge.
    #[tokio::test]
    async fn case_a_shell_lost_on_restart_gives_definite_message() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "sess-recovery-1";
        let call_id = "call_1";

        let marker_dir = tmp.path().join(crate::ids::bare_dir_name(session_id));
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join(".exec_marker"), call_id).unwrap();
        write_shell_entry(tmp.path(), session_id, "sh_lost", "lost_on_restart");

        let mut session = owned(session_with_orphan_shell_call(session_id, call_id)).await;
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime().with_sessions_dir(tmp.path().to_path_buf());
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        // run_inner hits the stub registry and errors — recovery's own
        // synthesized tool result is appended to `session.history` before
        // that happens, which is what this test inspects.
        let _ = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        let tool_msg = session
            .history
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(call_id))
            .expect("recovery must close the orphan tool_call with a tool result");
        let content = tool_msg.text_content();
        assert_eq!(tool_msg.is_error, Some(true));
        assert!(
            !content.contains("daemon restart") || content.contains("confirmed"),
            "must not fall back to the generic hedge wording: {content}"
        );
        assert!(content.contains("confirmed"), "got: {content}");
        assert!(
            !content.to_lowercase().contains("may have"),
            "must not hedge for a confirmed non-hot-switch kill: {content}"
        );
    }

    /// A non-shell interrupted call has no independent process to check —
    /// the message must still explain what to do next (check state before
    /// retrying), not just report unknown status.
    #[tokio::test]
    async fn case_a_non_shell_call_gives_actionable_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "sess-recovery-2";
        let call_id = "call_2";

        let marker_dir = tmp.path().join(crate::ids::bare_dir_name(session_id));
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join(".exec_marker"), call_id).unwrap();

        let mut session = Session::new(session_id.to_string());
        session.add_user("please write the file".to_string());
        session.add_assistant_with_tools(
            String::new(),
            vec![ToolCall {
                id: call_id.to_string(),
                name: "file_write".to_string(),
                arguments: r#"{"path":"out.txt"}"#.to_string(),
            }],
            None,
            None,
            None,
            None,
        );
        let mut session = owned(session).await;
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime().with_sessions_dir(tmp.path().to_path_buf());
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        let _ = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        let tool_msg = session
            .history
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(call_id))
            .expect("recovery must close the orphan tool_call with a tool result");
        let content = tool_msg.text_content();
        assert_eq!(tool_msg.is_error, Some(true));
        assert!(
            content.contains("before deciding whether to retry"),
            "must give actionable next-step guidance, not just report unknown status: {content}"
        );
        assert!(
            content.contains("in flight during"),
            "must name the actual restart cause: {content}"
        );
    }

    /// issue #232 (defect A): an orphan call with NO exec marker at all must
    /// still never be blindly re-executed. Before the fix, a missing marker
    /// fell through to `tool_executor.execute()` — with the empty tool
    /// registry `bailing_runtime()` provides, that would produce an "Unknown
    /// tool" error instead of the recovery guidance message. Asserting its
    /// absence is a precise way to prove `execute()` was never reached,
    /// without needing a dedicated spy tool.
    #[tokio::test]
    async fn case_a_no_marker_never_replays_orphan_call() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "sess-recovery-no-marker";
        let call_id = "call_no_marker";

        // Deliberately no `.exec_marker` file written at all.
        let mut session = owned(session_with_orphan_shell_call(session_id, call_id)).await;
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime().with_sessions_dir(tmp.path().to_path_buf());
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        let _ = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        let tool_msg = session
            .history
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(call_id))
            .expect("recovery must close the orphan tool_call with a tool result");
        let content = tool_msg.text_content();
        assert!(
            !content.contains("Unknown tool"),
            "must never reach tool_executor.execute() — got: {content}"
        );
        assert!(
            content.contains("before deciding whether to retry"),
            "must fall back to the generic guidance message: {content}"
        );
    }

    /// issue #232 (defect A): a marker that names a DIFFERENT call than the
    /// orphan being processed must also never trigger a blind replay — a
    /// mismatch is exactly the scenario that produced the restart storm.
    #[tokio::test]
    async fn case_a_mismatched_marker_never_replays_orphan_call() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "sess-recovery-mismatch";
        let call_id = "call_current";

        let marker_dir = tmp.path().join(crate::ids::bare_dir_name(session_id));
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join(".exec_marker"), "call_some_other_stale_id").unwrap();

        let mut session = owned(session_with_orphan_shell_call(session_id, call_id)).await;
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime().with_sessions_dir(tmp.path().to_path_buf());
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        let _ = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        let tool_msg = session
            .history
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(call_id))
            .expect("recovery must close the orphan tool_call with a tool result");
        let content = tool_msg.text_content();
        assert!(
            !content.contains("Unknown tool"),
            "must never reach tool_executor.execute() — got: {content}"
        );
        assert!(
            content.contains("before deciding whether to retry"),
            "must fall back to the generic guidance message: {content}"
        );
    }

    /// issue #232 (defect B): a marker that doesn't match anything in this
    /// recovery pass must be left on disk, not blindly cleared — it may
    /// still describe an unresolved situation the caller hasn't diagnosed
    /// yet, and clearing it would discard that forensic information.
    #[tokio::test]
    async fn case_a_unmatched_marker_is_not_cleared() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "sess-recovery-unmatched-clear";
        let call_id = "call_current_2";

        let marker_dir = tmp.path().join(crate::ids::bare_dir_name(session_id));
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join(".exec_marker"), "call_some_other_stale_id").unwrap();

        let mut session = owned(session_with_orphan_shell_call(session_id, call_id)).await;
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime().with_sessions_dir(tmp.path().to_path_buf());
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        let _ = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        assert_eq!(
            std::fs::read_to_string(marker_dir.join(".exec_marker")).ok(),
            Some("call_some_other_stale_id".to_string()),
            "an unmatched marker must survive the recovery pass untouched"
        );
    }

    /// issue #232 (defect B): once a marker DOES match a call processed in
    /// this pass, it has served its purpose and should be cleared — this
    /// guards the "only clear when consumed" change against also becoming
    /// "never clear at all".
    #[tokio::test]
    async fn case_a_matched_marker_is_cleared() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "sess-recovery-matched-clear";
        let call_id = "call_matched";

        let marker_dir = tmp.path().join(crate::ids::bare_dir_name(session_id));
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join(".exec_marker"), call_id).unwrap();

        let mut session = owned(session_with_orphan_shell_call(session_id, call_id)).await;
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime().with_sessions_dir(tmp.path().to_path_buf());
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        let _ = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        assert!(
            std::fs::read_to_string(marker_dir.join(".exec_marker")).is_err(),
            "a matched marker must be cleared once consumed"
        );
    }

    fn session_with_orphan_sessions_yield(session_id: &str, call_id: &str) -> Session {
        let mut session = Session::new(session_id.to_string());
        session.add_user("do something async".to_string());
        session.add_assistant_with_tools(
            String::new(),
            vec![ToolCall {
                id: call_id.to_string(),
                name: "sessions_yield".to_string(),
                arguments: "{}".to_string(),
            }],
            None,
            None,
            None,
            None,
        );
        session
    }

    /// issue #240: a `sessions_yield` orphan whose id matches
    /// `session.pending_yield` (nothing was queued for it yet — the eager
    /// redelivery pass had nothing to fill it with) must be left genuinely
    /// unresolved: no synthesized "[recovery] ..." result, and no
    /// `run_inner` call that would send malformed history (an unresolved
    /// tool_call) to the LLM.
    #[tokio::test]
    async fn sessions_yield_with_matching_pending_yield_is_left_unresolved() {
        let session_id = "sess-yield-pending";
        let call_id = "call_y1";
        let mut session = session_with_orphan_sessions_yield(session_id, call_id);
        session.pending_yield = Some(crate::agents::session::PendingYield {
            tool_call_id: call_id.to_string(),
            implicit: false,
        });
        let mut session = owned(session).await;
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime();
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        let result = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        assert!(
            matches!(result, Ok(None)),
            "must report nothing-to-recover, not a synthesized turn result"
        );
        assert!(
            !session
                .history
                .iter()
                .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(call_id)),
            "must not synthesize a tool_result for a deliberately deferred yield"
        );
        assert!(
            session.pending_yield.is_some(),
            "pending_yield must survive recovery untouched, ready for a real delivery"
        );
    }

    /// A `sessions_yield` orphan with NO matching `pending_yield` (e.g. it
    /// was part of a mixed breakpoint on load, so `Session::manager`
    /// deliberately left `pending_yield` `None`) is a genuine mid-turn
    /// crash for this call too — it must fall through to the ordinary
    /// generic Case A handling, exactly like any other tool.
    #[tokio::test]
    async fn sessions_yield_without_matching_pending_yield_falls_back_to_generic_case_a() {
        let session_id = "sess-yield-no-pending";
        let call_id = "call_y2";
        let mut session = owned(session_with_orphan_sessions_yield(session_id, call_id)).await;
        // Deliberately no `session.pending_yield` set.
        let agent = Agent::new(empty_config());
        let runtime = bailing_runtime();
        let turn_ctx = TurnContext {
            system_prompt: "",
            model_id: None,
            thinking: None,
            permission_mode: PermissionMode::Default,
            run_mode: RunMode::Interactive,
        };

        let _ = agent.run_recovery(&mut session, turn_ctx, &runtime).await;

        let tool_msg = session
            .history
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(call_id))
            .expect("without a matching pending_yield, the orphan must be closed out like any other");
        assert_eq!(tool_msg.is_error, Some(true));
    }
}
