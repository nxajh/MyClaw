//! Tool-set filtering (config layer / session-channel layer / modality
//! folding), extracted verbatim from the former `agent.rs`.

use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability_chat::{ChatMessage, ToolSpec};
use crate::providers::{
    ContentPart, FileModality, MediaInlineDecision, MediaPolicy, modality_from_mime,
};

use super::Agent;
use crate::agents::AgentRuntime;

impl Agent {
    /// Filter `runtime.tools` through `self.config.allows_tool/skill/mcp`.
    /// MVP: ignores `source()` distinctions because `allows_tool` is a
    /// flat name check; C18 (full) will switch to per-source dispatch
    /// once SubAgentConfig.tools is the structured `ToolFilter` form.
    pub(super) fn allowed_tools(
        &self,
        runtime: &AgentRuntime,
    ) -> Vec<Arc<dyn crate::providers::Tool>> {
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

pub(super) fn filter_turn_scoped_tools(
    allowed_tools: &mut Vec<Arc<dyn crate::providers::Tool>>,
    session: &Session,
) {
    if let Some(allowlist) = &session.turn_tool_allowlist {
        allowed_tools.retain(|tool| allowlist.contains(&tool.name().to_string()));
    }

    allowed_tools.retain(|tool| {
        let keep = match tool.name() {
            "send_message" => {
                // RFC agent-messaging §3: sub-agent sessions get the tool
                // even without a channel — `recipient` targeting reaches the
                // parent agent via the DelegationEvent channel.
                // RFC channel-role-split §1.2: main-agent visibility is now
                // `resolve_channel().is_some() || parent_session_id.is_some()`
                // — a headless (scheduled) turn whose routing key resolves to
                // a real channel keeps the tool (sending intermediate notices
                // from background turns is legitimate, not a coupling bug).
                if session.parent_session_id.is_some() {
                    true
                } else if session.resolve_channel().is_some() {
                    let has_receiver = session.reply_target().is_some();
                    let has_text_send = has_receiver;
                    let has_file_send = session
                        .resolve_channel()
                        .is_some_and(|ch| ch.capabilities().supports_file_send);
                    has_text_send || has_file_send
                } else {
                    false
                }
            }
            "send_media" => false,
            // RFC §4.2: friend tools are main-agent-only — contacts are
            // user-level state and sub-agents never see them.
            "friend_request" | "friend_accept" | "friend_decline" | "friend_list" => {
                session.parent_session_id.is_none()
            }
            _ => true,
        };
        if !keep {
            tracing::debug!(
                tool = tool.name(),
                session = %session.id,
                "filter_turn_scoped_tools: dropped"
            );
        }
        keep
    });
}

/// Remove media-retrieval tools (`view_video`, `view_image`, `hear_audio`)
/// when the model that will handle the request natively supports that input
/// modality. This prevents the model from choosing a tool call over inline
/// content analysis.
///
/// - With an override: that model's capabilities determine the filter.
/// - Without override: the primary model in the routing chain is used. On
///   transient failure the FallbackChatProvider may retry with a different
///   model — acceptable trade-off vs. always keeping redundant tools.
pub(super) fn native_media_availability(messages: &[ChatMessage], policy: MediaPolicy) -> (bool, bool, bool) {
    let mut image = false;
    let mut audio = false;
    let mut video = false;
    for msg in messages {
        for part in &msg.parts {
            let ContentPart::File {
                path,
                mime_type,
                size_bytes,
                ..
            } = part
            else {
                continue;
            };
            let modality = modality_from_mime(mime_type.as_deref(), path);
            if policy.decision_for(modality, *size_bytes) != MediaInlineDecision::Inline {
                continue;
            }
            match modality {
                FileModality::Image => image = true,
                FileModality::Audio => audio = true,
                FileModality::Video => video = true,
                FileModality::Other => {}
            }
        }
    }
    (image, audio, video)
}

pub(super) fn filter_modality_redundant_tools(
    allowed_tools: &mut Vec<Arc<dyn crate::providers::Tool>>,
    messages: &[ChatMessage],
    policy: MediaPolicy,
    model_id: &str,
) {
    let (native_image, native_audio, native_video) = native_media_availability(messages, policy);

    allowed_tools.retain(|tool| {
        let drop = match tool.name() {
            "view_video" => native_video,
            "view_image" => native_image,
            "hear_audio" => native_audio,
            _ => false,
        };
        if drop {
            tracing::info!(
                model = %model_id,
                tool = tool.name(),
                native_image,
                native_audio,
                native_video,
                "filter_modality_redundant_tools: dropping tool, current request has inline native media"
            );
        }
        !drop
    });
}

/// Backstop for a rare edge (e.g. config hot-reload drops the aux model
/// mid-session): if the request won't declare `tool_name` but history still
/// references it, fold each such call + its result into inline `[label]: …` text
/// on the calling assistant message and drop the tool-result message, so no
/// orphan tool call survives to be rejected by the provider. No-op when the tool
/// is declared. Operates on the cloned `messages` only.
pub(crate) fn fold_absent_tool(
    messages: &mut Vec<ChatMessage>,
    tool_specs: &[ToolSpec],
    tool_name: &str,
    label: &str,
) {
    if tool_specs.iter().any(|t| t.name == tool_name) {
        return;
    }
    let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages.iter() {
        if m.role == "assistant" {
            if let Some(tcs) = &m.tool_calls {
                for tc in tcs.iter().filter(|tc| tc.name == tool_name) {
                    ids.insert(tc.id.clone());
                }
            }
        }
    }
    if ids.is_empty() {
        return;
    }
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages.iter() {
        if m.role == "tool" {
            if let Some(id) = &m.tool_call_id {
                if ids.contains(id) {
                    results.insert(id.clone(), m.text_content());
                }
            }
        }
    }
    for m in messages.iter_mut() {
        if m.role != "assistant" {
            continue;
        }
        let Some(tcs) = m.tool_calls.take() else {
            continue;
        };
        let (mine, rest): (Vec<_>, Vec<_>) = tcs.into_iter().partition(|tc| tc.name == tool_name);
        m.tool_calls = if rest.is_empty() { None } else { Some(rest) };
        for tc in mine {
            if let Some(out) = results.get(&tc.id) {
                if !out.is_empty() {
                    m.parts.push(ContentPart::Text {
                        text: format!("[{label}]: {out}"),
                    });
                }
            }
        }
    }
    messages.retain(|m| {
        !(m.role == "tool" && m.tool_call_id.as_ref().is_some_and(|id| ids.contains(id)))
    });
}

/// Persist `session.history.last()` via `session.persist` and write the
/// returned backend ID into `session.message_ids.last_mut()`. Mirrors the
/// legacy `AgentLoop` pattern — without the id-capture, message_ids stays
/// at the 0 placeholder forever, which breaks compaction's

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_turn_scoped_tools_hides_send_tools_without_channel() {
        let mut session = Session::new("s".into());
        session.record_inbound(crate::channels::ChannelInboundMessage {
            id: "test".into(),
            sender: crate::channels::MessageSender::new("u"),
            receiver: crate::channels::MessageReceiver::new("s"),
            content: crate::channels::ChannelMessageContent::text("hi"),
            timestamp: 0,
            interruption_scope_id: None,
            silenced_override: None,
            run_mode: Default::default(),
        });
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("send_message")),
            Arc::new(NamedTool("send_media")),
            Arc::new(NamedTool("calculator")),
        ];

        filter_turn_scoped_tools(&mut tools, &session);

        assert_eq!(tool_names(&tools), vec!["calculator"]);
    }

    #[test]
    fn filter_turn_scoped_tools_keeps_send_message_for_sub_agent() {
        // RFC agent-messaging §3.3: a sub-agent gets send_message even with
        // no channel — `recipient` targeting reaches the parent agent via
        // the DelegationEvent channel.
        let mut session = Session::new("s".into());
        session.parent_session_id = Some("parent".into());
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(NamedTool("send_message")),
            Arc::new(NamedTool("send_media")),
            Arc::new(NamedTool("calculator")),
        ];

        filter_turn_scoped_tools(&mut tools, &session);

        assert_eq!(tool_names(&tools), vec!["send_message", "calculator"]);
    }



    use crate::providers::{Tool, ToolResult};

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            self.0
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _session: &Session,
        ) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        tools.iter().map(|tool| tool.name().to_string()).collect()
    }
}
