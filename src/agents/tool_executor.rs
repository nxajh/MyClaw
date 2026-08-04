use std::sync::Arc;
use std::time::Duration;

use super::ask_router::AskRouter;
use super::session::Session;
use super::tokens::is_write_tool;
use super::tool_registry::ToolRegistry;
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, InlineButton, MessageReceiver};
use crate::config::agent::PermissionMode;
use crate::providers::ToolCall;
use crate::providers::capability_tool::ToolResult;

/// Executes tool calls on behalf of `Agent::run`.
///
/// Per the RFC v2 target shape, the executor is a global singleton —
/// it carries only the timeout policy and dispatches against a
/// `&[Arc<dyn Tool>]` slice the caller (Agent::run) has already
/// filtered through the agent's `tools` / `skills` / `mcp` filters.
/// No `ToolRegistry` field; the executor is stateless w.r.t. which
/// tools an agent may call.
pub struct ToolExecutor {
    pub timeout_secs: u64,
    /// Shared `AskRouter` for per-operation approval in Default mode.
    /// When `Some`, write tools trigger an Inline Keyboard approval
    /// prompt before execution. `None` in CLI / test contexts.
    ask_router: Option<Arc<AskRouter>>,
}

impl ToolExecutor {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout_secs,
            ask_router: None,
        }
    }

    pub fn with_ask_router(mut self, router: Arc<AskRouter>) -> Self {
        self.ask_router = Some(router);
        self
    }

    /// Execute a single tool call against the pre-filtered `allowed`
    /// slice.
    ///
    /// `autonomy` controls write-tool blocking:
    /// - **ReadOnly**: hard-block all write tools.
    /// - **Default**: intercept write tools → send approval prompt →
    ///   wait for user decision via Inline Keyboard buttons.
    /// - **Full**: execute everything immediately.
    ///
    /// Special tools (`ask_user`, `agent_delegate`) are real `Tool`
    /// impls in `allowed`, dispatched through the same path as any
    /// other tool.
    pub(crate) async fn execute(
        &self,
        call: &ToolCall,
        session: &mut Session,
        autonomy: Option<&PermissionMode>,
        allowed: &[Arc<dyn crate::providers::Tool>],
    ) -> anyhow::Result<ToolResult> {
        // Autonomy enforcement.
        if let Some(autonomy) = autonomy {
            if matches!(autonomy, PermissionMode::ReadOnly) && is_write_tool(&call.name) {
                tracing::info!(tool = %call.name, "tool blocked by ReadOnly autonomy policy");
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Tool '{}' is not allowed in read-only mode (autonomy: ReadOnly).",
                        call.name
                    ),
                    error: Some("autonomy_policy: ReadOnly".to_string()),
                });
            }

            // Default mode: per-operation approval gate for write tools.
            if matches!(autonomy, PermissionMode::Default)
                && is_write_tool(&call.name)
                && !Self::is_approval_exempt(&call.name)
            {
                match self.request_approval(call, session).await {
                    ApprovalDecision::Approved => {
                        tracing::info!(
                            tool = %call.name,
                            session = %session.id,
                            "write tool approved by user"
                        );
                    }
                    ApprovalDecision::Denied => {
                        tracing::info!(
                            tool = %call.name,
                            session = %session.id,
                            "write tool denied by user"
                        );
                        return Ok(ToolResult {
                            success: false,
                            output: format!(
                                "User denied execution of tool '{}'.",
                                call.name
                            ),
                            error: Some("user_denied".to_string()),
                        });
                    }
                    ApprovalDecision::Skipped => {
                        // No ask_router / no channel — fall through to execute.
                    }
                }
            }
        }

        // Generic tool dispatch against the filtered slice. `ask_user` /
        // `agent_delegate` are real Tool impls in `allowed` (no
        // special-casing).
        let tool = allowed
            .iter()
            .find(|t| t.spec().name == call.name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: '{}'", call.name))?;
        let args = parse_tool_args(&call.arguments);
        self.run_tool(tool.as_ref(), &call.name, args, session)
            .await
    }

    /// Tools exempt from the Default-mode approval gate even though
    /// `is_write_tool` classifies them as writes. `agent_delegate` and
    /// `agent_kill` manage sub-agent lifecycles, not user-facing side
    /// effects — blocking them on approval would deadlock delegation.
    fn is_approval_exempt(name: &str) -> bool {
        matches!(name, "agent_delegate" | "agent_kill")
    }

    /// Send an approval prompt with Inline Keyboard buttons and wait
    /// for the user's decision. Reuses `AskRouter` (same mechanism as
    /// `ask_user`): the orchestrator's `AskReply` interceptor fulfills
    /// the wait when the next inbound message (button click → text)
    /// arrives for this session.
    async fn request_approval(
        &self,
        call: &ToolCall,
        session: &Session,
    ) -> ApprovalDecision {
        let router = match &self.ask_router {
            Some(r) => r,
            None => return ApprovalDecision::Skipped,
        };
        let channel = match session.channel.as_ref() {
            Some(c) => c,
            None => return ApprovalDecision::Skipped,
        };
        let reply_target = match session.reply_target() {
            Some(rt) => rt.to_string(),
            None => return ApprovalDecision::Skipped,
        };

        // Build a concise preview of the tool call arguments.
        let args_preview = Self::format_args_preview(&call.arguments, 500);
        let text = format!(
            "🔒 **Approval Required**\n\nTool: `{}`\nArgs: `{}`\n\nAllow this action?",
            call.name, args_preview
        );

        let mut receiver = MessageReceiver::new(reply_target);
        if let Some(ref last_msg) = session.last_message {
            receiver.reply_to_message_id = Some(
                last_msg
                    .receiver
                    .reply_to_message_id
                    .clone()
                    .unwrap_or_else(|| last_msg.id.clone()),
            );
            receiver.thread_id = last_msg.receiver.thread_id.clone();
        }

        let message = ChannelOutboundMessage {
            receiver,
            content: ChannelMessageContent {
                text,
                files: vec![],
                buttons: vec![
                    InlineButton {
                        label: "✅ Approve".to_string(),
                        callback_data: "approve".to_string(),
                    },
                    InlineButton {
                        label: "❌ Deny".to_string(),
                        callback_data: "deny".to_string(),
                    },
                ],
            },
            options: Default::default(),
        };

        if let Err(e) = channel.send_message(&message).await {
            tracing::warn!(tool = %call.name, err = %e, "approval prompt send failed, skipping gate");
            return ApprovalDecision::Skipped;
        }

        tracing::info!(
            tool = %call.name,
            session = %session.id,
            "approval prompt sent, waiting for user decision"
        );

        match router.wait_for_reply(&session.id).await {
            Ok(reply) => {
                let decision = reply.content.text.trim().to_lowercase();
                if decision.starts_with("deny") || decision.contains("拒绝") || decision.contains("❌") {
                    ApprovalDecision::Denied
                } else {
                    // Default to approved: "approve", "allow", "yes", "允许", etc.
                    ApprovalDecision::Approved
                }
            }
            Err(e) => {
                tracing::warn!(tool = %call.name, err = %e, "approval wait failed, denying");
                ApprovalDecision::Denied
            }
        }
    }

    /// Truncate tool arguments to `max_chars` for the approval prompt.
    fn format_args_preview(args: &str, max_chars: usize) -> String {
        if args.len() <= max_chars {
            args.to_string()
        } else {
            format!("{}…", &args[..max_chars])
        }
    }

    /// Execute a tool with timeout and framework-level output truncation.
    async fn run_tool(
        &self,
        tool: &dyn crate::providers::Tool,
        name: &str,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let raw = if self.timeout_secs > 0 && !tool.blocks_on_human() {
            let effective_secs = tool
                .preferred_timeout_secs()
                .map(|p| p.max(self.timeout_secs))
                .unwrap_or(self.timeout_secs);
            let timeout = Duration::from_secs(effective_secs);
            tokio::time::timeout(timeout, tool.execute(args, session))
                .await
                .unwrap_or_else(|_| {
                    Ok(ToolResult {
                        success: false,
                        output: format!("Tool '{}' timed out after {}s", name, effective_secs),
                        error: Some("timeout".to_string()),
                    })
                })?
        } else {
            // No timeout: either disabled globally, or a human-blocking tool
            // (`ask_user`) whose wait is bounded by a real reply / interruption,
            // not an arbitrary timer.
            tool.execute(args, session).await?
        };

        let max_tokens = tool.max_output_tokens();
        let output = crate::tools::truncation::truncate_tool_result(&raw.output, max_tokens);
        if output.len() != raw.output.len() {
            tracing::debug!(
                tool = %name,
                original_len = raw.output.len(),
                truncated_len = output.len(),
                max_tokens,
                "tool output truncated by framework"
            );
        }
        Ok(ToolResult { output, ..raw })
    }
}

/// Outcome of the Default-mode approval gate.
enum ApprovalDecision {
    Approved,
    Denied,
    /// No router/channel available — skip the gate and execute.
    Skipped,
}

pub(crate) fn parse_tool_args(arguments: &str) -> serde_json::Value {
    if arguments.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({ "raw": arguments }))
    }
}

/// Restricted tool executor for the compaction summarizer.
///
/// Only allows file read/write/edit and shell — prevents the summarizer from
/// touching session state, triggering ask_user, or spawning sub-agents.
pub(crate) struct MemoryToolExecutor {
    tools: Arc<ToolRegistry>,
}

impl MemoryToolExecutor {
    /// Tools the summarizer is permitted to call during compaction: file
    /// read/write/edit, shell, and the dedicated `memory_manage` tool. The
    /// summarizer request still advertises the *full* tool list (for prefix-cache
    /// matching with the main request); this allow-list is the actual gate —
    /// any other tool call is blocked with an error result.
    const ALLOWED: &'static [&'static str] = &[
        "file_read",
        "file_write",
        "file_edit",
        "shell",
        "memory_manage",
    ];

    pub(crate) fn new(tools: Arc<ToolRegistry>) -> Self {
        Self { tools }
    }

    pub(crate) async fn execute(
        &self,
        call: &ToolCall,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        if !Self::ALLOWED.contains(&call.name.as_str()) {
            tracing::warn!(tool = %call.name, "summarizer tried to call restricted tool, blocking");
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "tool '{}' not available during compaction summarization",
                    call.name
                )),
            });
        }
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("tool '{}' not found in registry", call.name))?;
        let args = parse_tool_args(&call.arguments);
        tool.execute(args, session).await
    }
}
