//! Unified event type for the Orchestrator main loop.
//!
//! RFC v2 §三.C: the orchestrator's main loop selects across inbound channel
//! messages, scheduler ticks, delegation completions, and ask-user replies.
//! Adapter tasks fan each source into a single `mpsc<OrchestratorEvent>`
//! (see `Orchestrator::run`), so the dispatch logic is linear and tests
//! have a single injection point (`tx.send(OrchestratorEvent::*)`).

use crate::agents::DelegationEvent;
use crate::channels::ChannelMessage;

/// Anything the orchestrator's main loop needs to react to.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum OrchestratorEvent {
    /// A user message arrived on a channel. `account_key` identifies which
    /// (channel_type, account_id) pair received it, since one channel kind
    /// can have multiple accounts (e.g. two Telegram bots).
    Inbound {
        channel_type: String,
        account_id: String,
        message: ChannelMessage,
    },

    /// Scheduler fired — either a heartbeat tick or a cron job. Carried
    /// verbatim from the existing `SchedulerEvent` shape so the switch is
    /// a one-line conversion in E29.
    Scheduled(super::SchedulerEvent),

    /// Background sub-agent finished. The orchestrator synthesizes a
    /// `ChannelMessage` from this event and feeds it back into the parent
    /// session so the LLM can react.
    Delegation(DelegationEvent),

    /// Reply to an outstanding `ask_user` call. Tagged with the session_id
    /// that originated the question (RFC v2: indexed by session_id, not by
    /// routing_key, so cross-channel ask_user works for sub-agents).
    AskReply {
        session_id: String,
        reply: crate::channels::ChannelMessage,
    },

    /// Graceful shutdown signal — main loop should drain and exit.
    Shutdown,
}
