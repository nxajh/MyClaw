//! Provider configuration — connection info, auth, and model declarations.
//!
//! Each provider declares its own capabilities as separate sections.
//! Models are grouped by capability under their respective sections.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// Re-export new types from the canonical source.
pub use crate::providers::capability::{
    Capability, Modality,
    ChatModelConfig, EmbeddingModelConfig, BasicModelConfig,
    ChatPricing, EmbeddingPricing, BasicPricing,
};
pub use crate::providers::RotationStrategy;

// ── Protocol ──────────────────────────────────────────────────────────────────

/// API protocol used by a provider capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OpenAI Chat Completions protocol (or OpenAI-compatible).
    #[default]
    OpenAi,
    /// Anthropic Messages protocol (or Anthropic-compatible).
    Anthropic,
}

// ── AuthStyle ─────────────────────────────────────────────────────────────────

/// How to authenticate with a provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    /// Bearer token in Authorization header.
    #[default]
    Bearer,
    /// x-api-key header (Anthropic style).
    XApiKey,
}

impl From<AuthStyle> for crate::providers::AuthStyle {
    fn from(style: AuthStyle) -> Self {
        match style {
            AuthStyle::Bearer => crate::providers::AuthStyle::Bearer,
            AuthStyle::XApiKey => crate::providers::AuthStyle::XApiKey,
        }
    }
}

// ── Capability Sections ───────────────────────────────────────────────────────

/// Chat capability section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSection {
    pub base_url: String,
    pub user_agent: Option<String>,
    pub api_key: Option<String>,
    pub auth_style: Option<AuthStyle>,
    /// Explicit protocol override. If not set, inferred from base_url host.
    pub protocol: Option<Protocol>,
    pub models: HashMap<String, ChatModelConfig>,
}

/// Embedding capability section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingSection {
    pub base_url: String,
    pub user_agent: Option<String>,
    pub api_key: Option<String>,
    pub auth_style: Option<AuthStyle>,
    /// Explicit protocol override. If not set, inferred from base_url host.
    pub protocol: Option<Protocol>,
    pub models: HashMap<String, EmbeddingModelConfig>,
}

/// Generic capability section (image generation, TTS, STT, video, search).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySection {
    pub base_url: String,
    pub user_agent: Option<String>,
    pub api_key: Option<String>,
    pub auth_style: Option<AuthStyle>,
    /// Explicit protocol override. If not set, inferred from base_url host.
    pub protocol: Option<Protocol>,
    pub models: HashMap<String, BasicModelConfig>,
}

// ── ProviderConfig ────────────────────────────────────────────────────────────

/// Provider connection configuration.
///
/// Each capability has its own section with base_url and models.
/// Provider-level api_key and auth_style serve as fallback defaults.
///
/// Multiple API keys can be configured for rotation (rate-limit / billing failover).
/// `api_keys` takes precedence over `api_key`; if neither is set, the provider
/// falls back to environment-variable resolution at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider identity override (e.g. "glm", "anthropic").
    /// If not set, inferred from the capability base_url host.
    #[serde(default)]
    pub provider: Option<String>,
    /// Provider-level API key (supports `${ENV_VAR}` expansion).
    /// Capability-level api_key takes precedence when set.
    pub api_key: Option<String>,
    /// Multiple API keys for rotation (comma-separated or array).
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Credential rotation strategy when multiple keys are available.
    #[serde(default)]
    pub rotation_strategy: RotationStrategy,
    /// Default authentication style.
    #[serde(default)]
    pub auth_style: AuthStyle,

    /// Chat capability section.
    #[serde(default)]
    pub chat: Option<ChatSection>,
    /// Embedding capability section.
    #[serde(default)]
    pub embedding: Option<EmbeddingSection>,
    /// Image generation capability section.
    #[serde(default)]
    pub image_generation: Option<CapabilitySection>,
    /// Text-to-speech capability section.
    #[serde(default)]
    pub tts: Option<CapabilitySection>,
    /// Speech-to-text capability section.
    #[serde(default)]
    pub stt: Option<CapabilitySection>,
    /// Video generation capability section.
    #[serde(default)]
    pub video: Option<CapabilitySection>,
    /// Search capability section.
    #[serde(default)]
    pub search: Option<CapabilitySection>,
}

impl ProviderConfig {
    /// Get the effective api_key for a capability section.
    /// Capability-level key takes precedence; falls back to provider-level.
    pub fn effective_api_key(&self, capability_api_key: Option<&str>) -> Option<String> {
        capability_api_key
            .filter(|k| !k.is_empty())
            .map(String::from)
            .or_else(|| self.api_key.clone())
    }

    /// Get all effective API keys for a capability section.
    /// Returns `api_keys` if populated, otherwise falls back to `api_key` as a single-element vec.
    pub fn effective_api_keys(&self, capability_api_key: Option<&str>) -> Vec<String> {
        // Capability-level api_keys (not yet supported per-capability, use provider-level)
        let provider_keys: Vec<String> = self.api_keys.iter()
            .filter(|k| !k.is_empty())
            .cloned()
            .collect();

        if !provider_keys.is_empty() {
            return provider_keys;
        }

        // Fallback: capability-level single key, then provider-level single key
        if let Some(key) = capability_api_key.filter(|k| !k.is_empty()) {
            return vec![key.to_string()];
        }
        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                return vec![key.clone()];
            }
        }
        Vec::new()
    }

    /// Get the effective auth_style for a capability section.
    /// Capability-level auth_style takes precedence; falls back to provider-level.
    pub fn effective_auth_style(&self, capability_auth: Option<AuthStyle>) -> AuthStyle {
        capability_auth.unwrap_or(self.auth_style)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_new_provider_config() {
        let toml_str = r#"
api_key = "${OPENAI_API_KEY}"

[chat]
base_url = "https://api.openai.com/v1"
user_agent = "MyClaw/0.1"

[chat.models.gpt-4o]
input = ["text", "image"]
output = ["text"]
context_window = 128000
max_output_tokens = 16384
reasoning = false

[chat.models.gpt-4o.pricing]
input = 2.5
output = 10.0
cache_read = 1.25

[embedding]
base_url = "https://api.openai.com/v1"

[embedding.models.text-embedding-3-small]
dimensions = 1536
"#;
        let config: ProviderConfig = toml::from_str(toml_str).unwrap();
        assert!(config.chat.is_some());
        let chat = config.chat.as_ref().unwrap();
        assert_eq!(chat.base_url, "https://api.openai.com/v1");
        assert_eq!(chat.models.len(), 1);
        let gpt4o = &chat.models["gpt-4o"];
        assert!(gpt4o.supports_image_input());
        assert_eq!(gpt4o.context_window, Some(128000));
        let pricing = gpt4o.pricing.as_ref().unwrap();
        assert_eq!(pricing.input, Some(2.5));
        assert!(config.embedding.is_some());
    }

    #[test]
    fn effective_api_key_fallback() {
        let toml_str = r#"
api_key = "provider-key"

[chat]
base_url = "https://api.openai.com/v1"

[chat.models.gpt-4o]
input = ["text"]
output = ["text"]
"#;
        let config: ProviderConfig = toml::from_str(toml_str).unwrap();
        let chat = config.chat.as_ref().unwrap();
        assert_eq!(
            config.effective_api_key(chat.api_key.as_deref()),
            Some("provider-key".to_string())
        );
    }

    #[test]
    fn effective_api_key_capability_override() {
        let toml_str = r#"
api_key = "provider-key"

[chat]
base_url = "https://api.openai.com/v1"
api_key = "chat-specific-key"

[chat.models.gpt-4o]
input = ["text"]
output = ["text"]
"#;
        let config: ProviderConfig = toml::from_str(toml_str).unwrap();
        let chat = config.chat.as_ref().unwrap();
        assert_eq!(
            config.effective_api_key(chat.api_key.as_deref()),
            Some("chat-specific-key".to_string())
        );
    }
}
