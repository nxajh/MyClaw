//! Async delegation lifecycle: timeout resolution helpers, background
//! spawning, timed-out resume, and daemon-restart recovery.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agents::delegation::{
    DelegationEvent, DelegationStatus, DelegationTimeout, SubAgentMailbox,
};
use crate::agents::SessionContext;

use super::registry::RunningEntry;
use super::{DelegationCoordinator, SUB_AGENT_TIMEOUT_DEFAULT_SECS, SUB_AGENT_TIMEOUT_MAX_SECS};

/// Capacity of each running sub-agent's inbox (parent → sub messages).
const SUB_AGENT_INBOX_CAPACITY: usize = 64;

/// Resolve the effective timeout.
///
/// Priority: `tool_timeout` > `config_timeout` > `SUB_AGENT_TIMEOUT_DEFAULT_SECS`,
/// clamped only to the global `SUB_AGENT_TIMEOUT_MAX_SECS` hard ceiling.
/// There is deliberately no per-agent override of that ceiling anymore
/// (removed 2026-08-19) — it let `SubAgentConfig.max_timeout` silently clamp
/// an explicit `agent_delegate` tool-call `timeout` down with no visibility
/// to the caller, which looked indistinguishable from the parameter being
/// ignored outright. The tool-call value is now authoritative up to the
/// global ceiling, full stop.
pub(super) fn resolve_timeout(tool_timeout: Option<u64>, config_timeout: Option<u64>) -> u64 {
    tool_timeout
        .or(config_timeout)
        .unwrap_or(SUB_AGENT_TIMEOUT_DEFAULT_SECS)
        .min(SUB_AGENT_TIMEOUT_MAX_SECS)
}

/// Whether `delegate_with_parent` should GC (delete) the sub-session's own
/// history immediately after it reaches a terminal state (issue #106).
///
/// Only for a *successful sync* delegation: the sync caller already has the
/// full result text folded into its tool-call return, and there is no other
/// consumer that would ever look the sub-session up again, so keeping its
/// history around is pure disk waste. Every other case must keep it:
/// - Failed/timed-out sync delegations are deliberately NOT GC'd here (the
///   caller sees the error text, but a failed/timed-out sub-session's
///   checkpoint is kept as a resumable tombstone by the code right after
///   this call — deleting the session history out from under that would
///   break `agent_resume`).
/// - Async delegations must never be GC'd on this path, success or not:
///   the result also reaches the parent asynchronously via a
///   `DelegationEvent`, and `session_query`/`sessions_yield` callers
///   reasonably expect to still be able to look up what a background task
///   actually did after it finishes. This was the actual bug: the
///   pre-fix code GC'd on `result.is_ok()` alone, so a successfully
///   completed *async* delegation's sub-session vanished immediately,
///   while a *timed-out-then-resumed* one (which never satisfies
///   `result.is_ok()` on this call) stayed visible — the inconsistency
///   the issue observed.
pub(super) fn should_gc_sub_session(is_async_delegation: bool, result_is_ok: bool) -> bool {
    result_is_ok && !is_async_delegation
}

/// issue #251: `Agent::run_recovery` returns `Ok(None)` both when the
/// session has no history at all AND when its history already ends with a
/// clean, final assistant response (no incomplete turn to continue) — the
/// latter is exactly what a `resume_timed_out` call sees when the sub-agent
/// completed naturally between the original timeout and the resume. Extract
/// that existing final answer so the caller can report it as a genuine
/// completion instead of a failure. Returns `None` for any other shape
/// (empty history, or an unexpected trailing role) — the caller falls back
/// to its original "nothing to recover" error in that case.
fn existing_final_text(session: &crate::agents::session::Session) -> Option<crate::agents::turn::TurnResult> {
    let last = session.history.last()?;
    // `tool_calls` may be `None` or `Some(vec![])` for a plain final answer
    // depending on which `Session` helper appended it — both mean "no
    // pending calls", matching how `run_recovery`'s own Case A detection
    // treats them identically (iterating an empty `Some` is a no-op).
    let has_tool_calls = last.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
    if last.role != "assistant" || has_tool_calls {
        return None;
    }
    let text: String = last
        .parts
        .iter()
        .filter_map(|p| match p {
            crate::providers::ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(crate::agents::turn::TurnResult {
        text,
        stop_reason: crate::providers::StopReason::EndTurn,
        pending_retry: None,
        has_pending: false,
    })
}

impl DelegationCoordinator {
    /// Delegate a task asynchronously — spawns the sub-agent in a
    /// background tokio task whose JoinHandle is stashed in `running`
    /// so `/agent_list` and `/agent_kill` can see it.
    pub fn spawn_delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        parent_session_id: &str,
        timeout_secs: u64,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
    ) -> anyhow::Result<String> {
        let agent = self.find_agent(agent_name).ok_or_else(|| {
            let available = self.configs.names();
            anyhow::anyhow!(
                "Unknown sub-agent '{}'. Available: {}",
                agent_name,
                available.join(", ")
            )
        })?;
        let config = &agent.config;

        // Recursion depth guard (RFC §6): reject synchronously at the tool
        // layer BEFORE registering the pending task — a rejected delegation
        // must not leave a hanging pending task / suspension behind. The
        // same check inside `delegate_with_parent` (async task body) is a
        // redundant backstop.
        self.check_depth(parent_session_id)?;

        // Create the sub-session FIRST so the sub-agent's identity (its
        // session FQID) exists before anything is registered — it is the
        // addressing key for agent_list / agent_kill / send_message /
        // pending-task matching / checkpoints. The returned id is what the
        // parent tool call (`agent_delegate` async mode) receives.
        let (sub_ctx, sub_session_id) = self
            .session_manager
            .create_sub_session_context(parent_session_id, &config.name)?;
        // issue #260: keep a registry-owned Arc — sub_ctx itself is moved
        // into the delegation task below, but notice routing must be able to
        // find the live (possibly contract-W parked) instance while the
        // delegation is in flight.
        let sub_ctx_for_registry = std::sync::Arc::clone(&sub_ctx);

        // 方案 C (docs/turn-suspension-rfc.md): register the task against the
        // parent's registered SessionContext so the running turn knows to
        // suspend when the LLM ends it. Only the orchestrator's active
        // top-level sessions are registered; sub-agent sessions and
        // switched-away sessions return None and simply don't suspend.
        if let Some(parent_ctx) = self
            .session_manager
            .registered_context_by_session_id(parent_session_id)
        {
            parent_ctx.add_pending_task(sub_session_id.clone());
        }

        tracing::info!(
            agent = %config.name,
            sub_session_id = %sub_session_id,
            task_len = task.len(),
            timeout_secs,
            "spawning sub-agent in background"
        );

        let sub_delegator = self.clone();
        let task_owned = task.to_string();
        let parent_session_id_owned = parent_session_id.to_string();
        let workspace_owned = workspace.map(|s| s.to_string());
        let event_tx = self.event_sender();
        let sub_session_id_clone = sub_session_id.clone();
        let agent_name_owned = agent_name.to_string();
        let running = Arc::clone(&self.running);
        let running_sub_session_id = sub_session_id.clone();
        let allowed_tools_entry = allowed_tools.clone();

        // RFC agent-messaging §3: register the sub-agent's inbox so the
        // parent's `send_message(recipient=sub_session_id)` can reach it.
        // The sender is dropped when the map entry is removed at completion.
        let (mail_tx, mail_rx) = mpsc::channel(SUB_AGENT_INBOX_CAPACITY);
        let mailbox = SubAgentMailbox {
            tx: mail_tx.clone(),
            rx: tokio::sync::Mutex::new(mail_rx),
        };
        self.mailboxes.insert(sub_session_id.clone(), mail_tx);
        let mailboxes = Arc::clone(&self.mailboxes);

        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            // ── Two-layer spawn (panic safety net) ──────────────────────
            //
            // The sub-agent body runs in an *inner* `tokio::spawn` so a panic
            // is surfaced as a `JoinError` (is_panic) instead of propagating
            // uncaught through the outer task. Without this, a panic skips
            // the running-table cleanup, event dispatch, and checkpoint
            // settlement below — the parent agent hangs on its terminal event
            // forever.
            //
            // The inner handle returns `anyhow::Result<String>`; the outer
            // converts `JoinError` outcomes into the same type so the existing
            // collection logic handles them uniformly.
            let sub_delegator_inner = sub_delegator.clone();
            let parent_session_id_inner = parent_session_id_owned.clone();
            let inner_handle: tokio::task::JoinHandle<anyhow::Result<String>> =
                tokio::spawn(async move {
                    sub_delegator_inner
                        .delegate_with_parent(
                            &agent_name_owned,
                            &task_owned,
                            &parent_session_id_inner,
                            sub_ctx,
                            timeout_secs,
                            Some(mailbox),
                            allowed_tools,
                            workspace_owned.as_deref(),
                        )
                        .await
                });

            let result = match inner_handle.await {
                Ok(r) => r,
                Err(je) if je.is_panic() => {
                    let payload = je.into_panic();
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic payload".to_string()
                    };
                    tracing::error!(
                        sub_session_id = %sub_session_id_clone,
                        panic = %msg,
                        "sub-agent task panicked"
                    );
                    Err(anyhow::anyhow!("sub-agent panicked: {}", msg))
                }
                Err(je) if je.is_cancelled() => {
                    // Abort path (cancel / checkpoint_and_cancel_all) already
                    // persisted a checkpoint tombstone. Clean up the running
                    // table and mailbox, then return without emitting an event
                    // — the caller (cancel / shutdown) owns the terminal event.
                    tracing::debug!(
                        sub_session_id = %sub_session_id_clone,
                        "sub-agent task cancelled (abort); cleaning up without event"
                    );
                    running.remove(&running_sub_session_id);
                    mailboxes.remove(&running_sub_session_id);
                    return;
                }
                Err(je) => {
                    tracing::error!(
                        sub_session_id = %sub_session_id_clone,
                        err = %je,
                        "sub-agent task join error"
                    );
                    Err(anyhow::anyhow!("sub-agent task join error: {}", je))
                }
            };

            let duration_secs = start_time.elapsed().as_secs();

            // Classify the outcome: a wall-clock timeout is distinct from a
            // generic failure — the parent must know the task was abandoned
            // mid-flight, not finished-with-error.
            let timed_out_secs = result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<DelegationTimeout>())
                .map(|t| t.secs);

            let parent_session_id_final = parent_session_id_owned.clone();
            // Count of sub → parent messages delivered while running. The
            // parent session has already received them as `DelegationEvent::
            // Message`, so a non-zero count lets `wake` skip the summary in
            // the completion note (de-duplication, ④).
            let sent_message_count = running
                .get(&running_sub_session_id)
                .map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);
            if let Some(tx) = event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        tracing::info!(sub_session_id = %sub_session_id_clone, duration_secs, sent_message_count, "sub-agent completed successfully");
                        if let Err(send_err) = tx
                            .send(DelegationEvent::Completed {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id_final,
                                summary: summary.clone(),
                                duration_secs,
                                sent_message_count,
                            })
                            .await
                        {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(_), Some(secs)) => {
                        tracing::warn!(sub_session_id = %sub_session_id_clone, timeout_secs = secs, duration_secs, "sub-agent timed out");
                        if let Err(send_err) = tx
                            .send(DelegationEvent::TimedOut {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id_final,
                                timeout_secs: secs,
                                duration_secs,
                            })
                            .await
                        {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(e), None) => {
                        tracing::warn!(sub_session_id = %sub_session_id_clone, duration_secs, err = %e, "sub-agent failed");
                        if let Err(send_err) = tx
                            .send(DelegationEvent::Failed {
                                sub_session_id: sub_session_id_clone.clone(),
                                parent_session_id: parent_session_id_final,
                                error: e.to_string(),
                            })
                            .await
                        {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                }
            }

            // Mark the terminal status on the running-table entry (briefly
            // observable by a concurrent `agent_list`), then self-remove.
            let terminal = if timed_out_secs.is_some() {
                DelegationStatus::TimedOut
            } else if result.is_ok() {
                DelegationStatus::Completed
            } else {
                DelegationStatus::Failed
            };
            if let Some(entry) = running.get(&running_sub_session_id) {
                if let Ok(mut status) = entry.status.write() {
                    *status = terminal;
                }
            }
            running.remove(&running_sub_session_id);
            // Drop the mailbox sender: the sub-agent is gone, so
            // `send_message(recipient=sub_session_id)` will now report the
            // session id as unknown instead of queueing into a dead channel.
            mailboxes.remove(&running_sub_session_id);
            // Settle the durable checkpoint — the task reached a terminal
            // state. Completed deletes it (history is complete; nothing
            // resumes on restart). TimedOut / Failed keep it as a tombstone
            // with the terminal status so `run_startup` skips re-running the
            // task whose history is mid-turn.
            if terminal == DelegationStatus::Completed {
                if let Err(e) = sub_delegator
                    .session_manager
                    .backend()
                    .delete_delegation_checkpoint(&running_sub_session_id)
                {
                    tracing::warn!(sub_session_id = %running_sub_session_id, err = %e, "delete delegation checkpoint failed");
                }
            } else {
                sub_delegator.persist_terminal_checkpoint(&running_sub_session_id, terminal);
            }
        });

        self.running.insert(
            sub_session_id.clone(),
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name: agent_name.to_string(),
                parent_session_id: parent_session_id.to_string(),
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(timeout_secs),
                started_at: chrono::Utc::now(),
                allowed_tools: allowed_tools_entry,
                sub_ctx: sub_ctx_for_registry,
            },
        );
        Ok(sub_session_id)
    }

    /// Layer-3 resumability: revive a timed-out delegation with a fresh budget.
    ///
    /// The kill timer ends the *task*, not the *work* — the sub-session
    /// history (and its incomplete-turn state, re-detected on load) survives
    /// on disk, so `run_recovery` continues from the breakpoint. This method:
    /// 1. validates the durable checkpoint is a `timed_out` tombstone (the
    ///    only resumable terminal — Completed checkpoints are deleted, and
    ///    Failed/Cancelled carry no continuable-turn guarantee),
    /// 2. re-arms the parent's suspension `pending` (idempotent) BEFORE
    ///    spawning, so the eventual terminal event records cleanly instead
    ///    of hitting the `Duplicate` drop path (TimedOut was already
    ///    collected once) — and, if the parent turn is still running, it
    ///    suspends awaiting the result like a fresh async delegation,
    /// 3. rewrites the checkpoint (`running`, `started_at = now`, new
    ///    budget) so a crash during the resumed run recovers with the
    ///    *resumed* budget, then drives `recover_async` (the same path as
    ///    daemon-restart recovery).
    pub fn resume_timed_out(
        &self,
        sub_session_id: &str,
        extra_secs: Option<u64>,
    ) -> anyhow::Result<String> {
        use anyhow::Context as _;
        if self.running.contains_key(sub_session_id) {
            anyhow::bail!("sub-agent '{}' is already running", sub_session_id);
        }
        let backend = self.session_manager.backend();
        let cp = backend
            .load_delegation_checkpoint(sub_session_id)
            .with_context(|| format!("no delegation checkpoint for '{}'", sub_session_id))?;
        if cp.status != "timed_out" {
            anyhow::bail!(
                "sub-agent '{}' is not resumable (checkpoint status: '{}'; only timed_out delegations can be resumed)",
                sub_session_id,
                cp.status
            );
        }
        // Issue #111: resume is only reachable after the original budget
        // already ran out, so defaulting to that same budget guarantees a
        // second timeout unless the first one was a fluke. Default to
        // double the original (floor 600s) instead — still overridable via
        // `extra_secs`, but no longer self-defeating on its core use case.
        let requested = extra_secs.unwrap_or_else(|| cp.timeout_secs.saturating_mul(2).max(600));
        let budget = requested.clamp(1, SUB_AGENT_TIMEOUT_MAX_SECS);

        // Re-arm the parent side. Registered (active) context first; the
        // fallback load covers a parent that switched away or whose turn
        // already ended — `add_pending_task` re-creates the suspension in
        // that case, keeping the parent turn-suspended awaiting the result.
        let parent_ctx = self
            .session_manager
            .registered_context_by_session_id(&cp.parent_session_id)
            .or_else(|| {
                self.session_manager
                    .load_context_by_session_id(&cp.parent_session_id)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "parent session '{}' not found for sub-agent '{}'",
                    cp.parent_session_id,
                    sub_session_id
                )
            })?;
        parent_ctx.add_pending_task(sub_session_id.to_string());

        // Sub-sessions are never registered — always a fresh load (same as
        // daemon-restart recovery). Loading re-detects the incomplete turn
        // from the history tail, which `run_recovery` continues from.
        let sub_ctx = self
            .session_manager
            .load_context_by_session_id(sub_session_id)
            .with_context(|| format!("sub-agent session '{}' not found", sub_session_id))?;

        let resumed = crate::storage::DelegationCheckpoint {
            parent_session_id: cp.parent_session_id.clone(),
            sub_session_id: sub_session_id.to_string(),
            agent_name: cp.agent_name.clone(),
            status: "running".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: budget,
            allowed_tools: cp.allowed_tools.clone(),
            last_checkpoint: None,
        };
        if let Err(e) = backend.save_delegation_checkpoint(&resumed) {
            tracing::warn!(sub_session_id, err = %e, "persist resumed delegation checkpoint failed");
        }
        tracing::info!(
            sub_session_id,
            budget_secs = budget,
            "resuming timed-out delegation with fresh budget"
        );

        self.recover_async(
            sub_session_id.to_string(),
            cp.agent_name.clone(),
            cp.parent_session_id.clone(),
            sub_ctx,
            budget,
            cp.allowed_tools.clone(),
        );
        Ok(sub_session_id.to_string())
    }

    pub fn recover_async(
        &self,
        sub_session_id: String,
        agent_name: String,
        parent_session_id: String,
        sub_ctx: Arc<SessionContext>,
        timeout_secs: u64,
        allowed_tools: Option<Vec<String>>,
    ) {
        let (mail_tx, mail_rx) = mpsc::channel(SUB_AGENT_INBOX_CAPACITY);
        let mailbox = SubAgentMailbox {
            tx: mail_tx.clone(),
            rx: tokio::sync::Mutex::new(mail_rx),
        };
        self.mailboxes.insert(sub_session_id.clone(), mail_tx);
        let mailboxes = Arc::clone(&self.mailboxes);

        let running = Arc::clone(&self.running);
        let event_tx = self.event_sender();
        let running_sub_session_id = sub_session_id.clone();
        let sub_session_id_clone = sub_session_id.clone();
        let parent_session_id_final = parent_session_id.clone();
        let backend = Arc::clone(self.session_manager.backend());

        let runtime = match self.runtime() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("recover_async failed to get runtime: {}", e);
                return;
            }
        };

        let agent_name_clone = agent_name.clone();
        let allowed_tools_entry = allowed_tools.clone();
        let sub_ctx_for_registry = std::sync::Arc::clone(&sub_ctx);
        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            {
                let mut session = sub_ctx.session.lock().await;
                session.sub_agent_inbox = Some(Arc::new(mailbox));
                session.turn_tool_allowlist = allowed_tools;
                // Time-awareness on recovery: same budget as the kill timer
                // below (the caller already discounted time spent before the
                // restart when computing `timeout_secs`), so the countdown
                // the sub-agent sees matches the actual remaining budget.
                session.delegation_deadline =
                    Some(crate::agents::session::DelegationDeadline::from_now(timeout_secs));
            }

            let turn_future = async {
                let mut turn_guard_slot = Some(Arc::clone(&sub_ctx.turn_lock).lock_owned().await);
                let mut session_slot = Some(Arc::clone(&sub_ctx.session).lock_owned().await);
                let resolved = crate::agents::orchestrator::turn::ResolvedTurn::resolve(&session, &runtime);
                let turn_ctx = resolved.turn_context();
                let recovered = sub_ctx
                    .agent
                    .run_recovery(&mut session_slot, &mut turn_guard_slot, turn_ctx, &runtime)
                    .await;
                match recovered {
                    // issue #251: `run_recovery` returning `Ok(None)` means
                    // "no incomplete turn to continue" — which also covers
                    // the case where the sub-agent completed naturally
                    // between the original timeout and this resume call, so
                    // its history already ends with a genuine final answer.
                    // Surface that existing result instead of letting the
                    // caller below misread a legitimate no-op as a failure
                    // (previously: a resume on an already-completed
                    // delegation always produced a spurious
                    // `DelegationEvent::Failed`, which then hit #252's
                    // stale-repeat-call dedup and vanished, both losing the
                    // real result and orphaning the parent's pending entry).
                    Ok(None) => Ok(existing_final_text(&session)),
                    other => other,
                }
            };

            let result = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), turn_future).await {
                Ok(Ok(Some(tr))) if !tr.text.is_empty() => Ok(tr.text),
                Ok(Ok(_)) => Err(anyhow::anyhow!("no recovery needed or empty text")),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow::anyhow!(DelegationTimeout { agent: agent_name_clone, secs: timeout_secs })),
            };

            let duration_secs = start_time.elapsed().as_secs();
            let timed_out_secs = result.as_ref().err().and_then(|e| e.downcast_ref::<DelegationTimeout>()).map(|t| t.secs);
            let sent_message_count = running.get(&running_sub_session_id).map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);

            if let Some(tx) = event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        if let Err(send_err) = tx.send(DelegationEvent::Completed { sub_session_id: sub_session_id_clone.clone(), parent_session_id: parent_session_id_final, summary: summary.clone(), duration_secs, sent_message_count }).await {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(_), Some(secs)) => {
                        if let Err(send_err) = tx.send(DelegationEvent::TimedOut { sub_session_id: sub_session_id_clone.clone(), parent_session_id: parent_session_id_final, timeout_secs: secs, duration_secs }).await {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                    (Err(e), None) => {
                        if let Err(send_err) = tx.send(DelegationEvent::Failed { sub_session_id: sub_session_id_clone.clone(), parent_session_id: parent_session_id_final, error: e.to_string() }).await {
                            tracing::warn!(sub_session_id = %sub_session_id_clone, err = %send_err, "terminal event channel closed; parent may hang until restart");
                        }
                    }
                }
            }

            let terminal = if timed_out_secs.is_some() { DelegationStatus::TimedOut } else if result.is_ok() { DelegationStatus::Completed } else { DelegationStatus::Failed };
            if let Some(entry) = running.get(&running_sub_session_id) {
                if let Ok(mut status) = entry.status.write() { *status = terminal; }
            }
            running.remove(&running_sub_session_id);
            mailboxes.remove(&running_sub_session_id);
            // Settle the durable checkpoint — the recovered task reached a
            // terminal state. Completed deletes it (its recovery finished and
            // the history is now complete). TimedOut / Failed keep it as a
            // tombstone so a SECOND restart does not re-run the recovery.
            if terminal == DelegationStatus::Completed {
                if let Err(e) = backend.delete_delegation_checkpoint(&running_sub_session_id) {
                    tracing::warn!(sub_session_id = %running_sub_session_id, err = %e, "delete delegation checkpoint failed");
                }
            } else if let Err(e) = backend.update_delegation_checkpoint_status(
                &running_sub_session_id,
                terminal.as_str(),
            ) {
                tracing::warn!(
                    sub_session_id = %running_sub_session_id,
                    status = %terminal.as_str(),
                    err = %e,
                    "update delegation checkpoint status (tombstone) failed"
                );
            }
        });

        self.running.insert(
            sub_session_id,
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name,
                parent_session_id,
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
                timeout_secs: Some(timeout_secs),
                started_at: chrono::Utc::now(),
                allowed_tools: allowed_tools_entry,
                sub_ctx: sub_ctx_for_registry,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::session::Session;
    use crate::providers::capability_chat::ChatMessage;

    /// issue #251: a session whose history already ends with a plain final
    /// assistant answer (no incomplete turn) must have that answer
    /// extracted — this is exactly what `resume_timed_out` sees when the
    /// sub-agent completed naturally between the original timeout and the
    /// resume call.
    #[test]
    fn existing_final_text_extracts_a_clean_trailing_assistant_message() {
        let mut session = Session::new("s1".to_string());
        session.history.push(ChatMessage::user_text("do the thing"));
        session.history.push(ChatMessage::assistant_text("TIMEOUT-TEST-5c81 all done"));

        let tr = existing_final_text(&session).expect("must extract the trailing answer");
        assert_eq!(tr.text, "TIMEOUT-TEST-5c81 all done");
        assert_eq!(tr.stop_reason, crate::providers::StopReason::EndTurn);
        assert!(!tr.has_pending);
    }

    /// An assistant message with unresolved tool_calls is NOT a clean final
    /// answer — `run_recovery` would have caught this as Case A (returning
    /// `Some`, never reaching `existing_final_text` at all), but the helper
    /// itself must still refuse to fabricate a result for this shape.
    #[test]
    fn existing_final_text_refuses_a_trailing_tool_call() {
        let mut session = Session::new("s2".to_string());
        session.history.push(ChatMessage::assistant_text(""));
        session.history.last_mut().unwrap().tool_calls = Some(vec![crate::providers::ToolCall {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: "{}".to_string(),
        }]);

        assert!(existing_final_text(&session).is_none());
    }

    /// Some `Session` helpers stamp `tool_calls: Some(vec![])` for a
    /// no-tool-call assistant message rather than `None` — must be treated
    /// identically to `None` (matches `run_recovery`'s own Case A check,
    /// which iterates an empty `Some` as a no-op).
    #[test]
    fn existing_final_text_accepts_an_empty_some_tool_calls() {
        let mut session = Session::new("s6".to_string());
        session.history.push(ChatMessage::assistant_text("done, empty Some"));
        session.history.last_mut().unwrap().tool_calls = Some(vec![]);

        let tr = existing_final_text(&session).expect("empty Some(tool_calls) must not block extraction");
        assert_eq!(tr.text, "done, empty Some");
    }

    #[test]
    fn existing_final_text_none_for_empty_history() {
        let session = Session::new("s3".to_string());
        assert!(existing_final_text(&session).is_none());
    }

    #[test]
    fn existing_final_text_none_when_trailing_message_is_not_assistant() {
        let mut session = Session::new("s4".to_string());
        session.history.push(ChatMessage::user_text("hello"));
        assert!(existing_final_text(&session).is_none());
    }

    #[test]
    fn existing_final_text_none_for_empty_assistant_text() {
        let mut session = Session::new("s5".to_string());
        session.history.push(ChatMessage::assistant_text(""));
        assert!(existing_final_text(&session).is_none());
    }

    /// Review finding (PR #253): a whitespace-only trailing message must be
    /// treated the same as empty — `text.is_empty()` alone would let it
    /// through and deliver a whitespace-only "completion".
    #[test]
    fn existing_final_text_none_for_whitespace_only_assistant_text() {
        let mut session = Session::new("s7".to_string());
        session.history.push(ChatMessage::assistant_text("   \n\t "));
        assert!(existing_final_text(&session).is_none());
    }

    /// Surrounding whitespace on a genuine answer is trimmed, not treated
    /// as part of the delivered result.
    #[test]
    fn existing_final_text_trims_surrounding_whitespace() {
        let mut session = Session::new("s8".to_string());
        session.history.push(ChatMessage::assistant_text("  done  \n"));
        let tr = existing_final_text(&session).expect("non-whitespace content must still extract");
        assert_eq!(tr.text, "done");
    }
}
