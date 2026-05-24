//! Ask user tool — pauses the agent loop to ask the user a question and waits for a response.
//!
//! F35: upgraded from fallback to a real Tool impl backed by AskRouter.
//! When an AskRouter is wired in, the tool sends the question through the
//! session's channel and awaits the user's next message via a oneshot.
//! Falls back to inline text when no router is available (CLI / tests).

use async_trait::async_trait;
use std::sync::Arc;
use std::collections::HashMap;
use crate::channels::Channel;
use crate::channels::message::SendMessage;
use crate::providers::{Tool, ToolResult};
use crate::agents::ask_router::AskRouter;
use serde_json::json;

type ChannelMap = Arc<parking_lot::RwLock<HashMap<(String, String), Arc<dyn Channel>>>>;

/// AskUserTool — sends a question to the user and blocks until they answer.
pub struct AskUserTool {
    router: Option<Arc<AskRouter>>,
    channels: Option<ChannelMap>,
}

impl AskUserTool {
    /// Create the fallback version (CLI / tests — no channel delivery).
    pub fn new() -> Self {
        Self { router: None, channels: None }
    }

    /// Create the production version with AskRouter and channel map.
    pub fn with_router(router: Arc<AskRouter>, channels: ChannelMap) -> Self {
        Self {
            router: Some(router),
            channels: Some(channels),
        }
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
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

    async fn execute(&self, args: serde_json::Value, session: &crate::agents::session::Session) -> anyhow::Result<ToolResult> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

        // If no router wired, fall back to inline text.
        let router = match &self.router {
            Some(r) => r,
            None => {
                return Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Please answer this question: {} (Note: direct channel delivery is not available, \
                         please respond in the conversation.)",
                        question
                    ),
                    error: None,
                });
            }
        };

        let reply_target = session.reply_target()
            .ok_or_else(|| anyhow::anyhow!("no reply_target on session, cannot ask_user"))?;

        // Register pending ask and get the receiver.
        let rx = router.register(&session.id, reply_target.to_string());

        // Send the question through the channel.
        if let Some(ref channels) = self.channels {
            // Try to find the channel from session's transient channel field first.
            let sent = if let Some(ref ch) = session.channel {
                let msg = SendMessage::new(question, reply_target.to_string());
                ch.send(&msg).await.is_ok()
            } else {
                // Try the first channel (clone Arc before await to release lock).
                let mut sent = false;
                let ch = {
                    let guard = channels.read();
                    guard.values().next().cloned()
                };
                if let Some(ch) = ch {
                    let msg = SendMessage::new(question, reply_target.to_string());
                    sent = ch.send(&msg).await.is_ok();
                }
                sent
            };

            if !sent {
                tracing::warn!("ask_user: could not send question through any channel");
                router.cancel(&session.id);
                return Ok(ToolResult {
                    success: false,
                    output: "Failed to deliver question to user.".to_string(),
                    error: Some("channel_send_failed".to_string()),
                });
            }
        }

        // Wait for the user's reply (with timeout).
        match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
            Ok(Ok(answer)) => Ok(ToolResult {
                success: true,
                output: answer,
                error: None,
            }),
            Ok(Err(_)) => Ok(ToolResult {
                success: false,
                output: "ask_user cancelled".to_string(),
                error: Some("cancelled".to_string()),
            }),
            Err(_) => {
                router.cancel(&session.id);
                Ok(ToolResult {
                    success: false,
                    output: "ask_user timed out after 300s".to_string(),
                    error: Some("timeout".to_string()),
                })
            }
        }
    }
}
