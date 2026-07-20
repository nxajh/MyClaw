//! ProviderFactory — central builder for provider trait objects.
//!
//! Replaces the scattered `ProviderHandle::from_url_with_user_agent` calls
//! in `daemon.rs` with a single entry point that respects explicit
//! `provider` / `protocol` config overrides.

use crate::config::provider::Protocol;
use crate::providers::capability_chat::ChatProvider;
use crate::providers::capability_embedding::EmbeddingProvider;
use crate::providers::image::ImageGenerationProvider;
use crate::providers::protocols::openai::chat_completions::OpenAiChatCompletionsClient;
use crate::providers::protocols::openai::responses::OpenAiResponsesClient;
use crate::providers::provider_id::well_known;
use crate::providers::search::SearchProvider;
use crate::providers::stt::SttProvider;
use crate::providers::tts::TtsProvider;
use crate::providers::video::VideoGenerationProvider;
use crate::providers::{AuthStyle, ProviderId, SharedApiKey};

// ── Build requests ────────────────────────────────────────────────────────────

/// Request to build a chat provider.
#[derive(Debug, Clone)]
pub struct BuildChatProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub protocol: Option<Protocol>,
    pub base_url: String,
    pub api_key: SharedApiKey,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}

/// Request to build an embedding provider.
#[derive(Debug, Clone)]
pub struct BuildEmbeddingProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}

/// Request to build an image generation provider.
#[derive(Debug, Clone)]
pub struct BuildImageProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}

/// Request to build a TTS provider.
#[derive(Debug, Clone)]
pub struct BuildTtsProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}

/// Request to build a search provider.
#[derive(Debug, Clone)]
pub struct BuildSearchProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub base_url: String,
    pub api_key: SharedApiKey,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}

/// Request to build a video provider.
#[derive(Debug, Clone)]
pub struct BuildVideoProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}

/// Request to build an STT provider.
#[derive(Debug, Clone)]
pub struct BuildSttProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// Central factory for constructing provider trait objects.
///
/// Uses `(provider_id, protocol)` to dispatch to the correct protocol client.
pub struct ProviderFactory;

impl Default for ProviderFactory {
    fn default() -> Self {
        Self
    }
}

impl ProviderFactory {
    pub fn new() -> Self {
        Self
    }

    /// Resolve effective protocol: explicit config > provider-specific default > OpenAi.
    fn resolve_protocol(provider_id: &ProviderId, configured: Option<Protocol>) -> Protocol {
        if let Some(p) = configured {
            return p;
        }
        match provider_id.as_str() {
            well_known::ANTHROPIC => Protocol::Messages,
            well_known::XIAOMI => Protocol::Messages,
            well_known::MINIMAX => Protocol::Messages,
            well_known::GOOGLE => Protocol::GenerateContent,
            _ => Protocol::ChatCompletions,
        }
    }

    /// Build a boxed `ChatProvider`.
    pub fn build_chat_provider(
        &self,
        request: BuildChatProviderRequest,
    ) -> anyhow::Result<Box<dyn ChatProvider>> {
        let protocol = Self::resolve_protocol(&request.provider_id, request.protocol);
        let id = request.provider_id.as_str();

        tracing::debug!(
            provider = %request.provider_key,
            id = %id,
            protocol = ?protocol,
            base_url = %request.base_url,
            "ProviderFactory: building chat provider"
        );

        match (id, protocol) {
            // ── GLM: OpenAI client + GLM body override (preserved thinking) ──
            (well_known::GLM, _) => {
                let client = OpenAiChatCompletionsClient::new(request.api_key, request.base_url)
                    .with_body_override(crate::providers::glm::glm_body_override);
                let client = match request.user_agent {
                    Some(ua) => client.with_user_agent(ua),
                    None => client,
                };
                Ok(Box::new(client))
            }
            // ── DeepSeek: OpenAI client + DeepSeek body override (interleaved thinking) ──
            (well_known::DEEPSEEK, _) => {
                let client = OpenAiChatCompletionsClient::new(request.api_key, request.base_url)
                    .with_body_override(crate::providers::deepseek::deepseek_body_override);
                let client = match request.user_agent {
                    Some(ua) => client.with_user_agent(ua),
                    None => client,
                };
                Ok(Box::new(client))
            }
            // ── Xiaomi: config-driven protocol (Anthropic or OpenAI) ──
            (well_known::XIAOMI, _) => {
                let mut p = crate::providers::xiaomi::XiaomiProvider::with_base_url(
                    request.api_key,
                    request.base_url,
                );
                if protocol == Protocol::ChatCompletions {
                    p = p.with_openai();
                }
                if let Some(ua) = request.user_agent {
                    p = p.with_user_agent(ua);
                }
                Ok(Box::new(p))
            }
            // ── OpenAI Responses API (grok-4.5 via CPA / api.x.ai) ────────────
            (_, Protocol::Responses) => {
                let client = OpenAiResponsesClient::new(request.api_key, request.base_url);
                let client = match request.user_agent {
                    Some(ua) => client.with_user_agent(ua),
                    None => client,
                };
                Ok(Box::new(client))
            }
            // ── OpenAI-compatible providers ──
            (_, Protocol::ChatCompletions) if id != well_known::GOOGLE && id != well_known::DEEPSEEK => {
                let client = OpenAiChatCompletionsClient::new(request.api_key, request.base_url);
                let client = match request.user_agent {
                    Some(ua) => client.with_user_agent(ua),
                    None => client,
                };
                Ok(Box::new(client))
            }
            // ── Anthropic-protocol providers ──
            (_, Protocol::Messages) => {
                let client =
                    crate::providers::protocols::anthropic::messages::AnthropicMessagesClient::new(
                        request.api_key,
                        request.base_url,
                    );
                let client = match request.user_agent {
                    Some(ua) => client.with_user_agent(ua),
                    None => client,
                };
                Ok(Box::new(client))
            }
            // ── Google Generate Content API ──────────────────────────────
            (well_known::GOOGLE, _) => {
                let client =
                    crate::providers::protocols::google::generate_content::GoogleGenerateContentClient::new(
                        request.api_key,
                        request.base_url,
                    );
                Ok(Box::new(client))
            }
            // ── Unreachable guard ──
            _ => anyhow::bail!(
                "no chat protocol client for provider '{}' (id='{}')",
                request.provider_key,
                id,
            ),
        }
    }

    /// Build a boxed `EmbeddingProvider`, if supported.
    pub fn build_embedding_provider(
        &self,
        request: BuildEmbeddingProviderRequest,
    ) -> Option<Box<dyn EmbeddingProvider>> {
        let id = request.provider_id.as_str();
        match id {
            well_known::GLM => {
                let mut p = crate::providers::glm::GlmProvider::with_base_url(
                    request.api_key,
                    request.base_url,
                );
                if let Some(ua) = request.user_agent {
                    p = p.with_user_agent(ua);
                }
                Some(Box::new(p))
            }
            well_known::OPENAI | well_known::KIMI | well_known::GENERIC => {
                let mut p = crate::providers::openai::OpenAiProvider::with_base_url(
                    request.api_key,
                    request.base_url,
                );
                if let Some(ua) = request.user_agent {
                    p = p.with_user_agent(ua);
                }
                Some(Box::new(p))
            }
            _ => None,
        }
    }

    /// Build a boxed `ImageGenerationProvider`, if supported.
    pub fn build_image_provider(
        &self,
        request: BuildImageProviderRequest,
    ) -> Option<Box<dyn ImageGenerationProvider>> {
        let id = request.provider_id.as_str();
        match id {
            // Only OpenAI-compatible providers support image generation.
            well_known::OPENAI | well_known::GENERIC | well_known::KIMI => {
                let mut p = crate::providers::openai::OpenAiProvider::with_base_url(
                    request.api_key,
                    request.base_url,
                );
                if let Some(ua) = request.user_agent {
                    p = p.with_user_agent(ua);
                }
                Some(Box::new(p))
            }
            _ => None,
        }
    }

    /// Build a boxed `TtsProvider`, if supported.
    pub fn build_tts_provider(
        &self,
        request: BuildTtsProviderRequest,
    ) -> Option<Box<dyn TtsProvider>> {
        let id = request.provider_id.as_str();
        match id {
            // Only OpenAI-compatible providers support TTS.
            well_known::OPENAI | well_known::GENERIC | well_known::KIMI => {
                let mut p = crate::providers::openai::OpenAiProvider::with_base_url(
                    request.api_key,
                    request.base_url,
                );
                if let Some(ua) = request.user_agent {
                    p = p.with_user_agent(ua);
                }
                Some(Box::new(p))
            }
            _ => None,
        }
    }

    /// Build a boxed `SearchProvider`, if supported.
    pub fn build_search_provider(
        &self,
        request: BuildSearchProviderRequest,
    ) -> Option<Box<dyn SearchProvider>> {
        let id = request.provider_id.as_str();
        match id {
            well_known::GLM => {
                if request.base_url.contains("mcp") {
                    let mut p = crate::providers::glm_mcp::GlmMcpSearchProvider::new(
                        request.api_key,
                        request.base_url,
                    );
                    if let Some(ua) = request.user_agent {
                        p = p.with_user_agent(ua);
                    }
                    Some(Box::new(p))
                } else {
                    let mut p = crate::providers::glm::GlmProvider::with_base_url(
                        request.api_key,
                        request.base_url,
                    );
                    if let Some(ua) = request.user_agent {
                        p = p.with_user_agent(ua);
                    }
                    Some(Box::new(p))
                }
            }
            well_known::GOOGLE => {
                let p = crate::providers::google::GoogleProvider::with_base_url(
                    request.api_key,
                    request.base_url,
                );
                Some(Box::new(p))
            }
            well_known::MINIMAX => {
                let mut p = crate::providers::minimax::MiniMaxProvider::with_base_url(
                    request.api_key,
                    request.base_url,
                );
                if let Some(ua) = request.user_agent {
                    p = p.with_user_agent(ua);
                }
                Some(Box::new(p))
            }
            _ => None,
        }
    }

    /// Build a boxed `VideoGenerationProvider`, if supported.
    pub fn build_video_provider(
        &self,
        _request: BuildVideoProviderRequest,
    ) -> Option<Box<dyn VideoGenerationProvider>> {
        None
    }

    /// Build a boxed `SttProvider`, if supported.
    pub fn build_stt_provider(
        &self,
        _request: BuildSttProviderRequest,
    ) -> Option<Box<dyn SttProvider>> {
        None
    }
}
