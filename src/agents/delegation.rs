//! Async delegation — event types and manager for background sub-agent execution.
//!
//! When `delegate_task` is called, the sub-agent is spawned in a background
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
}
