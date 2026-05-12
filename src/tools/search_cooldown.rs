//! Per-provider cooldown tracking for search providers.
//!
//! When a search provider fails (rate limit, overload, etc.), it's marked
//! with a cooldown duration. Subsequent requests skip providers whose
//! cooldown hasn't expired, falling through to the next provider in chain.

use regex::Regex;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::providers::{ClassifiedError, FailoverReason};

/// Default cooldown duration (30 minutes) when no specific retry-after is available.
const DEFAULT_COOLDOWN_SECS: u64 = 1800;

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

    /// Record a cooldown for the given provider using a specific duration.
    pub fn record_failure_with_cooldown(
        &self,
        provider_name: &str,
        cooldown: Duration,
    ) {
        self.record(provider_name, cooldown);
    }

    /// Record a cooldown for the given provider with the default duration.
    pub fn record_failure(&self, provider_name: &str) {
        self.record_failure_with_cooldown(provider_name, Duration::from_secs(DEFAULT_COOLDOWN_SECS));
    }

    /// Classify a search provider error and record cooldown if appropriate.
    /// Returns the classified error reason.
    ///
    /// First tries to parse a specific cooldown duration from the response body
    /// (e.g. `retry_after` JSON field, "try again in X seconds" text).
    /// Falls back to the default cooldown for the classified error type.
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

        // Try to extract a specific cooldown from the response body first.
        // This gives more precise retry timing than the default per-error-type cooldowns.
        let cooldown = parse_search_cooldown(body)
            .or(classified.cooldown);

        if let Some(duration) = cooldown {
            self.record(provider_name, duration);
        }

        tracing::warn!(
            provider = provider_name,
            reason = ?classified.reason,
            retryable = classified.retryable,
            should_fallback = classified.should_fallback,
            cooldown_secs = ?cooldown.map(|d| d.as_secs()),
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

/// Try to extract a specific cooldown duration from a search API response body.
///
/// Extraction priority:
/// 1. JSON body `retry_after` field (seconds)
/// 2. JSON body `retry-after` field (seconds)
/// 3. Nested `error.retry_after` (seconds)
/// 4. Text regex: `retry.*(after|in)\s+(\d+)\s*s`
/// 5. Text regex: `try again in (\d+) seconds?`
/// 6. Rate-limit keywords (`too many requests`, `rate limit`, `quota exceeded`) → 3600s
/// 7. No match → None (caller should use default cooldown)
pub fn parse_search_cooldown(body: &str) -> Option<Duration> {
    if body.is_empty() {
        return None;
    }

    // --- JSON-based extraction ---
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        // Top-level retry_after (number)
        if let Some(secs) = extract_json_seconds(&json, "retry_after") {
            return Some(secs);
        }
        // Top-level retry-after (number)
        if let Some(secs) = extract_json_seconds(&json, "retry-after") {
            return Some(secs);
        }
        // Nested error.retry_after
        if let Some(error) = json.get("error") {
            if let Some(secs) = extract_json_seconds(error, "retry_after") {
                return Some(secs);
            }
            if let Some(secs) = extract_json_seconds(error, "retry-after") {
                return Some(secs);
            }
        }
    }

    // --- Text-based extraction ---
    // Pattern: "retry after 60s" / "retry in 120 seconds" etc.
    if let Ok(re) = Regex::new(r"(?i)retry\s+(?:after|in)\s+(\d+)\s*s") {
        if let Some(secs) = extract_regex_seconds(&re, body) {
            return Some(secs);
        }
    }

    // Pattern: "try again in 60 seconds"
    if let Ok(re) = Regex::new(r"(?i)try again in\s+(\d+)\s*seconds?") {
        if let Some(secs) = extract_regex_seconds(&re, body) {
            return Some(secs);
        }
    }

    // Pattern: generic rate-limit keywords → 3600s fallback
    let lower = body.to_lowercase();
    if lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("quota exceeded")
        || lower.contains("quota_exceeded")
    {
        return Some(Duration::from_secs(3600));
    }

    None
}

/// Helper: extract a JSON numeric field as `Duration::from_secs`.
fn extract_json_seconds(json: &serde_json::Value, key: &str) -> Option<Duration> {
    let val = json.get(key)?;
    // Accept integer or float seconds.
    let secs = if let Some(n) = val.as_u64() {
        n
    } else if let Some(f) = val.as_f64() {
        if f > 0.0 { f as u64 } else { return None; }
    } else {
        return None;
    };
    if secs > 0 { Some(Duration::from_secs(secs)) } else { None }
}

/// Helper: extract the first capture group as seconds from a regex match.
fn extract_regex_seconds(re: &Regex, text: &str) -> Option<Duration> {
    let caps = re.captures(text)?;
    let secs = caps.get(1)?.as_str().parse::<u64>().ok()?;
    if secs > 0 { Some(Duration::from_secs(secs)) } else { None }
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

    #[test]
    fn record_failure_uses_default_cooldown() {
        let cd = SearchProviderCooldown::new();
        cd.record_failure("glm");
        assert!(cd.is_cooled_down("glm"));
    }

    #[test]
    fn record_failure_with_custom_cooldown() {
        let cd = SearchProviderCooldown::new();
        cd.record_failure_with_cooldown("google", Duration::from_secs(60));
        assert!(cd.is_cooled_down("google"));
    }

    // ── parse_search_cooldown tests ────────────────────────────────────────

    #[test]
    fn parse_cooldown_json_retry_after_field() {
        let body = r#"{"retry_after": 120, "error": "rate limited"}"#;
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(120));
    }

    #[test]
    fn parse_cooldown_json_retry_after_hyphen() {
        let body = r#"{"retry-after": 90}"#;
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(90));
    }

    #[test]
    fn parse_cooldown_json_nested_error_retry_after() {
        let body = r#"{"error": {"code": 429, "retry_after": 200}}"#;
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(200));
    }

    #[test]
    fn parse_cooldown_json_retry_after_as_float() {
        let body = r#"{"retry_after": 45.5}"#;
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(45));
    }

    #[test]
    fn parse_cooldown_json_zero_ignored() {
        let body = r#"{"retry_after": 0}"#;
        // zero is not a valid cooldown
        assert!(parse_search_cooldown(body).is_none());
    }

    #[test]
    fn parse_cooldown_text_retry_after_seconds() {
        let body = "Error: rate limit exceeded, retry after 60s";
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(60));
    }

    #[test]
    fn parse_cooldown_text_retry_in_seconds() {
        let body = "Please retry in 30 seconds";
        // matches "retry in 30 seconds" → "retry in 30s"
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(30));
    }

    #[test]
    fn parse_cooldown_text_try_again() {
        let body = "Too many requests. Try again in 120 seconds.";
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(120));
    }

    #[test]
    fn parse_cooldown_text_rate_limit_keywords() {
        let body = "too many requests";
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(3600));
    }

    #[test]
    fn parse_cooldown_text_quota_exceeded() {
        let body = "quota exceeded for this API";
        let dur = parse_search_cooldown(body).unwrap();
        assert_eq!(dur, Duration::from_secs(3600));
    }

    #[test]
    fn parse_cooldown_empty_body() {
        assert!(parse_search_cooldown("").is_none());
    }

    #[test]
    fn parse_cooldown_no_match() {
        let body = "internal server error";
        assert!(parse_search_cooldown(body).is_none());
    }

    #[test]
    fn classify_and_record_uses_body_cooldown() {
        // GLM 429 error with retry_after in JSON body
        let cd = SearchProviderCooldown::new();
        let msg = r#"GLM web_search HTTP 429: {"retry_after": 45, "error": "rate limited"}"#;
        let reason = cd.classify_and_record("glm", msg);
        assert_eq!(reason, FailoverReason::RateLimit);
        assert!(cd.is_cooled_down("glm"));
    }

    #[test]
    fn classify_and_record_falls_back_to_default_cooldown() {
        // GLM 429 error without retry_after — should use ClassifiedError default (1hr)
        let cd = SearchProviderCooldown::new();
        let msg = r#"GLM web_search HTTP 429: {"error": {"code": "1312", "message": "rate limited"}}"#;
        let reason = cd.classify_and_record("glm", msg);
        assert_eq!(reason, FailoverReason::RateLimit);
        assert!(cd.is_cooled_down("glm"));
    }
}
