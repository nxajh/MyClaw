use super::{DelegationCoordinator, RunningEntry, SUB_AGENT_TIMEOUT_DEFAULT_SECS, DelegationStatus};

impl DelegationCoordinator {
    /// Durable checkpoints currently in `timed_out` status — the only ones
    /// `resume_timed_out` will actually accept (issue #134 P2). Backs the
    /// listing appended to `agent_resume`'s not-found error; unscoped by
    /// parent session, same convention as `running_records`/`agent_list`.
    pub fn timed_out_checkpoints(&self) -> Vec<crate::storage::DelegationCheckpoint> {
        self.session_manager
            .backend()
            .load_delegation_checkpoints()
            .into_iter()
            .filter(|cp| cp.status == "timed_out")
            .collect()
    }

    /// Checkpoint all running tasks to durable storage and cancel them.
    ///
    /// Called during daemon shutdown (before hot-switch fork or process exit).
    /// Each running task's checkpoint is updated to `status: "checkpointed"`
    /// so startup recovery knows the task was interrupted by shutdown, not a
    /// business failure. The tokio tasks are then aborted — their sub-session
    /// history is already persisted, so `scan_unfinished_subagents` will
    /// resume them on restart.
    ///
    /// Unlike `drain`, this does NOT wait for tasks to finish — it checkpoints
    /// and immediately aborts. The drain timeout is therefore not a business
    /// failure.
    pub fn checkpoint_and_cancel_all(&self) {
        let backend = self.session_manager.backend();
        let existing: Vec<crate::storage::DelegationCheckpoint> = backend.load_delegation_checkpoints();

        // Take ownership of all entries by removing from the map (same pattern
        // as `drain`). Each entry carries the JoinHandle we need to abort.
        let entries: Vec<(String, RunningEntry)> = self
            .running
            .iter()
            .map(|e| e.key().clone())
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|id| self.running.remove(&id).map(|(_, v)| (id, v)))
            .collect();

        for (sub_session_id, entry) in &entries {
            let checkpoint = existing
                .iter()
                .find(|c| &c.sub_session_id == sub_session_id)
                .cloned()
                .map(|mut c| {
                    c.status = "checkpointed".to_string();
                    c.last_checkpoint = Some(chrono::Utc::now());
                    c
                })
                .unwrap_or_else(|| crate::storage::DelegationCheckpoint {
                    parent_session_id: entry.parent_session_id.clone(),
                    sub_session_id: sub_session_id.clone(),
                    agent_name: entry.agent_name.clone(),
                    status: "checkpointed".to_string(),
                    started_at: entry.started_at,
                    timeout_secs: entry.timeout_secs.unwrap_or(SUB_AGENT_TIMEOUT_DEFAULT_SECS),
                    allowed_tools: entry.allowed_tools.clone(),
                    last_checkpoint: Some(chrono::Utc::now()),
                });
            if let Err(e) = backend.save_delegation_checkpoint(&checkpoint) {
                tracing::warn!(sub_session_id = %sub_session_id, err = %e, "shutdown checkpoint failed");
            }
        }

        // Abort all running tasks. The sub-session history is already on disk;
        // startup recovery will resume via `scan_unfinished_subagents`.
        for (_, entry) in &entries {
            entry.handle.abort();
        }
        if !entries.is_empty() {
            tracing::info!(count = entries.len(), "checkpointed and cancelled running sub-agents");
        }
    }

    /// Load all durable delegation checkpoints from the backend.
    ///
    /// Called at daemon startup to distinguish tasks interrupted by shutdown
    /// (checkpointed → resumable) from tasks that crashed without a checkpoint
    /// (potentially failed).
    pub fn load_checkpoints(&self) -> Vec<crate::storage::DelegationCheckpoint> {
        self.session_manager.backend().load_delegation_checkpoints()
    }

    /// 方案 A tombstone: terminal cleanup rewrites the durable checkpoint's
    /// status (instead of deleting it) so a restart can tell "already
    /// finished, do not resume" from "crash remnant, resume". Only a
    /// *Completed* terminal state deletes the checkpoint — its history is
    /// complete and never triggers resume. Missing checkpoints are a no-op
    /// (idempotent; e.g. the crash happened before spawn finished).
    pub(crate) fn persist_terminal_checkpoint(&self, sub_session_id: &str, status: DelegationStatus) {
        if let Err(e) = self
            .session_manager
            .backend()
            .update_delegation_checkpoint_status(sub_session_id, status.as_str())
        {
            tracing::warn!(
                sub_session_id = %sub_session_id,
                status = %status.as_str(),
                err = %e,
                "update delegation checkpoint status (tombstone) failed"
            );
        }
    }
}
