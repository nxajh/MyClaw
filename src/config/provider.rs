//! Provider configuration — connection info, auth, and model declarations.
//!
//! Each provider declares its own models with self-contained capabilities.
//! No global model registry — clarity over DRY.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// Re-export Capability and ChatFeatures from the canonical source.
pub use crate::providers::capability::{Capability, ChatFeatures};

// ── AuthStyle ─────────────────────────────────────────────────────────────────

/// How to authenticate with a provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    /// Bearer token in Authorization header.
    #[default]
    Bearer,
    /// Azure OpenAI: api-key header.
    Azure,
    /// Zhipu (GLM): JWT from API key split.
    ZhipuJwt,
    /// x-api-key header (Anthropic style).
    XApiKey,
}

// ── Pricing ───────────────────────────────────────────────────────────────────

/// Per-model pricing info (USD per million tokens).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_million: Option<f64>,
    pub output_per_million: Option<f64>,
    pub cached_input_per_million: Option<f64>,
    pub per_image_standard: Option<f64>,
    pub per_image_hd: Option<f64>,
}

// ── OpenAiChatConfig ──────────────────────────────────────────────────────────

/// Provider-level chat protocol tweaks (applies to all models under this provider).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAiChatConfig {
    #[serde(default)]
    pub merge_system_into_user: bool,
    #[serde(default)]
    pub responses_fallback: bool,
    pub user_agent: Option<String>,
    pub reasoning_effort: Option<String>,
}

// ── ModelConfig ───────────────────────────────────────────────────────────────

/// A model declaration under a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Capabilities this model supports.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Detailed chat features (inferred from capabilities if absent).
    #[serde(default)]
    pub chat_features: Option<ChatFeatures>,
    /// Maximum output tokens.
    pub max_output_tokens: Option<u32>,
    /// Context window size in tokens.
    pub context_window: Option<u64>,
    /// Pricing information.
    #[serde(default)]
    pub pricing: Option<Pricing>,
    /// Whether this model supports reasoning/thinking.
    #[serde(default)]
    pub reasoning: bool,
}

impl ModelConfig {
    /// Check if the model supports a given capability.
    pub fn supports(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }
}

// ── ProviderConfig ────────────────────────────────────────────────────────────

/// Provider connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// API base URL.
    pub base_url: String,
    /// API key (supports `${ENV_VAR}` expansion).
    pub api_key: Option<String>,
    /// Authentication style.
    #[serde(default)]
    pub auth_style: AuthStyle,
    /// Request timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Extra HTTP headers.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    /// Provider-level chat config tweaks.
    #[serde(default)]
    pub chat: OpenAiChatConfig,
    /// Models declared under this provider.
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_provider() {
        let toml_str = r#"
base_url = "https://api.openai.com/v1"
api_key = "sk-test"
"#;
        let config: ProviderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
        assert!(config.models.is_empty());
    }

    #[test]
    fn deserialize_provider_with_models() {
        let toml_str = r#"
base_url = "https://api.minimaxi.com/v1"
api_key="${MI*[REDACTED]"
auth_style = "bearer"

[models.minimax-m2-dot-7]
capabilities = ["chat", "vision"]
max_output_tokens = 32768

[models.minimax-m2-dot-7.pricing]
input_per_million = 1.0
output_per_million = 2.0

[models.minimax-tts]
capabilities = ["text_to_speech"]
"#;
        let config: ProviderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.models.len(), 2, "expected 2 models, got: {:?}", config.models.keys().collect::<Vec<_>>());

        let m27 = &config.models["minimax-m2-dot-7"];
        assert!(m27.supports(Capability::Chat));
        assert!(m27.supports(Capability::Vision));
        assert!(!m27.supports(Capability::Embedding));
        assert_eq!(m27.max_output_tokens, Some(32768));
        assert!(m27.pricing.is_some());

        let tts = &config.models["minimax-tts"];
        assert!(tts.supports(Capability::TextToSpeech));
    }
}
