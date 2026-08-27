//! Session-key value types.
//!
//! Replaces ad-hoc `String` formatting/splitting of session keys scattered
//! across the orchestrator. A user session key is the 3-tuple
//! `channel_type:account_id:sender`; a sub-agent session key is the 2-tuple
//! `agent_name:sub_session_id` (deliberately a distinct type — it is *not* a
//! `SessionKey` with an empty field).

pub use crate::api::channel_registry::SessionKey;
