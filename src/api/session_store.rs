//! session_store — L0 facade over `agents::session::SessionManager`.
//!
//! #151 Phase 8+: `shell` (L3) touches the session manager for exactly
//! one thing — registering a notify-armed process as pending async work
//! (issue #140, the same mechanism `agent_delegate(mode="async")` uses).
//! This trait is that one-method surface, implemented by `SessionManager`
//! (and `SessionContext` for `PendingWorkSession`) in the agents layer.
//! The composition root keeps passing the concrete `Arc` unchanged.

use std::sync::Arc;

/// Lookup of live session contexts by session id.
pub trait SessionStore: Send + Sync {
    fn registered_context_by_session_id(
        &self,
        session_id: &str,
    ) -> Option<Arc<dyn PendingWorkSession>>;
}

/// A live session context that accepts pending-async-work registrations.
pub trait PendingWorkSession: Send + Sync {
    /// Register `task_id` (e.g. a shell `process_id`) as pending async
    /// work, armed for the session's suspension snapshot.
    fn add_pending_task(&self, task_id: String);
}

/// In-memory double for tests and bare-CLI scenarios — one context per
/// routing key, mirroring `SessionManager::get_or_create_context`'s
/// shape closely enough for the shell pending-work tests.
pub struct InMemorySessionStore {
    context: std::sync::Mutex<Option<Arc<InMemorySessionContext>>>,
}

pub struct InMemorySessionContext {
    pub session_id: String,
    pending: std::sync::Mutex<Vec<String>>,
}

/// Snapshot shape mirroring `TurnSuspension`'s pending list.
pub struct InMemorySuspension {
    pub pending: Vec<String>,
}

impl InMemorySessionContext {
    pub fn has_pending_async_work(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    pub fn suspension_snapshot(&self) -> Option<InMemorySuspension> {
        let pending = self.pending.lock().unwrap();
        if pending.is_empty() {
            None
        } else {
            Some(InMemorySuspension {
                pending: pending.clone(),
            })
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            context: std::sync::Mutex::new(None),
        }
    }

    /// Create (once) the context for `routing_key`; its `session_id` is
    /// derived from the key so tests can thread it through `ToolContext`.
    pub fn get_or_create_context(&self, routing_key: &str) -> Arc<InMemorySessionContext> {
        let mut guard = self.context.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            return Arc::clone(existing);
        }
        let ctx = Arc::new(InMemorySessionContext {
            session_id: format!("session-{routing_key}"),
            pending: std::sync::Mutex::new(Vec::new()),
        });
        *guard = Some(Arc::clone(&ctx));
        ctx
    }
}

impl SessionStore for InMemorySessionStore {
    fn registered_context_by_session_id(
        &self,
        session_id: &str,
    ) -> Option<Arc<dyn PendingWorkSession>> {
        let guard = self.context.lock().unwrap();
        guard
            .as_ref()
            .filter(|c| c.session_id == session_id)
            .map(|c| Arc::clone(c) as Arc<dyn PendingWorkSession>)
    }
}

impl PendingWorkSession for InMemorySessionContext {
    fn add_pending_task(&self, task_id: String) {
        self.pending.lock().unwrap().push(task_id);
    }
}
