//! Ask user tool — pauses the agent to ask the user a question and waits for a reply.
//!
//! Two modes, selected by which constructor the daemon (or test) calls:
//!
//! - **Real mode** (`AskUserTool::with_router`): holds `Arc<AskRouter>` plus
//!   a channel map keyed by `(channel_type, account_id)`. On `execute`, the
//!   tool sends the question via the session's channel using the stored
//!   reply_target, registers a oneshot with the router keyed by the
//!   session's id, awaits the user's reply, and returns it as tool output.
//!   This is the RFC v2 §三.B replacement for the legacy
//!   `AskUserHandler` closure threaded into `ToolExecutor` (H49 deletion
//!   target once everywhere uses this).
//!
//! - **Fallback mode** (`AskUserTool::new` / `Default`): no router / no
//!   channel. Returns the question to the LLM with a note so the agent
//!   surfaces it in its own reply. Used in CLI mode, tests, and as a
//!   stop-gap until the orchestrator (E29) wires the AskRouter.
//!
//! Until the orchestrator's inbound dispatch calls
//! `AskRouter::fulfill(session_id, content)` ahead of `process_turn`, the
//! Real mode here will hang waiting for a reply that never arrives.
//! Construct it once E29 lands; until then keep using `new()`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::json;

use crate::agents::ask_router::AskRouter;
use crate::channels::{Channel, SendMessage};
use crate::providers::{Tool, ToolResult};

/// Mirror of `Orchestrator::ASK_USER_TIMEOUT`. Kept here so the tool is
/// self-contained.
const ASK_USER_TIMEOUT: Duration = Duration::from_secs(300);

/// Map of (channel_type, account_id) → Channel adapter. Matches the
/// orchestrator's `channels` DashMap so the same Arc can be threaded in.
pub type ChannelMap = Arc<DashMap<(String, String), Arc<dyn Channel>>>;

pub struct AskUserTool {
    router: Option<Arc<AskRouter>>,
    channels: Option<ChannelMap>,
}

impl AskUserTool {
    /// Fallback constructor: no real-channel ask, just echo the question.
    pub fn new() -> Self {
        Self { router: None, channels: None }
    }

    /// Real constructor wired to `AskRouter` + channel map. F35 entry
    /// point — daemon registers this once E29 hands the orchestrator's
    /// `AskRouter` over to the tool registry.
    pub fn with_router(router: Arc<AskRouter>, channels: ChannelMap) -> Self {
        Self { router: Some(router), channels: Some(channels) }
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a routing_key (`"channel:account:sender"`) into its three parts.
/// Lifted from `orchestrator.rs::parse_session_key` so the tool is
/// self-contained. Returns `None` for malformed keys.
fn split_routing_key(rk: &str) -> Option<(&str, &str, &str)> {
    let mut parts = rk.splitn(3, ':');
    let c = parts.next()?;
    let a = parts.next()?;
    let s = parts.next()?;
    if c.is_empty() || a.is_empty() || s.is_empty() {
        return None;
    }
    Some((c, a, s))
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

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'question' is required"))?;

        // Fallback mode: no router / no channel. Return the question so
        // the LLM surfaces it inline.
        let (router, channels) = match (&self.router, &self.channels) {
            (Some(r), Some(c)) => (r, c),
            _ => {
                return Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Please answer this question: {} (Note: direct channel \
                         delivery is not available, please respond in the \
                         conversation.)",
                        question
                    ),
                    error: None,
                });
            }
        };

        // Real mode requires the session to have a routable target.
        let reply_target = match session.reply_target() {
            Some(rt) => rt.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "ask_user requires an active reply_target on the session"
                            .to_string(),
                    ),
                });
            }
        };

        // Parse the session's owner to find the channel adapter. The owner
        // field is the routing_key ("channel:account:sender").
        let (ch_type, acc_id, _) = match split_routing_key(&session.owner) {
            Some(triple) => triple,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "ask_user: malformed routing_key on session: '{}'",
                        session.owner
                    )),
                });
            }
        };
        let channel: Arc<dyn Channel> = match channels
            .get(&(ch_type.to_string(), acc_id.to_string()))
            .map(|r| r.clone())
        {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "ask_user: channel '{}:{}' not found",
                        ch_type, acc_id
                    )),
                });
            }
        };

        // Send the question via the channel.
        let send_msg = SendMessage::new(question, reply_target.clone());
        if let Err(e) = channel.send(&send_msg).await {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("ask_user: send failed: {}", e)),
            });
        }

        // Register with the router and await the user's reply. Index by
        // session.id so cross-channel sub-agents work post-E29.
        let rx = router.register(&session.id, reply_target);
        let reply = match tokio::time::timeout(ASK_USER_TIMEOUT, rx).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("ask_user: receiver cancelled".to_string()),
                });
            }
            Err(_) => {
                // Timeout — clean up the pending entry.
                router.cancel(&session.id);
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "ask_user: timed out after {}s",
                        ASK_USER_TIMEOUT.as_secs()
                    )),
                });
            }
        };

        // Surface any attached images alongside the text answer so the
        // model sees the full reply. RFC §三.B: AskRouter delivers a
        // ChannelMessage so image attachments survive the round trip.
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
