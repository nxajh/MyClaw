//! `run_recovery` — resume a session whose history ends mid-turn
//! (process crash / hot-switch during tool execution). Extracted
//! verbatim from `mod.rs` (batch 6, RFC §2).

use anyhow::Result;

use super::Agent;
use super::exec_marker::{exec_marker_clear, exec_marker_read, persist_last};
use super::tool_filter::filter_turn_scoped_tools;
use crate::agents::AgentRuntime;
use crate::agents::session::Session;
use crate::agents::turn::{TurnContext, TurnResult};

impl Agent {
    /// Resume a session whose history ends mid-turn (process crash,
    /// hot-switch during tool execution). Three cases handled, matching
    /// the legacy `AgentLoop::recover_interrupted_turn` semantics:
    ///
    /// - **Case A** — assistant tool_calls without matching tool_results:
    ///   re-execute each orphan call via the same ToolExecutor `run()`
    ///   uses, append the results to history, then fall through to
    ///   `run_inner()` so the LLM continues.
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
        session: &mut Session,
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

        // Case A: re-execute orphan tool_calls so history ends well-formed.
        if needs_case_a {
            tracing::info!(
                session = %session.id,
                missing_count = pending_calls.len(),
                "recovery: re-executing interrupted tool calls"
            );
            let mut allowed_tools = self.allowed_tools(runtime);
            filter_turn_scoped_tools(&mut allowed_tools, session);
            let tool_executor = &runtime.tool_executor;

            // Check the exec marker: if present, the call_id it contains was
            // mid-execution when the daemon died (e.g. `myclaw update` →
            // `systemctl restart` → SIGKILL). Re-running such a call would
            // kill the daemon again, creating a crash loop. Instead we
            // synthesize an error result so the LLM can assess the situation.
            let interrupted_id =
                exec_marker_read(runtime.sessions_dir.as_deref(), &session.id);
            if let Some(ref id) = interrupted_id {
                tracing::warn!(
                    session = %session.id,
                    interrupted_call_id = %id,
                    "recovery: exec marker found — a tool call was interrupted by daemon restart"
                );
            }

            for call in &pending_calls {
                // If this call was the one that killed the daemon, don't
                // blindly re-execute it — for `shell` in particular, a
                // tracked process can survive a `myclaw restart` hot switch
                // (see `crate::tools::shell::latest_entry_summary`), so
                // re-invoking would spawn a second copy racing the first.
                // Synthesize a result describing what's known instead and
                // let the LLM decide.
                if interrupted_id.as_deref() == Some(call.id.as_str()) {
                    let shell_detail = if call.name == "shell" {
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
                        // No independent process to check (non-shell tool
                        // call, or shell with no registry entry) — genuinely
                        // unknown whether it completed. Still name the
                        // actual restart cause instead of a blanket
                        // "daemon restart", since a deployment hot-switch is
                        // an expected, successful rollout, not an accident.
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
                    continue;
                }

                let result = tool_executor
                    .execute(
                        call,
                        session,
                        Some(&turn_ctx.permission_mode),
                        &allowed_tools,
                    )
                    .await;
                let (result_content, is_error) = match &result {
                    Ok(r) => {
                        let mut out = r.output.clone();
                        if let Some(ref err) = r.error {
                            if out.is_empty() {
                                out = format!("error: {}", err);
                            }
                        }
                        (out, !r.success)
                    }
                    Err(e) => (format!("error: {}", e), true),
                };
                session.add_tool_result(call.id.clone(), &call.name, result_content, is_error);
                persist_last(session);
            }

            // Clear any stale exec marker — recovery has handled all pending
            // calls, so the marker is no longer needed.
            exec_marker_clear(runtime.sessions_dir.as_deref(), &session.id);
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

        let mut session = session_with_orphan_shell_call(session_id, call_id);
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
}
