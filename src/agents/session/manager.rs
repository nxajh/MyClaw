//! SessionManager — creates, retrieves, and persists sessions.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

/// Returned by `switch_session` when the caller tries to point a routing_key
/// at a session that doesn't belong to it.
///
/// Per RFC v2: cross-channel session takeover is not supported — each
/// routing_key has its own session pool and may only switch among sessions
/// it owns. UI should display a friendly error and offer to create a new
/// session in this channel instead.
#[derive(Debug, Clone)]
pub struct SessionNotOwned {
    pub session_id: String,
    pub routing_key: String,
}

impl fmt::Display for SessionNotOwned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "session '{}' is not owned by routing_key '{}'",
            self.session_id, self.routing_key
        )
    }
}

impl std::error::Error for SessionNotOwned {}


use parking_lot::RwLock;

use crate::providers::capability_chat::ChatMessage;
use crate::storage::{SessionBackend, SessionInfo};

use super::backend::InMemoryBackend;
use super::recovery::{identify_breakpoint, BreakpointItem};
use super::session_override::sanitize_paired;
use super::types::{Session, SummaryMetadata};
use super::session_override::SessionOverride;

/// Manages session lifecycle — creates, retrieves, and persists sessions.
pub struct SessionManager {
    backend: Arc<dyn SessionBackend>,
    /// In-memory session cache: session_id → Session.
    cache: RwLock<HashMap<String, Session>>,
    /// User's active session: user_id → session_id.
    active: RwLock<HashMap<String, String>>,
}

impl SessionManager {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self {
            backend,
            cache: RwLock::new(HashMap::new()),
            active: RwLock::new(HashMap::new()),
        }
    }

    pub fn in_memory() -> Self {
        Self::new(Arc::new(InMemoryBackend::new()))
    }

    /// Get the active session for a user. Auto-creates if none exists.
    /// Attempts summary-based recovery first, then falls back to full load.
    pub fn get_or_create(&self, user_id: &str) -> Session {
        // 1. Resolve active session_id.
        let session_id = self.resolve_active(user_id);

        // 2. Check cache.
        {
            let cache = self.cache.read();
            if let Some(s) = cache.get(&session_id) {
                return s.clone();
            }
        }

        // 3. Load from backend.
        let last_total_tokens = self.backend.load_token_count(&session_id);
        let session_override = self.backend.load_session_override(&session_id)
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let (summary_meta, compact_ver) = match self.backend.load_latest_summary(&session_id) {
            Some(summary) => {
                let meta = SummaryMetadata {
                    version: summary.version,
                    token_estimate: summary.token_estimate.unwrap_or(0),
                    up_to_message: summary.up_to_message,
                };
                (Some(meta), summary.version)
            }
            None => (None, 0),
        };
        let from_compacted = summary_meta.is_some();

        let rows = self.backend.load_incremental(&session_id, 0);
        let count = rows.len();

        // Detect breakpoints on raw (pre-sanitization) messages so we can
        // decide whether to preserve the trailing assistant tool_calls for
        // recovery rather than trimming them away.
        let raw_msgs: Vec<ChatMessage> = rows.iter().map(|(_, m)| m.clone()).collect();
        let breakpoints = identify_breakpoint(&raw_msgs);

        let (ids, msgs, breakpoints): (Vec<i64>, Vec<ChatMessage>, Vec<BreakpointItem>) = if !breakpoints.is_empty() {
            // Breakpoint mode: only remove orphan tool results, but keep the
            // trailing assistant message with tool_calls so the model can
            // re-execute the interrupted tools.
            let known_tool_ids: HashSet<String> = rows
                .iter()
                .filter(|(_, m)| m.role == "assistant")
                .flat_map(|(_, m)| m.tool_calls.iter().flatten().map(|tc| tc.id.clone()))
                .collect();
            let filtered: Vec<_> = rows
                .into_iter()
                .filter(|(_, msg)| {
                    if msg.role == "tool" {
                        return msg
                            .tool_call_id
                            .as_ref()
                            .is_some_and(|id| known_tool_ids.contains(id));
                    }
                    true
                })
                .collect();
            let (i, m): (Vec<i64>, Vec<_>) = filtered.into_iter().unzip();
            tracing::warn!(
                session = %session_id,
                breakpoint_count = breakpoints.len(),
                "detected breakpoint: tool calls without results, preserving for recovery"
            );
            (i, m, breakpoints)
        } else {
            let pairs = sanitize_paired(rows);
            let sanitized = pairs.len();
            let (i, m): (Vec<i64>, Vec<_>) = pairs.into_iter().unzip();
            if count > 0 {
                if from_compacted {
                    tracing::info!(
                        session = %session_id,
                        message_count = count,
                        sanitized,
                        last_total_tokens,
                        "session restored from compacted history"
                    );
                } else {
                    tracing::info!(
                        session = %session_id,
                        message_count = count,
                        sanitized,
                        "session restored from full history"
                    );
                }
            }
            (i, m, Vec::new())
        };

        let last_reply_target = self.backend.load_reply_target(&session_id);

        let mut session = Session {
            id: session_id.clone(),
            owner: user_id.to_string(),
            agent_name: self.backend.load_agent_name(&session_id)
                .unwrap_or_else(|| "main".to_string()),
            parent_session_id: self.backend.load_parent_session_id(&session_id),
            history: msgs,
            message_ids: ids,
            compact_version: compact_ver,
            summary_metadata: summary_meta,
            last_total_tokens,
            session_override,
            incomplete_turn: false,
            breakpoint_items: breakpoints,
            last_message: self.backend.load_last_message(&session_id),
            last_reply_target,
        };

        // Breakpoint items are kept for diagnostics, but recovery is now handled
        // automatically by `recover_incomplete_turn` in run.rs — no prompt injection needed.

        // 4. Detect incomplete turn (last message is user without assistant reply).
        //    Only check the most recent turn — earlier orphan user messages are
        //    ignored because compaction or manual cleanup may have removed them.
        if session.history.last().is_some_and(|m| m.role == "user") {
            session.incomplete_turn = true;
            tracing::warn!(session = %session_id, "detected incomplete turn on load");
        }

        // 5. Cache.
        {
            let mut cache = self.cache.write();
            cache.insert(session_id, session.clone());
        }

        session
    }

    /// Resolve the active session_id for a user. Creates one if none exists.
    fn resolve_active(&self, user_id: &str) -> String {
        // 1. Check in-memory mapping.
        if let Some(sid) = self.active.read().get(user_id) {
            return sid.clone();
        }

        // 2. Check backend.
        if let Some(sid) = self.backend.get_active_session(user_id) {
            self.active.write().insert(user_id.to_string(), sid.clone());
            return sid;
        }

        // 3. Auto-create.
        match self.backend.create_session(user_id, None) {
            Ok(info) => {
                let _ = self.backend.set_active_session(user_id, &info.id);
                self.active.write().insert(user_id.to_string(), info.id.clone());
                tracing::info!(user = %user_id, session = %info.id, "auto-created first session");
                info.id
            }
            Err(e) => {
                // Backend failed (disk full, permissions, …). Generate an ephemeral
                // session ID so the agent can still operate this turn, rather than
                // crashing the whole process.
                let ephemeral = format!("ephemeral:{}", uuid::Uuid::new_v4());
                tracing::warn!(
                    err = %e,
                    user = %user_id,
                    session = %ephemeral,
                    "backend failed to create session; using ephemeral (non-persisted) session"
                );
                self.active.write().insert(user_id.to_string(), ephemeral.clone());
                ephemeral
            }
        }
    }

    /// Create a new session and make it active for the user.
    pub fn new_session(&self, user_id: &str, name: Option<&str>) -> std::io::Result<SessionInfo> {
        // Invalidate old cached session.
        if let Some(old_id) = self.active.read().get(user_id).cloned() {
            self.cache.write().remove(&old_id);
        }

        let info = self.backend.create_session(user_id, name)?;
        self.backend.set_active_session(user_id, &info.id)?;
        self.active.write().insert(user_id.to_string(), info.id.clone());
        tracing::info!(user = %user_id, session = %info.id, "new session created");
        Ok(info)
    }

    /// Switch to an existing session.
    ///
    /// Returns `SessionNotOwned` (wrapped in io::Error::Other) when the session
    /// exists but belongs to a different routing_key. UI should catch this and
    /// offer to create a new session in the current channel instead of bouncing
    /// the user to the other channel's session pool.
    pub fn switch_session(&self, user_id: &str, session_id: &str) -> std::io::Result<SessionInfo> {
        let info = self.backend.get_session(session_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "session not found"))?;

        if info.owner != user_id {
            let err = SessionNotOwned {
                session_id: session_id.to_string(),
                routing_key: user_id.to_string(),
            };
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, err));
        }

        // Invalidate old cached session.
        if let Some(old_id) = self.active.read().get(user_id).cloned() {
            self.cache.write().remove(&old_id);
        }

        self.backend.set_active_session(user_id, session_id)?;
        self.active.write().insert(user_id.to_string(), session_id.to_string());
        tracing::info!(user = %user_id, session = %session_id, "switched session");
        Ok(info)
    }

    /// Delete a session. Cannot delete the active session.
    pub fn delete_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()> {
        // Check not active.
        if self.active.read().get(user_id).map(|s| s.as_str()) == Some(session_id) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "cannot delete the active session"));
        }

        let info = self.backend.get_session(session_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "session not found"))?;

        if info.owner != user_id {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not your session"));
        }

        self.cache.write().remove(session_id);
        self.backend.delete_session(session_id)?;
        tracing::info!(user = %user_id, session = %session_id, "session deleted");
        Ok(())
    }

    /// Rename a session.
    pub fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        self.backend.rename_session(session_id, name)
    }

    /// List all sessions for a user.
    pub fn list_sessions(&self, user_id: &str) -> Vec<SessionInfo> {
        self.backend.list_sessions(user_id)
    }

    /// List ALL sessions across all owners (for startup recovery).
    pub fn list_all_sessions(&self) -> Vec<SessionInfo> {
        self.backend.list_all_sessions()
    }

    /// Get the active session_id for a user (None if not resolved yet).
    pub fn active_session_id(&self, user_id: &str) -> Option<String> {
        self.active.read().get(user_id).cloned()
            .or_else(|| self.backend.get_active_session(user_id))
    }

    /// Save a session override for a user's active session.
    /// Updates the in-memory cache and persists to the backend.
    pub fn save_session_override(&self, user_id: &str, session_override: SessionOverride) {
        let session_id = match self.active_session_id(user_id) {
            Some(id) => id,
            None => return,
        };

        // Update cache.
        {
            let mut cache = self.cache.write();
            if let Some(session) = cache.get_mut(&session_id) {
                session.session_override = session_override.clone();
            }
        }

        // Persist.
        if let Ok(json) = serde_json::to_string(&session_override) {
            if let Err(e) = self.backend.save_session_override(&session_id, &json) {
                tracing::warn!(session = %session_id, err = %e, "persist session override failed");
            }
        }
    }

    /// Get the current session override for the user's active session.
    pub fn get_session_override(&self, user_id: &str) -> SessionOverride {
        let session_id = match self.active_session_id(user_id) {
            Some(id) => id,
            None => return SessionOverride::default(),
        };
        self.cache.read().get(&session_id)
            .map(|s| s.session_override.clone())
            .unwrap_or_default()
    }

    /// Append a message to a session and persist.
    pub fn append_message(&self, session_id: &str, message: ChatMessage) {
        let msg_id = self.backend.append_message(session_id, &message).unwrap_or(0);
        let mut cache = self.cache.write();
        if let Some(session) = cache.get_mut(session_id) {
            session.history.push(message);
            session.message_ids.push(msg_id);
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::in_memory()
    }
}
