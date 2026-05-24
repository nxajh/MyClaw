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

use futures_util::StreamExt;

use crate::agents::context_engine::ContextEngine;
use crate::agents::error::AgentError;
use crate::agents::loop_breaker::{LoopBreak, LoopBreaker};
use crate::agents::session::Session;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::turn::{TurnContext, TurnResult};
use crate::agents::AgentRuntime;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, StopReason, ToolSpec};
use crate::providers::{BoxStream, Capability, ContentPart, StreamEvent, ToolCall};

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
        let permission_mode = turn_ctx.permission_mode;

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
            let response = collect_stream(stream).await?;

            // Update token tracker from API response.
            if let Some(ref usage) = response.usage {
                context.update_usage(
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0),
                    usage.cached_input_tokens.unwrap_or(0),
                );
                if let Some(ref hook) = session.persist {
                    hook.save_token_count(&session.id, context.token_total());
                }
            }

            // No tool calls → final response. Persist + return.
            if response.tool_calls.is_empty() {
                if !response.text.trim().is_empty() {
                    session.add_assistant(response.text.clone());
                    if let Some(ref hook) = session.persist {
                        if let Some(msg) = session.history.last() {
                            let _ = hook.persist_message(&session.id, msg);
                        }
                    }
                }
                return Ok(TurnResult {
                    text: response.text,
                    stop_reason: response.stop_reason,
                    pending_retry: None,
                });
            }

            // Tool calls present — append assistant message with the calls
            // (preserving thinking content for re-send), execute each tool,
            // append tool_result messages, then loop for the next LLM call.
            let mut assistant_msg = ChatMessage::assistant_text(&response.text);
            assistant_msg.tool_calls = Some(response.tool_calls.clone());
            if let Some(ref thinking_text) = response.reasoning_content {
                assistant_msg.parts.insert(
                    0,
                    ContentPart::Thinking {
                        thinking: thinking_text.clone(),
                        signature: response.thinking_signature.clone(),
                    },
                );
            }
            messages.push(assistant_msg);
            session.add_assistant_with_tools(
                response.text.clone(),
                response.tool_calls.clone(),
                response.reasoning_content.clone(),
                response.thinking_signature.clone(),
            );
            if let Some(ref hook) = session.persist {
                if let Some(msg) = session.history.last() {
                    let _ = hook.persist_message(&session.id, msg);
                }
            }

            for call in &response.tool_calls {
                tool_calls_count += 1;
                if max_tool_calls > 0 && tool_calls_count > max_tool_calls {
                    return Err(anyhow::anyhow!(
                        "tool call limit reached ({}), loop broken",
                        max_tool_calls
                    ));
                }

                let result = tool_executor
                    .execute(call, session, Some(&permission_mode))
                    .await;
                let (result_content, is_error) = match &result {
                    Ok(r) => {
                        let mut out = r.output.clone();
                        if let Some(ref err) = r.error {
                            if out.is_empty() {
                                out = format!("error: {}", err);
                            }
                        }
                        (out, !r.success)
                    }
                    Err(e) => (format!("error: {}", e), true),
                };

                match loop_breaker.record_and_check(
                    &call.name,
                    &call.arguments,
                    &result_content,
                ) {
                    LoopBreak::Detected(reason) => {
                        return Err(AgentError::LoopBreak {
                            reason: format!("{:?}", reason),
                        }
                        .into());
                    }
                    LoopBreak::None => {}
                }

                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(is_error);
                messages.push(tool_msg);

                session.add_tool_result(call.id.clone(), result_content, is_error);
                if let Some(ref hook) = session.persist {
                    if let Some(msg) = session.history.last() {
                        let _ = hook.persist_message(&session.id, msg);
                    }
                }
            }

            // Loop back to the next LLM call with the appended tool_result messages.
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

/// Bundle of fields extracted from one streaming LLM response. Mirrors
/// the shape of `agent_impl::types::CollectedResponse` but is defined
/// here so `agent.rs` doesn't reach into `agent_impl/` internals.
struct CollectedResponse {
    text: String,
    reasoning_content: Option<String>,
    thinking_signature: Option<String>,
    tool_calls: Vec<ToolCall>,
    stop_reason: StopReason,
    usage: Option<crate::providers::ChatUsage>,
}

/// Read a full stream into a [`CollectedResponse`]. Simplified compared
/// to `AgentLoop::collect_stream_inner` — no max_output_bytes guard, no
/// cancellation token, no `channel.push_event` per-chunk forwarding.
/// Those refinements move in once `Session.channel` is wired by E29.
async fn collect_stream(stream: BoxStream<StreamEvent>) -> anyhow::Result<CollectedResponse> {
    let mut stream = stream;
    let mut text = String::new();
    let mut reasoning_content: Option<String> = None;
    let mut thinking_signature: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage: Option<crate::providers::ChatUsage> = None;
    let mut received_first_chunk = false;

    loop {
        let event_opt = if !received_first_chunk {
            match tokio::time::timeout(
                crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT,
                stream.next(),
            )
            .await
            {
                Ok(ev) => ev,
                Err(_) => anyhow::bail!(
                    "stream chunk timeout after {}s, no data received",
                    crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT.as_secs()
                ),
            }
        } else {
            stream.next().await
        };

        let event = match event_opt {
            Some(e) => {
                received_first_chunk = true;
                e
            }
            None => break, // stream ended without explicit Done
        };

        match event {
            StreamEvent::Delta { text: delta } => text.push_str(&delta),
            StreamEvent::Thinking { text: delta } => {
                if !delta.is_empty() {
                    if let Some(rc) = &mut reasoning_content {
                        rc.push_str(&delta);
                    } else {
                        reasoning_content = Some(delta);
                    }
                }
            }
            StreamEvent::ThinkingSignature { signature } => {
                thinking_signature = Some(signature);
            }
            StreamEvent::ToolCallStart { id, name, initial_arguments } => {
                tool_calls.push(ToolCall { id, name, arguments: initial_arguments });
            }
            StreamEvent::ToolCallDelta { id, delta } => {
                if !id.is_empty() {
                    if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                        call.arguments.push_str(&delta);
                    } else {
                        tool_calls.push(ToolCall { id, name: String::new(), arguments: delta });
                    }
                } else if let Some(last) = tool_calls.last_mut() {
                    last.arguments.push_str(&delta);
                }
            }
            StreamEvent::ToolCallEnd { id, name, arguments } => {
                if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                    call.name = name;
                    call.arguments = arguments;
                }
            }
            StreamEvent::Usage(u) => {
                if let Some(ref mut existing) = usage {
                    if u.input_tokens.is_some() {
                        existing.input_tokens = u.input_tokens;
                    }
                    if u.output_tokens.is_some() {
                        existing.output_tokens = u.output_tokens;
                    }
                    if u.cached_input_tokens.is_some() {
                        existing.cached_input_tokens = u.cached_input_tokens;
                    }
                } else {
                    usage = Some(u);
                }
            }
            StreamEvent::Done { reason } => {
                stop_reason = reason;
                break;
            }
            StreamEvent::HttpError { status, message } => {
                return Err(crate::providers::ProviderHttpError { status, message }.into());
            }
            StreamEvent::Error(e) => anyhow::bail!("stream error: {}", e),
        }
    }

    Ok(CollectedResponse {
        text,
        reasoning_content,
        thinking_signature,
        tool_calls,
        stop_reason,
        usage,
    })
}

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
