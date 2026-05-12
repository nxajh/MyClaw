//! Structured API error classification for smart failover and recovery.
//!
//! Provides a taxonomy of API errors and a classification pipeline that
//! determines the correct recovery action (retry, rotate credential,
//! fallback to another provider, compress context, or abort).

use std::time::Duration;

/// Why an API call failed — determines recovery strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverReason {
    /// Transient auth failure (401/403) — refresh/rotate.
    Auth,
    /// Auth failed after refresh — abort.
    AuthPermanent,
    /// Billing exhausted (402 / confirmed credit depletion) — rotate immediately.
    Billing,
    /// Rate limit (429 / quota-based throttling) — backoff then rotate.
    RateLimit,
    /// Provider overloaded (503/529) — backoff.
    Overloaded,
    /// Internal server error (500/502) — retry.
    ServerError,
    /// Connection/read timeout — rebuild client + retry.
    Timeout,
    /// Context too large — compress, not failover.
    ContextOverflow,
    /// Payload too large (413) — compress payload.
    PayloadTooLarge,
    /// Model not found (404) — fallback to different model.
    ModelNotFound,
    /// Bad request (400) — abort or strip + retry.
    FormatError,
    /// Unclassifiable — retry with backoff.
    Unknown,
}

/// Structured classification of an API error with recovery hints.
#[derive(Debug, Clone)]
pub struct ClassifiedError {
    pub reason: FailoverReason,
    pub status_code: Option<u16>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub message: String,
    /// Whether the error is transient and retry may succeed.
    pub retryable: bool,
    /// Whether to trigger context compression before retry.
    pub should_compress: bool,
    /// Whether to rotate to the next credential before retry.
    pub should_rotate_credential: bool,
    /// Whether to failover to the next provider in the chain.
    pub should_fallback: bool,
    /// Cooldown duration before retrying this credential.
    pub cooldown: Option<Duration>,
}

impl ClassifiedError {
    /// Quick constructor for the common case.
    pub fn new(reason: FailoverReason, message: impl Into<String>) -> Self {
        let (retryable, should_compress, should_rotate, should_fallback, cooldown) =
            Self::recovery_hints(&reason);
        Self {
            reason,
            status_code: None,
            provider: None,
            model: None,
            message: message.into(),
            retryable,
            should_compress,
            should_rotate_credential: should_rotate,
            should_fallback,
            cooldown,
        }
    }

    /// Classify from an HTTP status code and optional error message.
    pub fn from_http(status: u16, message: Option<&str>) -> Self {
        let reason = Self::classify_status(status, message);
        let mut err = Self::new(reason.clone(), message.unwrap_or(""));
        err.status_code = Some(status);
        err
    }

    /// Classify from a raw error string (used when status code is unavailable).
    pub fn from_message(message: &str) -> Self {
        let reason = Self::classify_message(message);
        Self::new(reason, message)
    }

    /// Set provider/model metadata (builder style).
    pub fn with_provider(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self
    }

    /// Returns true if this is an auth-related error.
    pub fn is_auth(&self) -> bool {
        matches!(self.reason, FailoverReason::Auth | FailoverReason::AuthPermanent)
    }

    // ── Internal classification logic ────────────────────────────────────────

    fn classify_status(status: u16, message: Option<&str>) -> FailoverReason {
        match status {
            400 => {
                let msg = message.unwrap_or("").to_lowercase();
                if msg.contains("context") && msg.contains("length") {
                    FailoverReason::ContextOverflow
                } else if msg.contains("payload") || msg.contains("too large") {
                    FailoverReason::PayloadTooLarge
                } else {
                    FailoverReason::FormatError
                }
            }
            401 | 403 => FailoverReason::Auth,
            402 => FailoverReason::Billing,
            404 => FailoverReason::ModelNotFound,
            413 => FailoverReason::PayloadTooLarge,
            429 => FailoverReason::RateLimit,
            500 | 502 => FailoverReason::ServerError,
            503 | 529 => FailoverReason::Overloaded,
            504 => FailoverReason::Timeout,
            _ => {
                if let Some(msg) = message {
                    Self::classify_message(msg)
                } else {
                    FailoverReason::Unknown
                }
            }
        }
    }

    fn classify_message(message: &str) -> FailoverReason {
        let lower = message.to_lowercase();

        // Billing patterns
        if Self::matches_any(&lower, &[
            "insufficient credits", "insufficient_quota", "insufficient balance",
            "credit balance", "credits have been exhausted", "top up your credits",
            "payment required", "billing hard limit", "exceeded your current quota",
            "account is deactivated", "plan does not include",
        ]) {
            return FailoverReason::Billing;
        }

        // Rate limit patterns
        if Self::matches_any(&lower, &[
            "rate limit", "rate_limit", "too many requests", "throttled",
            "requests per minute", "tokens per minute", "requests per day",
            "try again in", "please retry after", "resource_exhausted",
            "rate increased too quickly", "throttlingexception",
            "too many concurrent requests", "servicequotaexceededexception",
        ]) {
            return FailoverReason::RateLimit;
        }

        // Auth patterns
        if Self::matches_any(&lower, &[
            "invalid api key", "authentication", "unauthorized",
            "invalid token", "api key invalid",
        ]) {
            return FailoverReason::Auth;
        }

        // Context overflow
        if Self::matches_any(&lower, &[
            "context_length_exceeded", "maximum context length",
            "context window", "token limit exceeded", "too many tokens",
        ]) {
            return FailoverReason::ContextOverflow;
        }

        // Timeout
        if Self::matches_any(&lower, &[
            "timeout", "timed out", "connection refused", "connection reset",
            "eof while", "broken pipe",
        ]) {
            return FailoverReason::Timeout;
        }

        // Server error
        if Self::matches_any(&lower, &[
            "internal server error", "bad gateway", "service unavailable",
        ]) {
            return FailoverReason::ServerError;
        }

        // Model not found
        if lower.contains("model not found") || lower.contains("invalid model") {
            return FailoverReason::ModelNotFound;
        }

        FailoverReason::Unknown
    }

    fn recovery_hints(
        reason: &FailoverReason,
    ) -> (bool, bool, bool, bool, Option<Duration>) {
        match reason {
            FailoverReason::Auth => (true, false, true, true, Some(Duration::from_secs(5 * 60))),
            FailoverReason::AuthPermanent => (false, false, false, false, None),
            FailoverReason::Billing => (true, false, true, true, Some(Duration::from_secs(24 * 3600))),
            FailoverReason::RateLimit => (true, false, true, true, Some(Duration::from_secs(60 * 60))),
            FailoverReason::Overloaded => (true, false, false, true, Some(Duration::from_secs(30))),
            FailoverReason::ServerError => (true, false, false, true, Some(Duration::from_secs(5))),
            FailoverReason::Timeout => (true, false, false, true, Some(Duration::from_secs(10))),
            FailoverReason::ContextOverflow => (true, true, false, false, None),
            FailoverReason::PayloadTooLarge => (true, true, false, false, None),
            FailoverReason::ModelNotFound => (false, false, false, true, None),
            FailoverReason::FormatError => (false, false, false, false, None),
            FailoverReason::Unknown => (false, false, false, true, Some(Duration::from_secs(5))),
        }
    }

    fn matches_any(text: &str, patterns: &[&str]) -> bool {
        patterns.iter().any(|p| text.contains(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_429_rate_limit() {
        let err = ClassifiedError::from_http(429, Some("rate limit exceeded"));
        assert_eq!(err.reason, FailoverReason::RateLimit);
        assert!(err.retryable);
        assert!(err.should_rotate_credential);
        assert!(err.should_fallback);
        assert!(!err.should_compress);
    }

    #[test]
    fn classify_402_billing() {
        let err = ClassifiedError::from_http(402, Some("insufficient credits"));
        assert_eq!(err.reason, FailoverReason::Billing);
        assert!(err.retryable);
        assert!(err.should_rotate_credential);
        assert_eq!(err.cooldown, Some(Duration::from_secs(24 * 3600)));
    }

    #[test]
    fn classify_400_context_overflow() {
        let err = ClassifiedError::from_http(400, Some("context_length_exceeded"));
        assert_eq!(err.reason, FailoverReason::ContextOverflow);
        assert!(err.retryable);
        assert!(err.should_compress);
        assert!(!err.should_fallback);
    }

    #[test]
    fn classify_message_billing() {
        let err = ClassifiedError::from_message("credit balance too low");
        assert_eq!(err.reason, FailoverReason::Billing);
    }

    #[test]
    fn classify_message_timeout() {
        let err = ClassifiedError::from_message("connection timed out");
        assert_eq!(err.reason, FailoverReason::Timeout);
        assert!(err.retryable);
    }

    #[test]
    fn auth_permanent_not_retryable() {
        let err = ClassifiedError::new(FailoverReason::AuthPermanent, "revoked");
        assert!(!err.retryable);
        assert!(!err.should_fallback);
    }
}
