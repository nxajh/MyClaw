//! Ask user tool — pauses the agent to ask the user a question and waits for a reply.
//!
//! Reads the active channel via `Session::resolve_channel()` (registry
//! installed by `SessionContext::process_turn`), so there is no per-tool
//! channel map and no need to parse `ctx.owner`. The tool is wired
//! with the global `AskRouter` at construction; orchestrator's inbound
//! dispatch fulfills the wait via `AskRouter::fulfill(session_id, ...)`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::api::ask_fulfillment::AskFulfillment;
use crate::api::message::{ChannelMessageContent, ChannelOutboundMessage, MessageReceiver};
use crate::providers::{Tool, ToolResult};

pub struct AskUserTool {
    router: Arc<dyn AskFulfillment>,
}

impl AskUserTool {
    /// Construct an `ask_user` tool bound to the shared `AskRouter`.
    /// Orchestrator's inbound dispatch must use the *same* router so
    /// `fulfill(session_id, msg)` wakes the wait registered here.
    pub fn new<R: AskFulfillment + 'static>(router: Arc<R>) -> Self {
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
        ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

        // 方案 C (RFC §3.3): ask_user is disabled on silenced resume turns —
        // the user's reply would queue behind the turn lock while the turn
        // waits for it (deadlock). Guide the agent to fold the question into
        // the final summary or use /btw (bypass question, independent of
        // turn/context). Origin turns (suspension created mid-run) keep the
        // tool available — `turn_silenced` is false there.
        if ctx.turn_silenced {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "当前 turn 挂起中，无法提问：请将问题写入最终汇总，或使用 /btw 即时插话"
                        .to_string(),
                ),
            });
        }

        // RFC channel-role-split §1.1: headless turns (cron/webhook) have
        // no human on the other end — asking would hang the turn forever.
        // Report as a tool error instead (mirrors the silenced-turn guard).
        if ctx.turn_headless {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("后台轮无用户可提问：本轮由定时任务触发，没有对话对端。请基于现有信息自行决策，或把问题写入最终输出"
                    .to_string()),
            });
        }

        let channel = match ctx.channel.clone() {
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

        let reply_target = match ctx.reply_target.as_deref() {
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

        let mut receiver = MessageReceiver::new(reply_target);
        if let Some(ref last_msg) = ctx.last_message {
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
            content: ChannelMessageContent::text(question),
            options: Default::default(),
        };
        if let Err(e) = channel.send_outbound_message(&message).await {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("ask_user: send failed: {}", e)),
            });
        }

        // Register with the router and await the user's reply, keyed by
        // ctx.session_id (cross-channel sub-agents would route by session, not
        // routing_key, after future delegation work).
        let reply = match self.router.wait_for_reply(&ctx.session_id).await {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let output = reply.content.text;

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}
