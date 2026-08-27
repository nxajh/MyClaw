//! ask_fulfillment — L0 facade over `agents::ask_router::AskRouter`.
//!
//! #151 Phase 8+: `ask_user` (L3) only needs to *wait* on the shared
//! router; the fulfillment half (`AskRouter::fulfill`) stays inside the
//! agents layer where the orchestrator's inbound dispatch lives. The
//! concrete `AskRouter` implements this trait in the agents layer and the
//! composition root keeps passing its `Arc` unchanged.

use crate::api::message::ChannelInboundMessage;

#[async_trait::async_trait]
pub trait AskFulfillment: Send + Sync {
    /// Block until the user's reply for `session_id` arrives (fulfilled by
    /// the orchestrator's inbound dispatch via the same shared router).
    async fn wait_for_reply(&self, session_id: &str) -> anyhow::Result<ChannelInboundMessage>;
}
