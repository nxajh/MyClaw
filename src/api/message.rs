//! api_message — Shared channel message types (L0 contract layer).
//!
//! These types are the shared contracts between channels, agents, and tools.
//! They are defined here to break the `agents→channels` and `tools→agents`
//! dependency violations.

use serde::{Deserialize, Serialize};

/// Identity of the party that sent a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSender {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl MessageSender {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
        }
    }
}

/// Where to deliver a channel message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReceiver {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
}

impl MessageReceiver {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            thread_id: None,
            reply_to_message_id: None,
        }
    }

    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    pub fn with_reply_to(mut self, message_id: impl Into<String>) -> Self {
        self.reply_to_message_id = Some(message_id.into());
        self
    }
}

/// Serializable inbound context kept on sessions. File bodies are runtime-only
/// and must not be persisted here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedChannelMessage {
    pub id: String,
    pub sender_id: String,
    pub receiver: MessageReceiver,
    pub text: String,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
}
