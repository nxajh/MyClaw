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

// ── Capability Sections ───────────────────────────────────────────────────────

/// Chat capability section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSection {
    pub base_url: String,
    pub user_agent: Option<String>,
    pub api_key: Option<String>,
    pub auth_style: Option<AuthStyle>,
    pub models: HashMap<String, ChatModelConfig>,
}

/// Embedding capability section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingSection {
    pub base_url: String,
    pub user_agent: Option<String>,
    pub api_key: Option<String>,
    pub auth_style: Option<AuthStyle>,
    pub models: HashMap<String, EmbeddingModelConfig>,
}

/// Generic capability section (image generation, TTS, STT, video, search).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySection {
    pub base_url: String,
    pub user_agent: Option<String>,
    pub api_key: Option<String>,
    pub auth_style: Option<AuthStyle>,
    pub models: HashMap<String, BasicModelConfig>,
}

// ── ProviderConfig ────────────────────────────────────────────────────────────

/// Provider connection configuration.
///
/// Each capability has its own section with base_url and models.
/// Provider-level api_key and auth_style serve as fallback defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider-level API key (supports `${ENV_VAR}` expansion).
    /// Capability-level api_key takes precedence when set.
    pub api_key: Option<String>,
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
        assert_eq!(chat.models.len(), 2);
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
