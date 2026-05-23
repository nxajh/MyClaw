//! `SessionContext` — bundles a `Session` with everything the per-turn
//! pipeline needs.
//!
//! RFC v2 §三.A: introduces SessionContext as the boundary that owns
//! per-session mutable state (attachments, pending retry, turn lock) and
//! triggers `process_turn`. `Agent.run` takes `&mut Session` plus a
//! `TurnContext` (already-resolved decisions); SessionContext is what
//! the Orchestrator actually holds in its session table.
//!
//! This struct is intentionally additive in this commit — it is not yet
//! wired into the Orchestrator or AgentLoop. C18 (Agent.run rewrite) and
//! E29 (Orchestrator main loop) will start consuming it. Keeping the
//! scaffold here so downstream commits don't have to introduce both the
//! type and its callers in one go.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agents::attachment::AttachmentManager;
use crate::agents::session::Session;

/// Per-session bundle held by the Orchestrator's session table.
///
/// Fields:
/// - `session`: owns the conversation state (Session — history, override, …)
/// - `attachments`: per-session AttachmentManager (file uploads pending injection)
/// - `pending_retry`: user message saved when last turn ended abnormally;
///   surfaced as a "retry?" prompt next time the user types
/// - `turn_lock`: tokio Mutex held for the duration of `process_turn`,
///   ensuring two messages on the same session do not race the LLM
///
/// `user_profile` is added in G41 once UserProfile lands; left out here
/// to avoid coupling B13 to G40.
pub struct SessionContext {
    /// Mutable session state. Wrapped in Mutex so the turn lock and the
    /// Session itself share the same critical section.
    pub session: Arc<Mutex<Session>>,
    /// Attachments awaiting injection on the next user turn.
    pub attachments: Arc<AttachmentManager>,
    /// User message saved when the previous turn ended with an empty LLM
    /// response or interrupted streaming. Cleared once retried.
    pub pending_retry: Arc<Mutex<Option<String>>>,
    /// Serializes `process_turn` per session. Distinct from `session`'s
    /// Mutex because some readers want to peek at session state without
    /// blocking on an in-flight turn.
    pub turn_lock: Arc<Mutex<()>>,
}

impl SessionContext {
    pub fn new(session: Session) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            attachments: Arc::new(AttachmentManager::new()),
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Snapshot the session for read-only consumers (e.g., /status commands).
    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }
}
