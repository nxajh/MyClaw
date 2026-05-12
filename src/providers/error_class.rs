//! Structured API error classification for smart failover and recovery.
//!
//! Provides a three-layer classification pipeline:
//! 1. **HTTP status code** → broad category (401→Auth, 503→Overloaded, …)
//! 2. **Provider-specific business codes** → fine-grained category
//!    (GLM code 1312→Overloaded, OpenAI insufficient_quota→Billing, …)
//! 3. **Fallback** → Timeout when no HTTP status; FormatError/RateLimit default

use std::fmt;
use std::time::Duration;

// ── ErrorCategory ────────────────────────────────────────────────────────

/// Error category — determines recovery strategy via [`RecoveryHints`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Authentication failure (401/403).
    Auth,
    /// Permanent authentication failure (API key invalid/revoked).
    AuthPermanent,
    /// Billing/quota exhaustion.
    Billing,
    /// Rate limiting (429).
    RateLimit,
    /// Provider overloaded (503/529).
    Overloaded,
    /// Internal server error (500/502).
    ServerError,
    /// Timeout (504, connection timeout, missing HTTP status).
    Timeout,
    /// Model not found (404).
    ModelNotFound,
    /// Context window overflow.
    ContextOverflow,
    /// Request payload too large (413).
    PayloadTooLarge,
    /// Request format error (400 — invalid schema, tool format, etc.).
    FormatError,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth => write!(f, "Auth"),
            Self::AuthPermanent => write!(f, "AuthPermanent"),
            Self::Billing => write!(f, "Billing"),
            Self::RateLimit => write!(f, "RateLimit"),
            Self::Overloaded => write!(f, "Overloaded"),
            Self::ServerError => write!(f, "ServerError"),
            Self::Timeout => write!(f, "Timeout"),
            Self::ModelNotFound => write!(f, "ModelNotFound"),
            Self::ContextOverflow => write!(f, "ContextOverflow"),
            Self::PayloadTooLarge => write!(f, "PayloadTooLarge"),
            Self::FormatError => write!(f, "FormatError"),
        }
    }
}

// ── FailoverReason (backward compat) ─────────────────────────────────────

/// Why an API call failed — determines recovery strategy.
///
/// Kept for backward compatibility with [`ErrorCategory`].
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

impl From<ErrorCategory> for FailoverReason {
    fn from(cat: ErrorCategory) -> Self {
        match cat {
            ErrorCategory::Auth => Self::Auth,
            ErrorCategory::AuthPermanent => Self::AuthPermanent,
            ErrorCategory::Billing => Self::Billing,
            ErrorCategory::RateLimit => Self::RateLimit,
            ErrorCategory::Overloaded => Self::Overloaded,
            ErrorCategory::ServerError => Self::ServerError,
            ErrorCategory::Timeout => Self::Timeout,
            ErrorCategory::ContextOverflow => Self::ContextOverflow,
            ErrorCategory::PayloadTooLarge => Self::PayloadTooLarge,
            ErrorCategory::ModelNotFound => Self::ModelNotFound,
            ErrorCategory::FormatError => Self::FormatError,
        }
    }
}

// ── RecoveryHints ────────────────────────────────────────────────────────

/// Recovery hints derived from error category.
#[derive(Debug, Clone)]
pub struct RecoveryHints {
    /// Whether the operation should be retried.
    pub retry: bool,
    /// How long to wait before retrying.
    pub cooldown: Option<Duration>,
    /// Whether to report this error upstream (e.g. to monitoring).
    pub report: bool,
}

// ── ClassifiedError ──────────────────────────────────────────────────────

/// Structured classification of an API error with recovery hints.
#[derive(Debug, Clone)]
pub struct ClassifiedError {
    /// The error category (primary classification).
    pub category: ErrorCategory,
    /// Backward-compatible failover reason.
    pub reason: FailoverReason,
    /// HTTP status code (`None` if unavailable / status 0).
    pub status_code: Option<u16>,
    /// Provider name.
    pub provider: Option<String>,
    /// Model identifier.
    pub model: Option<String>,
    /// Human-readable error message.
    pub message: String,
    /// Retry-after duration extracted from response body.
    pub retry_after: Option<Duration>,
    // ── Backward-compat boolean flags (derived from category) ──
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
    // ── Primary constructor: three-layer classification pipeline ─────────

    /// Classify an error through the three-layer pipeline.
    ///
    /// - **Layer 1** — HTTP status code → broad category.
    /// - **Layer 2** — Provider-specific business codes → fine-grained category.
    /// - **Layer 3** — Fallback: `Timeout` when `status == 0`; otherwise
    ///   `FormatError` for 400 or `RateLimit` for everything else.
    pub fn classify(provider: &str, status: u16, body: &str) -> Self {
        let category = match classify_http(status) {
            Some(cat) => cat,
            None => {
                // Layer 3: no HTTP status → Timeout
                if status == 0 {
                    ErrorCategory::Timeout
                } else {
                    // Layer 2: provider-specific refinement (400 / 429)
                    classify_provider(provider, status, body).unwrap_or(
                        if status == 400 {
                            ErrorCategory::FormatError
                        } else {
                            ErrorCategory::RateLimit
                        },
                    )
                }
            }
        };

        let retry_after = extract_retry_after(body);
        let message = if !body.is_empty() {
            body.to_string()
        } else {
            format!("HTTP {status}")
        };

        Self::from_parts(category, status, provider, message, retry_after)
    }

    // ── Backward-compatible constructors ────────────────────────────────

    /// Quick constructor for the common case (backward compat).
    pub fn new(reason: FailoverReason, message: impl Into<String>) -> Self {
        let category = match reason {
            FailoverReason::Auth => ErrorCategory::Auth,
            FailoverReason::AuthPermanent => ErrorCategory::AuthPermanent,
            FailoverReason::Billing => ErrorCategory::Billing,
            FailoverReason::RateLimit => ErrorCategory::RateLimit,
            FailoverReason::Overloaded => ErrorCategory::Overloaded,
            FailoverReason::ServerError => ErrorCategory::ServerError,
            FailoverReason::Timeout => ErrorCategory::Timeout,
            FailoverReason::ContextOverflow => ErrorCategory::ContextOverflow,
            FailoverReason::PayloadTooLarge => ErrorCategory::PayloadTooLarge,
            FailoverReason::ModelNotFound => ErrorCategory::ModelNotFound,
            FailoverReason::FormatError => ErrorCategory::FormatError,
            FailoverReason::Unknown => ErrorCategory::ServerError,
        };
        Self::from_parts(category, 0, "", message.into(), None)
    }

    /// Classify from an HTTP status code and optional error message.
    pub fn from_http(status: u16, message: Option<&str>) -> Self {
        Self::classify("", status, message.unwrap_or(""))
    }

    /// Classify from a raw error string (no HTTP status → Timeout).
    pub fn from_message(message: &str) -> Self {
        Self::classify("", 0, message)
    }

    /// Set provider/model metadata (builder style).
    pub fn with_provider(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self
    }

    /// Returns true if this is an auth-related error.
    pub fn is_auth(&self) -> bool {
        matches!(
            self.category,
            ErrorCategory::Auth | ErrorCategory::AuthPermanent
        )
    }

    // ── Recovery hints ──────────────────────────────────────────────────

    /// Compute recovery hints from the error category.
    pub fn recovery_hints(&self) -> RecoveryHints {
        recovery_hints_for(&self.category, self.retry_after)
    }

    /// Cooldown duration before retrying this credential.
    pub fn cooldown_duration(&self) -> Option<Duration> {
        self.recovery_hints().cooldown
    }

    /// Whether this error should be reported upstream.
    pub fn should_report(&self) -> bool {
        self.recovery_hints().report
    }

    // ── Internal ────────────────────────────────────────────────────────

    /// Assemble a [`ClassifiedError`] from parts, computing derived fields.
    fn from_parts(
        category: ErrorCategory,
        status: u16,
        provider: &str,
        message: String,
        retry_after: Option<Duration>,
    ) -> Self {
        let hints = recovery_hints_for(&category, retry_after);
        let reason: FailoverReason = category.clone().into();
        let retryable = hints.retry;
        let should_compress = matches!(
            category,
            ErrorCategory::ContextOverflow | ErrorCategory::PayloadTooLarge
        );
        let should_rotate_credential = matches!(
            category,
            ErrorCategory::Auth | ErrorCategory::Billing | ErrorCategory::RateLimit
        );
        let should_fallback = matches!(
            category,
            ErrorCategory::Auth
                | ErrorCategory::AuthPermanent
                | ErrorCategory::Billing
                | ErrorCategory::ModelNotFound
        );
        let cooldown = hints.cooldown;

        Self {
            category,
            reason,
            status_code: if status > 0 { Some(status) } else { None },
            provider: if provider.is_empty() {
                None
            } else {
                Some(provider.to_string())
            },
            model: None,
            message,
            retry_after,
            retryable,
            should_compress,
            should_rotate_credential,
            should_fallback,
            cooldown,
        }
    }
}

// ── Free-standing helpers ────────────────────────────────────────────────

/// Compute recovery hints for a category and optional retry-after.
fn recovery_hints_for(
    category: &ErrorCategory,
    retry_after: Option<Duration>,
) -> RecoveryHints {
    match category {
        ErrorCategory::Auth => RecoveryHints {
            retry: false,
            cooldown: Some(Duration::from_secs(1800)),
            report: false,
        },
        ErrorCategory::AuthPermanent => RecoveryHints {
            retry: false,
            cooldown: Some(Duration::from_secs(86400 * 7)),
            report: true,
        },
        ErrorCategory::Billing => RecoveryHints {
            retry: false,
            cooldown: Some(Duration::from_secs(86400)),
            report: true,
        },
        ErrorCategory::RateLimit => RecoveryHints {
            retry: true,
            cooldown: retry_after.or(Some(Duration::from_secs(3600))),
            report: false,
        },
        ErrorCategory::Overloaded => RecoveryHints {
            retry: true,
            cooldown: retry_after.or(Some(Duration::from_secs(300))),
            report: false,
        },
        ErrorCategory::ServerError => RecoveryHints {
            retry: true,
            cooldown: retry_after.or(Some(Duration::from_secs(300))),
            report: false,
        },
        ErrorCategory::Timeout => RecoveryHints {
            retry: true,
            cooldown: retry_after.or(Some(Duration::from_secs(120))),
            report: false,
        },
        ErrorCategory::ContextOverflow => RecoveryHints {
            retry: false,
            cooldown: None,
            report: true,
        },
        ErrorCategory::PayloadTooLarge => RecoveryHints {
            retry: false,
            cooldown: None,
            report: true,
        },
        ErrorCategory::ModelNotFound => RecoveryHints {
            retry: false,
            cooldown: None,
            report: false,
        },
        ErrorCategory::FormatError => RecoveryHints {
            retry: false,
            cooldown: None,
            report: true,
        },
    }
}

// ── Layer 1: HTTP status code classification ─────────────────────────────

fn classify_http(status: u16) -> Option<ErrorCategory> {
    match status {
        401 | 403 => Some(ErrorCategory::Auth),
        404 => Some(ErrorCategory::ModelNotFound),
        413 => Some(ErrorCategory::PayloadTooLarge),
        500 | 502 => Some(ErrorCategory::ServerError),
        503 | 529 => Some(ErrorCategory::Overloaded),
        504 => Some(ErrorCategory::Timeout),
        _ => None, // Penetrates to Layer 2
    }
}

// ── Layer 2: Provider-specific business code refinement ──────────────────

fn classify_provider(
    provider: &str,
    status: u16,
    body: &str,
) -> Option<ErrorCategory> {
    let lp = provider.to_lowercase();

    match status {
        // ── 429 refinement ──
        429 => {
            // GLM / Zhipu
            if lp.contains("glm") || lp.contains("zhipu") {
                if body_contains_code(body, 1312) {
                    return Some(ErrorCategory::Overloaded);
                }
                if body_contains_code(body, 1308) || body_contains_code(body, 1309) {
                    return Some(ErrorCategory::Billing);
                }
            }
            // OpenAI
            if lp.contains("openai") && body.contains("insufficient_quota") {
                return Some(ErrorCategory::Billing);
            }
            // Other 429 → None (caller falls back to RateLimit)
            None
        }
        // ── 400 refinement ──
        400 => {
            // GLM / Zhipu context overflow
            if (lp.contains("glm") || lp.contains("zhipu")) && body_contains_code(body, 1261)
            {
                return Some(ErrorCategory::ContextOverflow);
            }
            // Generic context overflow
            if body.contains("context_length_exceeded") {
                return Some(ErrorCategory::ContextOverflow);
            }
            // Other 400 → None (caller falls back to FormatError)
            None
        }
        // Other status → no refinement
        _ => None,
    }
}

/// Check whether `body` contains a JSON `"code": <n>` field (with or without
/// spaces around the colon).
fn body_contains_code(body: &str, code: u64) -> bool {
    // Fast path: literal patterns without spaces
    let compact = format!("\"code\":{code}");
    if body.contains(&compact) {
        return true;
    }
    // With spaces
    let spaced = format!("\"code\": {code}");
    body.contains(&spaced)
}

// ── Retry-after extraction ───────────────────────────────────────────────

fn extract_retry_after(body: &str) -> Option<Duration> {
    if body.is_empty() {
        return None;
    }
    // Try JSON parsing
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        // Top-level retry_after / retry-after
        for key in &["retry_after", "retry-after"] {
            if let Some(d) = json_get_seconds(&json, key) {
                return Some(d);
            }
        }
        // Nested error.retry_after
        if let Some(error) = json.get("error") {
            for key in &["retry_after", "retry-after"] {
                if let Some(d) = json_get_seconds(error, key) {
                    return Some(d);
                }
            }
        }
    }
    None
}

fn json_get_seconds(json: &serde_json::Value, key: &str) -> Option<Duration> {
    let val = json.get(key)?;
    let secs = val.as_u64().or_else(|| val.as_f64().map(|f| f as u64))?;
    if secs > 0 {
        Some(Duration::from_secs(secs))
    } else {
        None
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Layer 1 tests ──

    #[test]
    fn layer1_401_is_auth() {
        let err = ClassifiedError::from_http(401, Some("Unauthorized"));
        assert_eq!(err.category, ErrorCategory::Auth);
        assert_eq!(err.reason, FailoverReason::Auth);
        assert!(!err.retryable);
        assert!(!err.should_fallback || err.is_auth());
    }

    #[test]
    fn layer1_503_is_overloaded() {
        let err = ClassifiedError::from_http(503, Some("Service Unavailable"));
        assert_eq!(err.category, ErrorCategory::Overloaded);
        assert!(err.retryable);
    }

    #[test]
    fn layer1_504_is_timeout() {
        let err = ClassifiedError::from_http(504, Some("Gateway Timeout"));
        assert_eq!(err.category, ErrorCategory::Timeout);
        assert!(err.retryable);
    }

    #[test]
    fn layer1_404_is_model_not_found() {
        let err = ClassifiedError::from_http(404, Some("Not Found"));
        assert_eq!(err.category, ErrorCategory::ModelNotFound);
        assert!(!err.retryable);
    }

    #[test]
    fn layer1_413_is_payload_too_large() {
        let err = ClassifiedError::from_http(413, None);
        assert_eq!(err.category, ErrorCategory::PayloadTooLarge);
        assert!(!err.retryable);
        assert!(err.should_compress);
    }

    // ── Layer 2 tests ──

    #[test]
    fn layer2_glm_429_1312_overloaded() {
        let body = r#"{"error":{"code":1312,"message":"该模型当前访问量过大"}}"#;
        let err = ClassifiedError::classify("glm", 429, body);
        assert_eq!(err.category, ErrorCategory::Overloaded);
        assert!(err.retryable);
    }

    #[test]
    fn layer2_glm_429_1308_billing() {
        let body = r#"{"error":{"code":1308,"message":"余额不足"}}"#;
        let err = ClassifiedError::classify("glm", 429, body);
        assert_eq!(err.category, ErrorCategory::Billing);
        assert!(!err.retryable);
    }

    #[test]
    fn layer2_glm_429_1309_billing() {
        let body = r#"{"error":{"code":1309,"message":"额度用尽"}}"#;
        let err = ClassifiedError::classify("zhipu", 429, body);
        assert_eq!(err.category, ErrorCategory::Billing);
    }

    #[test]
    fn layer2_openai_429_insufficient_quota() {
        let body = r#"{"error":{"message":"insufficient_quota","type":"invalid_request_error"}}"#;
        let err = ClassifiedError::classify("openai", 429, body);
        assert_eq!(err.category, ErrorCategory::Billing);
    }

    #[test]
    fn layer2_429_generic_is_rate_limit() {
        let err = ClassifiedError::classify("unknown", 429, "rate limit exceeded");
        assert_eq!(err.category, ErrorCategory::RateLimit);
        assert!(err.retryable);
    }

    #[test]
    fn layer2_glm_400_1261_context_overflow() {
        let body = r#"{"error":{"code":1261,"message":"上下文超长"}}"#;
        let err = ClassifiedError::classify("glm", 400, body);
        assert_eq!(err.category, ErrorCategory::ContextOverflow);
        assert!(!err.retryable);
    }

    #[test]
    fn layer2_400_context_length_exceeded() {
        let body = r#"{"error":{"message":"context_length_exceeded"}}"#;
        let err = ClassifiedError::classify("openai", 400, body);
        assert_eq!(err.category, ErrorCategory::ContextOverflow);
    }

    #[test]
    fn layer2_400_generic_is_format_error() {
        let err = ClassifiedError::classify("unknown", 400, "bad request");
        assert_eq!(err.category, ErrorCategory::FormatError);
        assert!(!err.retryable);
    }

    // ── Layer 3 tests ──

    #[test]
    fn layer3_status_0_is_timeout() {
        let err = ClassifiedError::classify("", 0, "connection refused");
        assert_eq!(err.category, ErrorCategory::Timeout);
        assert!(err.retryable);
    }

    #[test]
    fn from_message_is_timeout() {
        let err = ClassifiedError::from_message("connection timed out");
        assert_eq!(err.category, ErrorCategory::Timeout);
        assert_eq!(err.reason, FailoverReason::Timeout);
        assert!(err.retryable);
    }

    // ── Recovery hints tests ──

    #[test]
    fn auth_cooldown_30min() {
        let err = ClassifiedError::from_http(401, None);
        assert_eq!(err.cooldown_duration(), Some(Duration::from_secs(1800)));
    }

    #[test]
    fn rate_limit_uses_retry_after() {
        let body = r#"{"error":{"retry_after":42}}"#;
        let err = ClassifiedError::classify("openai", 429, body);
        assert_eq!(err.category, ErrorCategory::RateLimit);
        assert_eq!(err.retry_after, Some(Duration::from_secs(42)));
        assert_eq!(err.cooldown_duration(), Some(Duration::from_secs(42)));
    }

    #[test]
    fn rate_limit_default_cooldown_1h() {
        let err = ClassifiedError::from_http(429, Some("too many requests"));
        assert_eq!(err.cooldown_duration(), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn billing_report_true() {
        let err = ClassifiedError::classify("openai", 429, r#"{"error":{"message":"insufficient_quota"}}"#);
        assert!(err.should_report());
    }

    #[test]
    fn format_error_report_true() {
        let err = ClassifiedError::classify("", 400, "bad request");
        assert!(err.should_report());
    }

    #[test]
    fn overloaded_report_false() {
        let err = ClassifiedError::from_http(503, None);
        assert!(!err.should_report());
    }

    // ── Backward compat tests ──

    #[test]
    fn new_constructs_from_failover_reason() {
        let err = ClassifiedError::new(FailoverReason::AuthPermanent, "revoked");
        assert_eq!(err.category, ErrorCategory::AuthPermanent);
        assert!(!err.retryable);
        assert!(err.should_report());
        // AuthPermanent is a provider error — warrants failover to another provider.
        assert!(err.should_fallback);
    }

    #[test]
    fn is_auth_works() {
        let auth = ClassifiedError::from_http(401, None);
        assert!(auth.is_auth());
        let not_auth = ClassifiedError::from_http(429, None);
        assert!(!not_auth.is_auth());
    }

    #[test]
    fn with_provider_sets_metadata() {
        let err = ClassifiedError::from_http(500, Some("oops"))
            .with_provider("openai", "gpt-4");
        assert_eq!(err.provider.as_deref(), Some("openai"));
        assert_eq!(err.model.as_deref(), Some("gpt-4"));
    }

    // ── body_contains_code helper ──

    #[test]
    fn body_code_compact_and_spaced() {
        assert!(body_contains_code(r#"{"code":1312}"#, 1312));
        assert!(body_contains_code(r#"{"code": 1312}"#, 1312));
        assert!(!body_contains_code(r#"{"code":9999}"#, 1312));
    }

    // ── extract_retry_after tests ──

    #[test]
    fn retry_after_from_json() {
        let body = r#"{"error":{"retry_after":120}}"#;
        assert_eq!(extract_retry_after(body), Some(Duration::from_secs(120)));
    }

    #[test]
    fn retry_after_from_top_level() {
        let body = r#"{"retry_after":60}"#;
        assert_eq!(extract_retry_after(body), Some(Duration::from_secs(60)));
    }

    #[test]
    fn retry_after_empty_body() {
        assert_eq!(extract_retry_after(""), None);
    }

    #[test]
    fn retry_after_no_field() {
        assert_eq!(extract_retry_after(r#"{"error":"oops"}"#), None);
    }
}
