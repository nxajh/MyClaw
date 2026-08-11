//! TurnStream — per-turn streaming output handle.
//!
//! See `docs/channel-model-rfc.md` §7.6.
//!
//! Replaces the `Channel::push_event` + `Channel::cancel_signal` pair with
//! a `Box<dyn TurnStream>` owned by `Session.turn_stream` for the duration
//! of one `Agent::run` invocation. Strengths:
//!
//! 1. Per-turn state ownership (no shared HashMap keyed by reply_target).
//! 2. Three-state delivery feedback (Pending/Visible/FinalDelivered) instead
//!    of binary `Result<()>`.
//! 3. Explicit lifecycle: `create_stream` → `push`* → `finish` / `abort`.
//! 4. Drop tracks "leak" as best-effort abort (impls SHOULD provide a Drop).

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::agents::TurnEvent;

/// Delivery state for an event pushed to a TurnStream.
///
/// Monotonic: once a stream reaches `FinalDelivered`, all subsequent
/// `status()` calls MUST return `FinalDelivered`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StreamDelivery {
    /// Buffered locally; not yet observed by the consumer.
    #[default]
    Pending,
    /// Delivered to the transport layer (e.g. WS send returned, HTTP 200).
    /// The consumer has the bytes but hasn't acknowledged completion.
    Visible,
    /// Consumer has acknowledged final delivery (e.g. client ack frame,
    /// Telegram's final editMessageText success).
    FinalDelivered,
}

/// A message a streaming turn already delivered that the caller may
/// repurpose (edit in place) instead of leaving it visible next to a second
/// message — OpenClaw single-message draft semantics: one message evolves
/// (output → preview → final), never output + preview side by side.
#[derive(Debug, Clone)]
pub struct FoldCandidate {
    /// Platform message id (e.g. Telegram message id).
    pub msg_id: String,
    /// Current body of that message (what the user sees right now).
    pub text: String,
}

/// A per-turn streaming output handle. Owned by `Session.turn_stream`;
/// dropped or `finish`ed when the turn ends.
///
/// `Send + Sync` because `Session` is held under `Arc<Mutex<…>>` and
/// crosses await points in scheduler/webhook paths.
#[async_trait]
pub trait TurnStream: Send + Sync {
    /// Push one event. Returns the current delivery state, or `Err` if
    /// the transport has permanently failed (caller should stop pushing
    /// but is NOT expected to fall back to `send_message`).
    async fn push(&mut self, event: TurnEvent) -> anyhow::Result<StreamDelivery>;

    /// Current cumulative delivery state. Read-only; does not trigger
    /// new work.
    fn status(&self) -> StreamDelivery;

    /// Graceful end-of-turn. Implementations SHOULD await ack with a
    /// best-effort timeout. Returns `FinalDelivered` on success,
    /// `Visible` on timeout / partial delivery.
    async fn finish(self: Box<Self>) -> StreamDelivery;

    /// Abort: cancel in-flight transmission; do not await ack.
    async fn abort(self: Box<Self>);

    /// Cancellation token observed by `Agent::run` for user-initiated
    /// turn cancel. Default `None` for streams without cancel support.
    fn cancel_token(&self) -> Option<CancellationToken> {
        None
    }

    /// Fold candidate: if this stream flushed a visible message, return its
    /// platform id + current body so the caller can repurpose it (e.g. edit
    /// it into a progress preview) instead of leaving it next to a second
    /// message. Default: none (non-message streams, nothing flushed yet, or
    /// the message was already deleted — e.g. partial mode past the 4096 cap).
    ///
    /// MUST be called before `finish()`/`abort()` (they consume `Box<Self>`).
    fn fold_candidate(&self) -> Option<FoldCandidate> {
        None
    }

    /// 单 preview (2026-08-12): mark this stream as an intermediate
    /// (silenced) resume turn of an async-delegation suspension. Its `Done`
    /// must KEEP the preview lines (append the turn's output as new lines,
    /// no collapse) so the whole flow stays one evolving message — the
    /// final collapse happens on the last resume turn. Default: no-op.
    fn defer_collapse(&mut self) {}
}
