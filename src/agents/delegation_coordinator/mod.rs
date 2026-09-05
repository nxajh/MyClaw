//! `DelegationCoordinator` — creates and runs specialized sub-agents on demand.
//!
//! RFC v2 §三.D: implements [`AgentDelegator`](crate::agents::AgentDelegator) by
//! running `Agent::run` invocations with restricted tool sets and
//! specialized system prompts. Also provides `delegate_async` for non-blocking
//! background execution.
//!
//! ## History persistence
//!
//! When a `parent_session_id` is supplied each sub-agent invocation gets its
//! own `JsonFileBackend` rooted at:
//!
//! ```text
//! sessions/{parent_session_id}/subagents/
//!   {sub_session_id}/
//!     meta.json
//!     history.jsonl
//!     ...          ← same structure as a top-level session, incl. compaction
//! ```
//!
//! Sub-agents therefore support context compaction and rotation identically
//! to the parent agent.  If storage cannot be opened the sub-agent runs
//! ephemerally (no history is saved).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;

pub(crate) mod checkpoint;
pub(crate) mod worktree;
mod agent_lifecycle;
mod delegate;
mod delegator;
mod lifecycle;
mod messenger;
mod registry;

use tokio::sync::mpsc;

use crate::agents::delegation::{AgentMail, DelegationEvent, DelegationStatus};
use crate::agents::session::SessionManager;
use crate::agents::session_context::SessionContext;

use registry::RunningEntry;

pub use crate::api::delegation::{RunningAgentInfo, SUB_AGENT_TIMEOUT_MAX_SECS};

/// Default wall-clock timeout for a sub-agent when neither the tool caller
/// nor the SubAgentConfig specifies one. Sized to cover ~2 CI rounds
/// (5-7 min each) plus margin — shorter defaults starve code-modifying
/// sub-agents on CI-only hosts.
const SUB_AGENT_TIMEOUT_DEFAULT_SECS: u64 = 1200;

/// Holds sub-agent configs and creates temporary `Agent::run` invocations
/// for delegation.
///
/// Implements [`AgentDelegator`](crate::agents::AgentDelegator); the legacy
/// `TaskDelegator` dual-impl was removed in H47 (callers now go through
/// `AgentDelegator`, which carries `&Session` so per-user context is
/// available end-to-end).
#[derive(Clone)]
pub struct DelegationCoordinator {
    /// Sub-agent configurations, indexed by name. Same Arc as
    /// `AgentRuntime.agents` so name → Agent lookups stay consistent.
    configs: Arc<super::AgentRegistry>,
    /// Shared SessionManager. Sub-sessions are flat peers of regular
    /// sessions (`meta.parent_session_id` is the link).
    session_manager: Arc<SessionManager>,
    /// Root directory for git worktrees (when isolation = worktree).
    worktrees_root: PathBuf,
    /// Parent AgentRuntime, set exactly once by the daemon after both this
    /// coordinator and the runtime have been built (they form a
    /// construction cycle: runtime → tools → DelegateTaskTool →
    /// coordinator → runtime). `delegate` reads the runtime from here and
    /// passes it (with workspace_dir overlaid when worktree-isolated) to
    /// `SessionContext::process_turn`. `OnceLock` encodes the set-once
    /// contract and gives lock-free reads on the hot delegate path.
    runtime_cell: Arc<std::sync::OnceLock<crate::agents::runtime::AgentRuntime>>,
    /// In-flight background delegations (sub_session_id → entry). Powers
    /// `/agent_list` (read snapshot with live status) and `/agent_kill`
    /// (abort by session id).
    running: Arc<DashMap<String, RunningEntry>>,
    /// Parent → sub inboxes of running async sub-agents
    /// (sub_session_id → sender). Registered by `spawn_delegate_async`,
    /// removed when the sub-agent finishes; powers the
    /// `send_message(recipient=sub_session_id)` tool via
    /// `AgentMessenger::send_to_sub_agent`.
    mailboxes: Arc<DashMap<String, mpsc::Sender<AgentMail>>>,
    /// Namespace for generated FQIDs (`<ns>/s/<uuidv7>` session ids). Bound
    /// at construction from `[system] namespace`.
    namespace: String,
    /// Maximum delegation depth, counting the main agent as depth 1
    /// (RFC §6, `[delegation] max_depth`). A delegation whose child depth
    /// would exceed this limit is rejected at the tool layer before any
    /// pending task is registered or suspension created.
    max_depth: u32,
    /// Sender for `DelegationEvent`s, set once by the daemon via
    /// `set_event_sender` when wiring the orchestrator's `delegation_rx`.
    event_tx_cell: Arc<OnceLock<mpsc::Sender<DelegationEvent>>>,
    /// Sub → parent final messages recorded for SYNC delegations
    /// (sub_session_id → texts). `send_to_parent` skips the broadcast for
    /// sync sub-agents (no running-table entry — the parent is blocked
    /// inside the tool call) and records the text here instead; the sync
    /// completion branch attaches it to the tool result so the parent gets
    /// the full report immediately instead of via the delayed notice
    /// channel. Removed on read. (2026-08-14, sync notice B)
    sent_messages: Arc<DashMap<String, std::sync::RwLock<Vec<String>>>>,
    /// Shared with `ShellTool`/`ShellPollTool`/`ShellKillTool` — used to kill
    /// any shell processes a sub-agent's session still has running when its
    /// delegation ends (timeout or `agent_kill`). See
    /// `crate::tools::shell::kill_processes_for_session` for why this is
    /// necessary: cancelling the delegation's future does not reach the
    /// detached tasks tracking those processes.
    shell_registry: crate::tools::shell::ShellRegistry,
}

impl DelegationCoordinator {
    /// issue #260: the live sub-session context for an in-flight delegation,
    /// if any. Contract W (issue #256) means the sub-agent's turn may be
    /// physically parked in `park_for_yield`; this accessor is how delegation
    /// notice routing reaches the parked instance instead of silently
    /// rebuilding a second one from disk.
    pub fn live_sub_context(&self, sub_session_id: &str) -> Option<Arc<SessionContext>> {
        self.running
            .get(sub_session_id)
            .map(|entry| Arc::clone(&entry.sub_ctx))
    }

    /// issue #260: reconcile a delegation whose sub-session just completed a
    /// notice turn outside the normal completion flow (route_notice's
    /// non-active fallback). Without this the in-flight entry lingers until
    /// the wall-clock timer fires, and the parent is woken with a spurious
    /// "timed out, use agent_resume" even though the work is already done.
    ///
    /// No-op when `sub_session_id` has no in-flight entry (the normal
    /// completion path already removed it — at-most-once semantics).
    pub fn reconcile_notice_completed(&self, sub_session_id: &str, summary: &str) {
        let Some((_, entry)) = self.running.remove(sub_session_id) else {
            return;
        };
        let parent_session_id = entry.parent_session_id.clone();
        let duration_secs = entry.spawned_at.elapsed().as_secs();
        let sent_message_count = entry
            .messages_sent
            .load(std::sync::atomic::Ordering::Relaxed);
        self.mailboxes.remove(sub_session_id);
        tracing::info!(
            sub_session_id = %sub_session_id,
            duration_secs,
            "issue #260: delegation reconciled as completed by notice fallback turn"
        );
        if let Some(tx) = self.event_sender() {
            let sub_session_id = sub_session_id.to_string();
            let summary = summary.to_string();
            tokio::spawn(async move {
                let _ = tx
                    .send(DelegationEvent::Completed {
                        sub_session_id,
                        parent_session_id,
                        summary,
                        duration_secs,
                        sent_message_count,
                    })
                    .await;
            });
        }
    }
}

/// P1-4: DelegationCoordinator unit tests — depth gating, async spawn
/// registration, timeout resolution, and cancel-broadcast semantics. Tests
/// reach private internals (`running` table, `check_depth`, `session_depth`,
/// `resolve_timeout`) as descendants of this module.
#[cfg(test)]
mod tests;
