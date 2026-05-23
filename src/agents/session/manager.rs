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

    /// Delete a session and all its sub-sessions. Cannot delete the active session.
    ///
    /// B14: cascades into sub-sessions (sessions whose `parent_session_id`
    /// matches `session_id`). A sub-session's history is meaningless once
    /// its parent is gone, so we drop them together rather than leaving
    /// orphaned data on disk.
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

        // Cascade: drop sub-sessions first so an interrupted delete leaves
        // sub-sessions still reachable through their (now orphaned) parent,
        // rather than the other way around.
        for sub in self.list_sub_sessions(session_id) {
            self.cache.write().remove(&sub.id);
            if let Err(e) = self.backend.delete_session(&sub.id) {
                tracing::warn!(
                    parent = %session_id,
                    sub = %sub.id,
                    err = %e,
                    "failed to delete sub-session during cascade; continuing"
                );
            }
        }

        self.cache.write().remove(session_id);
        self.backend.delete_session(session_id)?;
        tracing::info!(user = %user_id, session = %session_id, "session deleted (cascade)");
        Ok(())
    }

    /// Rename a session.
    pub fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()> {
        self.backend.rename_session(session_id, name)
    }

    /// List all sessions for a user. Excludes sub-sessions (sessions with
    /// `parent_session_id != None`).
    /// RFC v2 §三.A: UI session pickers should only show top-level sessions;
    /// sub-sessions are addressed via `agent_delegate` outputs and live in
    /// their parent's context.
    pub fn list_sessions(&self, user_id: &str) -> Vec<SessionInfo> {
        self.backend
            .list_sessions(user_id)
            .into_iter()
            .filter(|info| self.backend.load_parent_session_id(&info.id).is_none())
            .collect()
    }

    /// G44: list all sessions belonging to a user, resolved across every
    /// routing_key that maps to `user_id` via the supplied `UserResolver`.
    ///
    /// Sessions are deduplicated by id; sub-sessions are filtered out.
    /// `list_sessions(routing_key)` is the per-channel slice; this method is
    /// the per-human aggregation needed by the `/sessions` slash command.
    pub fn list_sessions_for_user(
        &self,
        resolver: &crate::agents::UserResolver,
        user_id: &str,
    ) -> Vec<SessionInfo> {
        let mut seen = std::collections::HashSet::<String>::new();
        let mut out = Vec::new();
        for rk in resolver.routing_keys_for(user_id) {
            for info in self.list_sessions(&rk) {
                if seen.insert(info.id.clone()) {
                    out.push(info);
                }
            }
        }
        // If the user_id is itself a routing_key (no override), include
        // sessions registered directly under it.
        if !seen.contains(user_id) {
            for info in self.list_sessions(user_id) {
                if seen.insert(info.id.clone()) {
                    out.push(info);
                }
            }
        }
        out
    }

    /// List sub-sessions of a parent (used by recovery / inspection tools).
    pub fn list_sub_sessions(&self, parent_session_id: &str) -> Vec<SessionInfo> {
        self.backend
            .list_all_sessions()
            .into_iter()
            .filter(|info| {
                self.backend.load_parent_session_id(&info.id).as_deref()
                    == Some(parent_session_id)
            })
            .collect()
    }

    /// Look up a session by its routing_key (the channel:account:sender triple
    /// previously called `user_id`). Returns `None` if no session is bound.
    /// RFC v2 §三.A: alias for `active_session_id` — kept as a B14 placeholder
    /// for the eventual user_id → routing_key vocabulary migration.
    pub fn session_id_for_routing_key(&self, routing_key: &str) -> Option<String> {
        self.active_session_id(routing_key)
    }

    /// Get a session by ID (caller doesn't need to know the routing_key).
    /// Used by delegation recovery and PR-review tools.
    pub fn get_by_id(&self, session_id: &str) -> Option<Session> {
        if let Some(s) = self.cache.read().get(session_id) {
            return Some(s.clone());
        }
        // Cache miss — fall back to the owner-keyed get_or_create using the
        // backend's recorded owner. Returns None if the session doesn't exist.
        let info = self.backend.get_session(session_id)?;
        Some(self.get_or_create(&info.owner))
    }

    /// Create a sub-session that delegates work back to its parent for routing
    /// (replies go through parent.last_message.reply_target).
    ///
    /// B14: thin wrapper around backend.create_session that additionally
    /// persists parent_session_id + agent_name so list_sessions can filter
    /// it out.
    pub fn create_sub_session(
        &self,
        parent_session_id: &str,
        agent_name: &str,
    ) -> std::io::Result<SessionInfo> {
        // Sub-sessions belong to the parent's owner so recovery scans the same
        // bucket. The parent's owner is read from the backend rather than the
        // cache because the parent may have been evicted.
        let parent = self
            .backend
            .get_session(parent_session_id)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "parent session not found",
                )
            })?;
        let info = self.backend.create_session(
            &parent.owner,
            Some(&format!("sub:{}:{}", parent_session_id, agent_name)),
        )?;
        self.backend
            .save_parent_session_id(&info.id, parent_session_id)?;
        self.backend.save_agent_name(&info.id, agent_name)?;
        // Sub-sessions are NOT made the active session for the parent's
        // routing_key — that would hijack the user's chat.
        tracing::info!(
            parent = %parent_session_id,
            sub = %info.id,
            agent = %agent_name,
            "sub-session created"
        );
        Ok(info)
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
