//! Per-provider cooldown tracking for search providers.
//!
//! When a search provider fails (rate limit, overload, etc.), it's marked
//! with a cooldown duration. Subsequent requests skip providers whose
//! cooldown hasn't expired, falling through to the next provider in chain.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::providers::{ClassifiedError, FailoverReason};

/// Tracks per-provider cooldown state for search fallback.
pub struct SearchProviderCooldown {
    inner: Mutex<HashMap<String, Instant>>,
}

impl SearchProviderCooldown {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if the provider is still in cooldown (should skip).
    pub fn is_cooled_down(&self, provider_name: &str) -> bool {
        let map = self.inner.lock().unwrap();
        if let Some(until) = map.get(provider_name) {
            if Instant::now() < *until {
                return true;
            }
        }
        false
    }

    /// Record a cooldown for the given provider.
    pub fn record(&self, provider_name: &str, duration: Duration) {
        let mut map = self.inner.lock().unwrap();
        let until = Instant::now() + duration;
        // Only extend cooldown, never shorten it.
        map.entry(provider_name.to_string())
            .and_modify(|e| if *e < until { *e = until })
            .or_insert(until);
        tracing::info!(
            provider = provider_name,
            cooldown_secs = duration.as_secs(),
            "search provider cooldown recorded"
        );
    }

    /// Classify a search provider error and record cooldown if appropriate.
    /// Returns the classified error reason.
    pub fn classify_and_record(
        &self,
        provider_name: &str,
        error_msg: &str,
    ) -> FailoverReason {
        // Parse HTTP status code from error message.
        // Provider errors follow patterns like:
        //   "GLM web_search HTTP 429: {body}"
        //   "Google Gemini HTTP 503 Service Unavailable: {body}"
        //   "MiniMax search HTTP 401: {body}"
        let (status, body) = parse_http_error(error_msg);

        let classified = if let Some(code) = status {
            ClassifiedError::from_http(code, Some(body))
        } else {
            ClassifiedError::from_message(error_msg)
        };

        // Only record cooldown for retryable/fallback-worthy errors with a cooldown.
        if let Some(cooldown) = classified.cooldown {
            self.record(provider_name, cooldown);
        }

        tracing::warn!(
            provider = provider_name,
            reason = ?classified.reason,
            retryable = classified.retryable,
            should_fallback = classified.should_fallback,
            "search error classified: {}", error_msg
        );

        classified.reason
    }
}

/// Parse "HTTP {status}: {body}" from provider error messages.
fn parse_http_error(msg: &str) -> (Option<u16>, &str) {
    // Match patterns like "HTTP 429:" or "HTTP 503 Service Unavailable:"
    if let Some(pos) = msg.find("HTTP ") {
        let rest = &msg[pos + 5..];
        // Extract status code (first 3 digits)
        if rest.len() >= 3 {
            if let Ok(code) = rest[..3].parse::<u16>() {
                // Body starts after "HTTP {code}: " or "HTTP {code} ...: "
                let body = rest.find(':').map(|i| rest[i + 1..].trim()).unwrap_or("");
                return (Some(code), body);
            }
        }
    }
    (None, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_glm_error() {
        let msg = "GLM web_search HTTP 429: {\"error\":{\"code\":\"1312\",\"message\":\"该模型当前访问量过大\"}}";
        let (status, body) = parse_http_error(msg);
        assert_eq!(status, Some(429));
        assert!(body.contains("1312"));
    }

    #[test]
    fn parse_google_error() {
        let msg = "Google Gemini HTTP 503 Service Unavailable: {\"error\":{\"code\":503,\"message\":\"overloaded\"}}";
        let (status, body) = parse_http_error(msg);
        assert_eq!(status, Some(503));
        assert!(body.contains("overloaded"));
    }

    #[test]
    fn parse_no_status() {
        let msg = "connection timed out";
        let (status, _) = parse_http_error(msg);
        assert_eq!(status, None);
    }

    #[test]
    fn cooldown_skips_provider() {
        let cd = SearchProviderCooldown::new();
        assert!(!cd.is_cooled_down("google"));

        cd.record("google", Duration::from_secs(300));
        assert!(cd.is_cooled_down("google"));
        assert!(!cd.is_cooled_down("glm"));
    }

    #[test]
    fn cooldown_extends_never_shortens() {
        let cd = SearchProviderCooldown::new();
        cd.record("google", Duration::from_secs(300));
        let first_until = cd.inner.lock().unwrap()["google"];

        cd.record("google", Duration::from_secs(10)); // shorter — should be ignored
        let second_until = cd.inner.lock().unwrap()["google"];
        assert_eq!(first_until, second_until);

        cd.record("google", Duration::from_secs(600)); // longer — should extend
        let third_until = cd.inner.lock().unwrap()["google"];
        assert!(third_until > second_until);
    }
}
