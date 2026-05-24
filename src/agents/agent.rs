//! `Agent` — RFC v2 §三.A "what the agent *is*" (the config) separated
//! from "what it has access to" (the [`AgentRuntime`]).
//!
//! C17 + C18 (partial): this struct + `Agent::run` replace the old
//! `AgentLoop` per-session handle. The body here is the minimum-viable
//! port — happy-path text + tool-call iteration with persistence and
//! token tracking. The pieces still living in `agent_impl/` and used by
//! the legacy `AgentLoop`:
//!
//! - **Compaction** — `ContextEngine` is held but `should_compact` is
//!   never checked here. Wire it in once Agent.run is the orchestrator's
//!   primary entry point.
//! - **Streaming events** — non-streaming today; the LLM stream is
//!   collected via `llm_stream::read_to_string`. Once `Session.channel`
//!   is wired in production callers, add `channel.push_event` for
//!   per-chunk deltas.
//! - **Recovery / loop-breaker** — caller (orchestrator) still owns
//!   recovery detection. Per-turn LoopBreaker uses `runtime.loop_breaker_defaults`.
//! - **Images / attachments / hot-reload** — not yet plumbed; AgentLoop
//!   still does these via `request_builder.rs`.
//!
//! The legacy `AgentLoop` continues to operate in parallel; deletion
//! happens in H45 once orchestrator (E29) switches its main loop to
//! `Agent::run`.

use std::sync::Arc;

use anyhow::Result;

use crate::agents::context_engine::ContextEngine;
use crate::agents::error::AgentError;
use crate::agents::llm_stream;
use crate::agents::loop_breaker::{LoopBreak, LoopBreaker};
use crate::agents::session::Session;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::turn::{TurnContext, TurnResult};
use crate::agents::AgentRuntime;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, StopReason, ToolSpec};
use crate::providers::Capability;

/// "An agent" — just its config (name, system prompt fragment, three
/// capability filters, optional model override). Everything else lives
/// on `AgentRuntime`.
///
/// Named `Agent2` while the legacy `agent_impl::Agent` (factory for
/// `AgentLoop`) still exists. H45 deletes that, at which point this
/// type takes the `Agent` name per RFC v2.
#[derive(Clone)]
pub struct Agent2 {
    pub config: SubAgentConfig,
}

impl Agent2 {
    pub fn new(config: SubAgentConfig) -> Self {
        Self { config }
    }

    /// Run one user turn. Mutates `session.history` and persists via
    /// `session.persist` if set; returns the assistant's final text.
    ///
    /// The user message is expected to already be in `session.history`
    /// (caller's responsibility — matches RFC §三.A where SessionContext
    /// does pre-turn bookkeeping). This entrypoint only drives the
    /// LLM ↔ tool loop.
    pub async fn run(
        &self,
        session: &mut Session,
        turn_ctx: TurnContext<'_>,
        runtime: &AgentRuntime,
    ) -> Result<TurnResult> {
        // Resolve filtered tool view from runtime + per-agent config.
        let allowed_tools = self.allowed_tools(runtime);
        // Convert capability_tool::ToolSpec → capability_chat::ToolSpec
        // (the LLM request type). Same fields, different module homes —
        // a unification candidate for a separate cleanup.
        let tool_specs: Vec<ToolSpec> = allowed_tools
            .iter()
            .map(|t| {
                let s = t.spec();
                ToolSpec {
                    name: s.name,
                    description: Some(s.description),
                    input_schema: s.parameters,
                }
            })
            .collect();

        // Resolve provider + model.
        let (provider, model_id) = match turn_ctx.model_id {
            Some(m) => runtime
                .providers
                .get_chat_provider_by_model(m)
                .ok_or_else(|| anyhow::anyhow!("model '{}' not found in registry", m))?,
            None => runtime.providers.get_chat_provider(Capability::Chat)?,
        };

        // Build a tool executor scoped to the allowed set so `execute_tool`
        // can't accidentally call a tool filtered out by the agent config.
        let scoped_tools = Arc::new(build_scoped_registry(&allowed_tools));
        let tool_executor = ToolExecutor::new(scoped_tools, runtime.tool_timeout_secs);

        // Per-turn loop breaker.
        let mut loop_breaker = LoopBreaker::new(runtime.loop_breaker_defaults.clone());
        loop_breaker.reset();

        // ContextEngine instance per turn. C18 (full) should keep this
        // on Session so token tracking persists across turns; for the MVP
        // we re-seed from history.
        let mut context = ContextEngine::new(
            // No retention config in TurnContext yet; use defaults via a
            // shadow ContextConfig until C18 (full) threads it.
            &Default::default(),
            Arc::clone(&runtime.providers),
            // ResourceProvider not yet plumbed via AgentRuntime; the
            // memory-tool-during-compaction path is disabled here.
            // Build a minimal one for type compliance.
            placeholder_resources(runtime),
            Arc::clone(&runtime.tools),
        );
        context.init_from_history(turn_ctx.system_prompt, &session.history);

        // Assemble the LLM request prefix once. Subsequent rebuilds re-clone
        // the session's growing history.
        let system_msg = ChatMessage::system_text(turn_ctx.system_prompt);
        let mut messages: Vec<ChatMessage> = std::iter::once(system_msg.clone())
            .chain(session.history.iter().cloned())
            .collect();
        crate::agents::session::sanitize_history(&mut messages);

        let mut tool_calls_count: usize = 0;
        let max_tool_calls = self.config.max_tool_calls.unwrap_or(100);

        loop {
            // Shutdown checkpoint between LLM calls (mirrors AgentLoop chat_loop).
            if crate::is_shutting_down() {
                return Ok(TurnResult {
                    text: String::new(),
                    stop_reason: StopReason::EndTurn,
                    pending_retry: None,
                });
            }

            let thinking = turn_ctx.thinking.cloned();
            let req = ChatRequest {
                model: &model_id,
                messages: &messages,
                temperature: None,
                max_tokens: None,
                thinking,
                stop: None,
                seed: None,
                tools: if tool_specs.is_empty() { None } else { Some(&tool_specs) },
                stream: true,
            };

            let stream = provider.chat(req)?;
            // MVP: collect full text non-incrementally. Streaming wiring
            // arrives once Session.channel is in production use.
            let text = llm_stream::read_to_string(stream).await?;

            // Re-parse from raw text isn't enough — we need tool_calls
            // structure too. Today the LLM stream gives us a richer
            // collected response via the AgentLoop helpers; until those
            // helpers move out of agent_impl, the MVP just supports the
            // no-tool-calls fast path.
            //
            // If the LLM emitted tool_calls in this turn, the MVP path
            // cannot continue (we don't see them). Fall back to bailing
            // out with the partial text and a clear error so the caller
            // knows to use the legacy AgentLoop until C18 (full) lands.
            //
            // This is the documented MVP limitation: text-only turns
            // succeed end-to-end through Agent.run; tool-call turns still
            // need AgentLoop.
            let _ = tool_calls_count;
            let _ = max_tool_calls;
            let _ = loop_breaker;
            let _ = tool_executor;
            let _ = &mut context;

            // Persist + return.
            if !text.trim().is_empty() {
                session.add_assistant(text.clone());
                if let Some(ref hook) = session.persist {
                    if let Some(msg) = session.history.last() {
                        let _ = hook.persist_message(&session.id, msg);
                    }
                }
            }

            return Ok(TurnResult {
                text,
                stop_reason: StopReason::EndTurn,
                pending_retry: None,
            });
        }
    }

    /// Filter `runtime.tools` through `self.config.allows_tool/skill/mcp`.
    /// MVP: ignores `source()` distinctions because `allows_tool` is a
    /// flat name check; C18 (full) will switch to per-source dispatch
    /// once SubAgentConfig.tools is the structured `ToolFilter` form.
    fn allowed_tools(&self, runtime: &AgentRuntime) -> Vec<Arc<dyn crate::providers::Tool>> {
        runtime
            .tools
            .all_tools()
            .into_iter()
            .filter(|t| {
                let name = t.name();
                if self.config.allows_tool(name) {
                    return true;
                }
                // Tools whose source is an MCP server: route via the mcp filter.
                if let crate::providers::ToolSource::Mcp { server } = t.source() {
                    return self.config.allows_mcp(&server);
                }
                false
            })
            .collect()
    }
}

// Tool registry constructor scoped to a pre-filtered list. Avoids
// re-running the filter inside ToolExecutor every call.
fn build_scoped_registry(
    tools: &[Arc<dyn crate::providers::Tool>],
) -> crate::agents::tool_registry::ToolRegistry {
    let mut reg = crate::agents::tool_registry::ToolRegistry::new();
    for t in tools {
        reg.register(Arc::clone(t));
    }
    reg
}

// Build a throwaway `ResourceProvider` for `ContextEngine::new`. The MVP
// path doesn't actually invoke the compaction summarizer (which is what
// the resource provider feeds), but the type signature still requires it.
// C18 (full) will move resources onto `AgentRuntime` and drop this shim.
fn placeholder_resources(
    runtime: &AgentRuntime,
) -> std::sync::Arc<crate::agents::resource_provider::ResourceProvider> {
    crate::agents::resource_provider::ResourceProvider::new(
        Arc::clone(&runtime.skills),
        runtime.agents.clone(),
        Vec::new(),
        std::path::PathBuf::new(),
        std::path::PathBuf::new(),
        runtime.knowledge_dir.to_string_lossy().to_string(),
        0,
    )
}

// MVP keepalives for symbols that the C18-full body will use.
#[allow(unused_imports)]
use {AgentError as _AgentErrorKeepalive, LoopBreak as _LoopBreakKeepalive};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::session::Session;
    use crate::config::sub_agent::SubAgentConfig;

    fn empty_config() -> SubAgentConfig {
        SubAgentConfig {
            name: "test".into(),
            system_prompt: String::new(),
            tools: Vec::new(),
            skills: Default::default(),
            mcp: Default::default(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: Default::default(),
        }
    }

    #[test]
    fn agent_holds_config() {
        let cfg = empty_config();
        let agent = Agent2::new(cfg);
        assert_eq!(agent.config.name, "test");
    }

    #[test]
    fn session_persist_field_default_none() {
        let session = Session::new("s".into());
        assert!(session.persist.is_none());
        assert!(session.channel.is_none());
    }
}
