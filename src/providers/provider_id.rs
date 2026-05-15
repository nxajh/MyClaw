//! Provider identity — stable string-based identifier for provider vendors.
//!
//! `ProviderId` is a newtype around `String` (not a closed enum) so that
//! third-party providers can be registered without modifying core code.

use std::fmt;

/// Stable identifier for a provider vendor (e.g. "openai", "glm").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── Well-known providers ─────────────────────────────────────────────────────

pub mod well_known {
    pub const GENERIC: &str = "generic";
    pub const OPENAI: &str = "openai";
    pub const ANTHROPIC: &str = "anthropic";
    pub const GLM: &str = "glm";
    pub const XIAOMI: &str = "xiaomi";
    pub const KIMI: &str = "kimi";
    pub const MINIMAX: &str = "minimax";
    pub const GOOGLE: &str = "google";
}

// ── URL host detection ───────────────────────────────────────────────────────

/// Infer provider identity from a base_url string.
///
/// Returns `None` if the host cannot be parsed or no known provider matches.
pub fn detect_from_url(base_url: &str) -> Option<ProviderId> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");

    if host.contains("bigmodel.cn") || host.contains("zhipuai") {
        Some(ProviderId::new(well_known::GLM))
    } else if host.contains("xiaomimimo") {
        Some(ProviderId::new(well_known::XIAOMI))
    } else if host.contains("anthropic.com") || host.contains("claude.ai") {
        Some(ProviderId::new(well_known::ANTHROPIC))
    } else if host.contains("minimax") || host.contains("minimaxi") {
        Some(ProviderId::new(well_known::MINIMAX))
    } else if host.contains("moonshot") || host.contains("kimi") {
        Some(ProviderId::new(well_known::KIMI))
    } else if host.contains("googleapis.com") || host.contains("google.com") {
        Some(ProviderId::new(well_known::GOOGLE))
    } else if host.contains("openai.com") || host.contains("deepseek") || host.contains("siliconflow") {
        Some(ProviderId::new(well_known::OPENAI))
    } else {
        None
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_glm() {
        assert_eq!(
            detect_from_url("https://open.bigmodel.cn/api/paas/v4"),
            Some(ProviderId::new("glm"))
        );
    }

    #[test]
    fn detect_xiaomi() {
        assert_eq!(
            detect_from_url("https://api.xiaomimimo.com/anthropic/v1"),
            Some(ProviderId::new("xiaomi"))
        );
    }

    #[test]
    fn detect_openai() {
        assert_eq!(
            detect_from_url("https://api.openai.com/v1"),
            Some(ProviderId::new("openai"))
        );
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(detect_from_url("https://proxy.example.com/v1"), None);
    }
}