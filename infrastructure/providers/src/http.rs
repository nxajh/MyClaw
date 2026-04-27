//! Shared HTTP utilities for provider implementations.

use reqwest::Client;
use std::time::Duration;

/// Build a reqwest Client with appropriate timeouts.
pub fn build_reqwest_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("reqwest client must build")
}