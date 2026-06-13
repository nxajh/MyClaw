//! Ask user tool — pauses the agent to ask the user a question and waits for a reply.
//!
//! Reads the active channel from `session.channel` (the transient handle
//! installed by `SessionContext::process_turn`), so there is no per-tool
//! channel map and no need to parse `session.owner`. The tool is wired
//! with the global `AskRouter` at construction; orchestrator's inbound
//! dispatch fulfills the wait via `AskRouter::fulfill(session_id, ...)`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::agents::ask_router::AskRouter;
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};
use crate::providers::{Tool, ToolResult};

pub struct AskUserTool {
    router: Arc<AskRouter>,
}

impl AskUserTool {
    /// Construct an `ask_user` tool bound to the shared `AskRouter`.
    /// Orchestrator's inbound dispatch must use the *same* router so
    /// `fulfill(session_id, msg)` wakes the wait registered here.
    pub fn new(router: Arc<AskRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their response. Use this when you need clarification or confirmation before proceeding."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user."
                }
            },
            "required": ["question"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        1_000
    }

    /// `ask_user` waits on a human — exempt it from the executor's generic
    /// per-tool timeout (which is a watchdog for runaway compute tools).
    fn blocks_on_human(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

        let channel = match session.channel.as_ref() {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "ask_user requires an active channel on the session \
                         (sub-agent / scheduled paths have none)"
                            .to_string(),
                    ),
                });
            }
        };

        let reply_target = match session.reply_target() {
            Some(rt) => rt.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "ask_user requires an active reply_target on the session".to_string(),
                    ),
                });
            }
        };

        let message = ChannelOutboundMessage {
            receiver: MessageReceiver::new(reply_target),
            content: ChannelMessageContent::text(question),
            options: Default::default(),
        };
        if let Err(e) = channel.send_message(&message).await {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("ask_user: send failed: {}", e)),
            });
        }

        // Register with the router and await the user's reply, keyed by
        // session.id (cross-channel sub-agents would route by session, not
        // routing_key, after future delegation work).
        let reply = match self.router.wait_for_reply(&session.id).await {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        // Surface any attached images alongside the text answer so the
        // model sees the full reply (AskRouter delivers a ChannelMessage so
        // image attachments survive the round trip).
        let mut output = reply.content;
        if let Some(ref urls) = reply.image_urls {
            for url in urls {
                output.push_str("\n[image] ");
                output.push_str(url);
            }
        }
        if let Some(ref b64) = reply.image_base64 {
            if !b64.is_empty() {
                output.push_str(&format!("\n[{} inline image(s) attached]", b64.len()));
            }
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}
