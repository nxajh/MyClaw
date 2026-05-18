//! Gateway network types and session state for QQ Bot WebSocket.

use serde::Deserialize;

/// Session state for WebSocket Resume.
#[derive(Clone)]
pub struct SessionState {
    pub session_id: String,
    pub last_seq: u64,
}

/// Result of a WebSocket disconnection, used by ws_loop to decide reconnect strategy.
pub enum WsDisconnect {
    /// Normal disconnect or unknown close code — reconnect with fresh Identify.
    Clean,
    /// Should try Resume (e.g. server-initiated Reconnect opcode).
    TryResume,
    /// Fatal — do not reconnect (e.g. close codes 4914/4915).
    Fatal,
    /// Token-related — refresh token before reconnecting.
    TokenExpired,
}

#[derive(Debug, Deserialize)]
pub struct GatewayPayload {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub op: u32,
    #[serde(default)]
    pub s: Option<u64>,
    #[serde(default)]
    pub t: Option<String>,
    #[serde(default)]
    pub d: serde_json::Value,
}
