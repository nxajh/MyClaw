//! Shared HTTP utilities for provider implementations.

use reqwest::Client;
use std::time::Duration;

/// Build a reqwest Client suitable for streaming LLM responses.
///
/// No overall request timeout — per-chunk timeouts are enforced at the
/// application layer by `collect_stream_inner`.  We do configure:
/// - `connect_timeout`: fail fast when the remote is unreachable
/// - `tcp_keepalive`: OS-level probes detect half-open (stale) connections
/// - `pool_idle_timeout`: expire idle pooled connections before they go stale
pub fn build_reqwest_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest client must build")
}