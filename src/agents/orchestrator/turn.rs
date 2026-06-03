//! Per-turn parameter resolution.
//!
//! Collapses the "merge `session_override` into the prompt config, build the
//! system prompt, derive thinking/model" block that was duplicated verbatim
//! across `startup_recover_sessions` and `startup_recover_subagents` (and
//! mirrored inside the scheduled path). One home for "what are the effective
//! parameters of this turn".

use crate::agents::session::Session;
use crate::agents::{AgentRuntime, PermissionMode, RunMode, TurnContext};
use crate::providers::ThinkingConfig;

/// The effective parameters for one turn, owned so that a borrowing
/// [`TurnContext`] can be constructed from it via [`ResolvedTurn::turn_context`].
pub struct ResolvedTurn {
    pub system_prompt: String,
    pub thinking: Option<ThinkingConfig>,
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub run_mode: RunMode,
}

impl ResolvedTurn {
    /// Resolve the session's overrides layered over the runtime defaults:
    /// `SessionOverride > [agent] defaults`.
    pub fn resolve(session: &Session, runtime: &AgentRuntime) -> Self {
        let ov = session.session_override.clone();
        let mut cfg = runtime.defaults.prompt.clone();
        if let Some(pm) = ov.permission_mode {
            cfg.permission_mode = pm;
        }
        if let Some(rm) = ov.run_mode {
            cfg.run_mode = rm;
        }
        let system_prompt = runtime.build_system_prompt(&cfg);
        Self {
            system_prompt,
            thinking: ov.to_thinking_config(),
            model: ov.model.clone(),
            permission_mode: cfg.permission_mode,
            run_mode: cfg.run_mode,
        }
    }

    /// Borrow as a [`TurnContext`] for `Agent::run` / `run_recovery`.
    pub fn turn_context(&self) -> TurnContext<'_> {
        TurnContext {
            system_prompt: &self.system_prompt,
            model_id: self.model.as_deref(),
            thinking: self.thinking.as_ref(),
            permission_mode: self.permission_mode,
            run_mode: self.run_mode,
        }
    }
}
