//! Async delegation — event types and manager for background sub-agent execution.
//!
//! When `agent_delegate` is called with mode="async", the sub-agent is spawned in a background
//! tokio task. When it completes (or fails), a `DelegationEvent` is sent via
//! mpsc channel to the Orchestrator, which wakes the main agent by injecting
//! a synthetic message.

use std::sync::Arc;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Events sent from background sub-agents to the Orchestrator.
#[derive(Debug, Clone)]
pub enum DelegationEvent {
    /// Sub-agent completed successfully.
    Completed {
        task_id: String,
        session_key: String,
        reply_target: String,
        summary: String,
        /// How long the sub-agent ran (in seconds).
        duration_secs: u64,
    },
    /// Sub-agent failed.
    Failed {
        task_id: String,
        session_key: String,
        reply_target: String,
        error: String,
    },
}

/// Manages background delegation tasks.
pub struct DelegationManager {
    /// Running background tasks: task_id → JoinHandle.
    running: Arc<DashMap<String, JoinHandle<()>>>,
    /// Event sender — cloned and given to sub-agent spawns.
    event_tx: mpsc::Sender<DelegationEvent>,
}

impl DelegationManager {
    pub fn new(event_tx: mpsc::Sender<DelegationEvent>) -> Self {
        Self {
            running: Arc::new(DashMap::new()),
            event_tx,
        }
    }

    /// Get a clone of the event sender (for passing to sub-agent spawns).
    pub fn event_sender(&self) -> mpsc::Sender<DelegationEvent> {
        self.event_tx.clone()
    }

    /// Register a running task.
    pub fn register(&self, task_id: String, handle: JoinHandle<()>) {
        self.running.insert(task_id, handle);
    }

    /// Cancel a running task.
    #[allow(dead_code)]
    pub fn cancel(&self, task_id: &str) -> bool {
        if let Some((_, handle)) = self.running.remove(task_id) {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// Get a snapshot of running task_ids.
    pub fn running_snapshot(&self) -> Vec<String> {
        self.running.iter().map(|e| e.key().clone()).collect()
    }

    /// Get agent names for running tasks (parallel vector, same length as running_snapshot).
    /// Returns empty vec if no task info stored.
    pub fn running_agent_names(&self) -> Vec<String> {
        // Tasks registered with delegate_async don't store agent name in the current design.
        // Return empty vec — caller can match with running_snapshot by index.
        Vec::new()
    }
}
