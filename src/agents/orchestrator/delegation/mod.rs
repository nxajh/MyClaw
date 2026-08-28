//! Delegation wakes.
//!
//! A sub-agent completing (or failing) a background task is a *system* event,
//! not a user message. We synthesize a system-note `ChannelInboundMessage` and
//! drive it into the parent session.
//!
//! ## Routing
//!
//! The `DelegationEvent.session_id` is a hex session ID, NOT a routing key.
//! We look up the session to find its `owner` (the routing key
//! `channel:account:sender`), then:
//!
//! - **Active session** — route through `inbound::dispatch_turn` so the turn
//!   streams to the user's UI in real time.
//! - **Non-active session** — load a temporary `SessionContext` (not
//!   registered in the table) and call `process_turn` directly with
//!   `channel=None`. The LLM processes the result and the response is
//!   persisted to history; the user sees it when they switch back.

mod dispatch;
mod notice;

pub(super) use dispatch::{
    drain_delegation_notices, notice_fallback_sender, notice_receiver, process_non_active,
};
pub(super) use notice::{route_notice, route_shell_completion, wake};

// Test-import forwarding（P0 webui/client 批 3 模式，同 migration/mod.rs）：tests.rs 的
// `use super::*` 从本模块命名空间取绑定；cfg(test) 门控避免非测试构建下的
// unused import（clippy -D warnings）。
#[cfg(test)]
use super::ctx::OrchestratorCtx;
#[cfg(test)]
use crate::agents::turn::SubStatus;
#[cfg(test)]
use crate::agents::{DelegationEvent, MessageKind};
#[cfg(test)]
use dispatch::split_batch_ids;
#[cfg(test)]
use notice::maybe_append_silence_guidance;

/// P1-4: `wake` routing tests — terminal events collect into the parent's
/// suspension (progress folding, degraded summary), Progress messages are
/// suppressed, and unknown sessions are a no-op. `test_ctx` has no channels,
/// so `route_notice` falls back to the spawned non-active path (NullRegistry
/// fails fast) and never blocks the assertions.
#[cfg(test)]
mod tests;
