//! SessionManager — creates, retrieves, and persists sessions.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use parking_lot::RwLock;
use std::collections::HashMap;

use crate::agents::Agent;
use crate::agents::agent_registry::AgentRegistry;
use crate::agents::session_context::SessionContext;
use crate::agents::user_profile::UserResolver;
use crate::config::sub_agent::SubAgentConfig;

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

use crate::providers::capability_chat::ChatMessage;
use crate::storage::{SessionBackend, SessionInfo};

use super::backend::InMemoryBackend;
use super::recovery::{BreakpointItem, identify_breakpoint};
use super::session_override::SessionOverride;
use super::session_override::sanitize_paired;
use super::types::{Session, SummaryMetadata};

/// Manages session lifecycle — creates, retrieves, and persists sessions.
pub struct SessionManager {
    backend: Arc<dyn SessionBackend>,
    /// User's active session: user_id → session_id.
    /// Per-routing-key SessionContext. At most one SessionContext per
    /// routing_key (the 1:1 invariant): every active routing_key has a
    /// SessionContext that wraps its active Session.
    contexts: RwLock<HashMap<String, Arc<SessionContext>>>,
    /// AgentRegistry used to resolve `Session.agent_name` to an
    /// `Arc<Agent>` when building SessionContexts. Defaults to an
    /// empty registry for test-only managers; production daemons
    /// install the workspace-loaded registry via `with_agents`.
    /// Stored as `Arc` so it stays in sync with AgentRuntime's view.
    agents: Arc<AgentRegistry>,
    /// User resolver — maps routing_key → user_id for per-user paths
    /// (profile, memory). Held here so `list_sessions_for_user` and
    /// future per-user lookups don't need to take it as a parameter.
    resolver: Arc<UserResolver>,
}

impl SessionManager {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self {
            backend,
            contexts: RwLock::new(HashMap::new()),
            agents: Arc::new(AgentRegistry::new()),
            resolver: Arc::new(UserResolver::new()),
        }
    }

    /// Install the shared AgentRegistry used to resolve
    /// `Session.agent_name` to an `Arc<Agent>` when materializing
    /// SessionContexts. The same `Arc<AgentRegistry>` is shared with
    /// AgentRuntime so workspace reloads are visible to both.
    pub fn with_agents(mut self, agents: Arc<AgentRegistry>) -> Self {
        self.agents = agents;
        self
    }

    /// Install the shared UserResolver. The daemon shares the same
    /// `Arc<UserResolver>` between SessionManager and other components
    /// that need routing_key → user_id mappings.
    pub fn with_resolver(mut self, resolver: Arc<UserResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Borrow the installed resolver.
    pub fn resolver(&self) -> &Arc<UserResolver> {
        &self.resolver
    }

    pub fn in_memory() -> Self {
        Self::new(Arc::new(InMemoryBackend::new()))
    }

    /// Shared reference to the underlying backend. Used by callers
    /// (DelegationCoordinator post-B15) that need to construct
    /// `PersistHook`s pointing at the same storage layer as the manager.
    pub fn backend(&self) -> &Arc<dyn SessionBackend> {
        &self.backend
    }

    /// Get the active session for a user. Auto-creates if none exists.
    /// Attempts summary-based recovery first, then falls back to full load.
    ///
    /// Per the RFC v2 target shape, SessionManager no longer keeps a
    /// per-session cache — the canonical mutable state lives on
    /// `SessionContext.session` (held in `contexts`). Callers that
    /// just need a read-only snapshot reach for `get_context(rk)?
    /// .session.lock().await.clone()`; this helper loads fresh from
    /// the backend for callers that want a one-shot value.
    pub fn get_or_create(&self, user_id: &str) -> Session {
        let session_id = self.resolve_active(user_id);
        self.load_session(session_id, user_id.to_string())
    }

    fn load_session(&self, session_id: String, owner: String) -> Session {
        // Load from backend.
        let stored_total_tokens = self.backend.load_token_count(&session_id);
        let session_override = self
            .backend
            .load_session_override(&session_id)
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

        let (ids, msgs, breakpoints): (Vec<i64>, Vec<ChatMessage>, Vec<BreakpointItem>) =
            if !breakpoints.is_empty() {
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
                            stored_total_tokens,
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

        // Seed the token tracker from the persisted total so it carries
        // across restarts. If there's no stored value, the tracker stays
        // fresh and Agent::run will estimate from history at turn start.
        let mut token_tracker = crate::agents::tokens::TokenTracker::new();
        if let Some(total) = stored_total_tokens {
            token_tracker.update_from_usage(total, 0, 0);
        }

        let mut session = Session {
            id: session_id.clone(),
            owner,
            agent_name: self
                .backend
                .load_agent_name(&session_id)
                .unwrap_or_else(|| "main".to_string()),
            parent_session_id: self.backend.load_parent_session_id(&session_id),
            history: msgs,
            message_ids: ids,
            compact_version: compact_ver,
            summary_metadata: summary_meta,
            session_override,
            incomplete_turn: false,
            last_message: self.backend.load_last_message(&session_id),
            token_tracker,
            attachments: crate::agents::attachment::AttachmentManager::new(),
            persist: None,
            channel: None,
            turn_stream: None,
            sub_agent_inbox: None,
            sub_agent_task_id: self.backend.load_task_id(&session_id),
            turn_injections: Vec::new(),
            turn_silenced: false,
        };
        // Breakpoints are detected purely for the incomplete-turn flag below;
        // recovery itself is handled by `Agent::run_recovery` (which re-reads
        // history) so we don't carry the detected items on the Session.
        let _ = breakpoints;

        // Detect incomplete turn: when the session history ends with user or tool,
        // or an assistant message with pending tool calls.
        if crate::agents::orchestrator::history_has_incomplete_turn(&session.history) {
            session.incomplete_turn = true;
            tracing::warn!(session = %session_id, "detected incomplete turn on load");
        }

        session
    }

    /// Resolve the active session_id for a user. Creates one if none exists.
    /// Per the RFC v2 target shape (cache + active layers removed),
    /// session_id lookup goes straight to the backend.
    fn resolve_active(&self, user_id: &str) -> String {
        if let Some(sid) = self.backend.get_active_session(user_id) {
            return sid;
        }

        // Auto-create on first contact.
        match self.backend.create_session(user_id, None) {
            Ok(info) => {
                let _ = self.backend.set_active_session(user_id, &info.id);
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
                ephemeral
            }
        }
    }

    /// Create a new session and make it active for the user.
    pub fn new_session(&self, user_id: &str, name: Option<&str>) -> std::io::Result<SessionInfo> {
        let info = self.backend.create_session(user_id, name)?;
        self.backend.set_active_session(user_id, &info.id)?;
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
        let info = self.backend.get_session(session_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "session not found")
        })?;

        if info.owner != user_id {
            let err = SessionNotOwned {
                session_id: session_id.to_string(),
                routing_key: user_id.to_string(),
            };
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                err,
            ));
        }

        self.backend.set_active_session(user_id, session_id)?;
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
        if self.backend.get_active_session(user_id).as_deref() == Some(session_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cannot delete the active session",
            ));
        }

        let info = self.backend.get_session(session_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "session not found")
        })?;

        if info.owner != user_id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "not your session",
            ));
        }

        // Cascade: drop sub-sessions first so an interrupted delete leaves
        // sub-sessions still reachable through their (now orphaned) parent,
        // rather than the other way around.
        for sub in self.list_sub_sessions(session_id) {
            if let Err(e) = self.backend.delete_session(&sub.id) {
                tracing::warn!(
                    parent = %session_id,
                    sub = %sub.id,
                    err = %e,
                    "failed to delete sub-session during cascade; continuing"
                );
            }
        }

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
    pub fn list_sessions_for_user(&self, user_id: &str) -> Vec<SessionInfo> {
        let mut seen = std::collections::HashSet::<String>::new();
        let mut out = Vec::new();
        for rk in self.resolver.routing_keys_for(user_id) {
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
                self.backend.load_parent_session_id(&info.id).as_deref() == Some(parent_session_id)
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
        let info = self.backend.get_session(session_id)?;
        Some(self.load_session(info.id, info.owner))
    }

    /// Build a SessionContext for a **non-active** session without registering
    /// it in the `contexts` table.
    ///
    /// Used by delegation wake to process a sub-agent completion for a session
    /// that the user has switched away from. The caller holds the only `Arc`
    /// and drops it after processing; the session is NOT made active.
    ///
    /// Returns `None` if the session doesn't exist in the backend.
    pub fn load_context_by_session_id(&self, session_id: &str) -> Option<Arc<SessionContext>> {
        let mut session = self.get_by_id(session_id)?;
        session.persist = Some(self.build_persist_hook());
        let agent = self.build_agent_for_session(&session);
        Some(Arc::new(SessionContext::new(session, agent)))
    }

    /// Per RFC §三.A line 412: build a sub-session SessionContext that
    /// is NOT registered in the `contexts` table — the caller holds the
    /// only Arc and drops it when the delegation finishes. The bound
    /// Session.session_override is left empty; the caller (typically
    /// `DelegationCoordinator`) populates it (run_mode = Background,
    /// permission_mode = Full, system_prompt_override = identity prompt)
    /// before calling `process_turn`.
    pub fn create_sub_session_context(
        &self,
        parent_session_id: &str,
        agent_name: &str,
    ) -> std::io::Result<Arc<SessionContext>> {
        let info = self.create_sub_session(parent_session_id, agent_name)?;
        // Load the freshly-created session through the standard get_or_create
        // path — owner is resolved from backend metadata.
        let owner = self
            .backend
            .get_session(&info.id)
            .map(|s| s.owner)
            .unwrap_or_else(|| parent_session_id.to_string());
        let mut session = self.get_or_create(&owner);
        if session.id != info.id {
            // get_or_create returned the routing_key's active session, not
            // the sub-session we just made. Load explicitly via owner +
            // sub-session id: backend.load_messages on the sub-session id.
            session = crate::agents::session::types::Session::new(info.id.clone());
            session.owner = owner;
            session.parent_session_id = Some(parent_session_id.to_string());
            session.agent_name = agent_name.to_string();
        }
        session.persist = Some(self.build_persist_hook());
        let agent = self.build_agent_for_session(&session);
        Ok(Arc::new(SessionContext::new(session, agent)))
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
        let parent = self.backend.get_session(parent_session_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "parent session not found")
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
        self.backend.get_active_session(user_id)
    }

    /// Save a session override for a user's active session.
    ///
    /// If a `SessionContext` is currently materialized for the
    /// routing_key, the live `Session.session_override` inside it is
    /// updated synchronously (acquiring the mutex via `try_lock` to
    /// avoid blocking when a turn is in flight; the backend write
    /// happens unconditionally so the next session load sees the new
    /// value either way).
    pub fn save_session_override(&self, user_id: &str, session_override: SessionOverride) {
        let session_id = match self.active_session_id(user_id) {
            Some(id) => id,
            None => return,
        };

        // Update the live SessionContext.session if we can grab the mutex.
        if let Some(ctx) = self.get_context(user_id) {
            if let Ok(mut session) = ctx.session.try_lock() {
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
        // Prefer the live SessionContext's view when one is materialized.
        if let Some(ctx) = self.get_context(user_id) {
            if let Ok(session) = ctx.session.try_lock() {
                return session.session_override.clone();
            }
        }
        // Otherwise read from the backend.
        let session_id = match self.active_session_id(user_id) {
            Some(id) => id,
            None => return SessionOverride::default(),
        };
        self.backend
            .load_session_override(&session_id)
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// Look up an existing SessionContext for a routing_key. Returns
    /// `None` if no turn has materialized one yet.
    pub fn get_context(&self, routing_key: &str) -> Option<Arc<SessionContext>> {
        self.contexts.read().get(routing_key).cloned()
    }

    /// Find the registered (contexts-table) SessionContext wrapping the
    /// session with this hex id. The coordinator uses it to register 方案 C
    /// pending delegations from `spawn_delegate_async`, which only knows the
    /// parent's hex session id. Comparison is lock-free on
    /// `SessionContext.session_id` (snapshot taken at construction), so it
    /// works even while `process_turn` holds the session's tokio Mutex.
    /// Unregistered sessions (sub-sessions, switched-away sessions) return
    /// `None` — 挂起只对 orchestrator 活跃主会话生效.
    pub fn registered_context_by_session_id(&self, session_id: &str) -> Option<Arc<SessionContext>> {
        self.contexts
            .read()
            .values()
            .find(|ctx| ctx.session_id == session_id)
            .cloned()
    }

    /// Get-or-create the SessionContext for a routing_key. On miss,
    /// loads the active Session via `get_or_create` and wraps it with
    /// an Agent resolved from `Session.agent_name`. This is the
    /// canonical entry point for per-turn dispatch.
    pub fn get_or_create_context(&self, routing_key: &str) -> Arc<SessionContext> {
        self.get_or_create_context_with(routing_key, |_| {})
    }

    /// Get-or-create with a hook to mutate the freshly loaded Session
    /// before it's wrapped (scheduler/webhook paths use this to force
    /// `session_override.run_mode = Background` so the prompt builder
    /// knows the turn is unattended). The closure runs only on cache
    /// miss — once a SessionContext exists, subsequent calls reuse it
    /// verbatim.
    pub fn get_or_create_context_with<F>(
        &self,
        routing_key: &str,
        configure_session: F,
    ) -> Arc<SessionContext>
    where
        F: FnOnce(&mut Session),
    {
        if let Some(existing) = self.get_context(routing_key) {
            return existing;
        }
        let mut session = self.get_or_create(routing_key);
        configure_session(&mut session);
        // Wire the persist hook once at SessionContext creation. It's
        // the same hook every turn; process_turn no longer needs to
        // pull it from AgentRuntime.
        session.persist = Some(self.build_persist_hook());
        let agent = self.build_agent_for_session(&session);
        let ctx = Arc::new(SessionContext::new(session, agent));
        self.contexts
            .write()
            .insert(routing_key.to_string(), ctx.clone());
        ctx
    }

    /// Build a fresh BackendPersistHook bound to the manager's
    /// backend. Cheap (just Arc-clones the backend); callers that
    /// need their own hook (recovery paths) can ask for one without
    /// reaching for the AgentRuntime.
    pub fn build_persist_hook(&self) -> Arc<dyn super::PersistHook> {
        Arc::new(super::BackendPersistHook::new(Arc::clone(&self.backend)))
    }

    /// Resolve `session.agent_name` to an `Arc<Agent>` via the
    /// installed AgentRegistry. Falls back to a permissive default
    /// for cases where `workspace/agents/<name>` hasn't been parsed
    /// into the registry yet.
    fn build_agent_for_session(&self, session: &Session) -> Arc<Agent> {
        self.agents
            .get(&session.agent_name)
            .unwrap_or_else(|| Arc::new(Agent::new(permissive_main_default(&session.agent_name))))
    }

    /// Register a caller-built SessionContext. Used by the
    /// scheduler/webhook paths that need to customize
    /// `session_override.run_mode` before wrapping the Session.
    pub fn register_context(&self, routing_key: &str, ctx: Arc<SessionContext>) {
        self.contexts.write().insert(routing_key.to_string(), ctx);
    }

    /// Drop the SessionContext for a routing_key. Called by /reset,
    /// /autonomy, and /switch-session so the next turn rebuilds the
    /// context (and, transitively, the cached system prompt) from
    /// the fresh override.
    pub fn drop_context(&self, routing_key: &str) {
        self.contexts.write().remove(routing_key);
    }

    /// Append a message to a session's backend store.
    ///
    /// Used by the queue-drain path on daemon startup before any
    /// `SessionContext` for the session has been materialized; the
    /// session's first inbound thereafter loads the appended messages
    /// from the backend like any other history.
    pub fn append_message(&self, session_id: &str, message: ChatMessage) {
        let _ = self.backend.append_message(session_id, &message);
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::in_memory()
    }
}

/// Permissive fallback SubAgentConfig used when the AgentRegistry
/// doesn't yet have a definition for `agent_name`. "all" tools / skills
/// / MCP servers; empty system prompt because the cached prompt on
/// AgentRuntime is what's actually injected at turn start.
fn permissive_main_default(agent_name: &str) -> SubAgentConfig {
    SubAgentConfig {
        name: agent_name.to_string(),
        description: None,
        system_prompt: String::new(),
        tools: crate::config::filters::ToolFilter::all(),
        skills: Default::default(),
        mcp: Default::default(),
        model: None,
        max_tool_calls: None,
        isolation: Default::default(),
        timeout: None,
    }
}
