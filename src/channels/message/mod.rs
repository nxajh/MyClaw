//! channels_message — Shared channel message types.

use async_trait::async_trait;

// ── L0 contract types (canonical defs in crate::api) ─────────────────────────

pub use crate::api::message::{
    CallbackAction, Channel, ChannelCapabilities, ChannelFile, ChannelFileBody, ChannelFileMeta,
    ChannelInboundMessage, ChannelMessageContent, ChannelOutboundMessage, GroupStat, InlineButton,
    LenUnit, LocalFileBody, MessageId, MessageReceiver, MessageSender, MINIMAL_CAPABILITIES,
    OutboundSendResult, PersistedChannelMessage, ProcessingStatus, SendOptions, ToolEvent,
};


// ── File types ──────────────────────────────────────────────────────────────





// ── ChannelMessageContent ───────────────────────────────────────────────────






// ── Core message types ─────────────────────────────────────────────────────────


// ── Callback actions (RFC §11 Phase 5) ─────────────────────────────────────────
// ProcessingStatus / ToolEvent moved to crate::api::message (#151 Phase 3c).

#[async_trait]
impl<T: ?Sized + Channel> crate::api::message::OutboundChannel for T {
    async fn send_outbound_message(
        &self,
        msg: &crate::api::message::ChannelOutboundMessage,
    ) -> anyhow::Result<crate::api::message::OutboundSendResult> {
        self.send_message(msg).await
    }
    fn supports_file_send(&self) -> bool {
        self.capabilities().supports_file_send
    }
}

// GroupStat moved to crate::api::message (#151 Phase 3c).

pub(crate) mod chunking;
pub(crate) mod model;

pub use chunking::{split_message_chunk, split_message_chunk_chars};
pub use model::DedupState;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_action_roundtrip_retry_abort() {
        let r = CallbackAction::Retry {
            session_key_prefix: "abc123".into(),
        };
        assert_eq!(r.serialize(), "__retry:abc123");
        assert_eq!(CallbackAction::parse("__retry:abc123"), Some(r));

        let a = CallbackAction::Abort {
            session_key_prefix: "xyz".into(),
        };
        assert_eq!(a.serialize(), "__abort:xyz");
        assert_eq!(CallbackAction::parse("__abort:xyz"), Some(a));
    }

    #[test]
    fn callback_action_custom_passthrough() {
        let c = CallbackAction::Custom {
            tag: "vote".into(),
            data: "yes".into(),
        };
        assert_eq!(c.serialize(), "__vote:yes");
        assert_eq!(CallbackAction::parse("__vote:yes"), Some(c));
    }

    #[test]
    fn callback_action_rejects_non_callback() {
        assert_eq!(CallbackAction::parse("hello world"), None);
        assert_eq!(CallbackAction::parse("__noseparator"), None);
        assert_eq!(CallbackAction::parse(""), None);
    }
}
