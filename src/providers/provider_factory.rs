//! ProviderFactory — central builder for provider trait objects.
//!
//! Replaces the scattered `ProviderHandle::from_url_with_user_agent` calls
//! in `daemon.rs` with a single entry point that respects explicit
//! `provider` / `protocol` config overrides.

use std::collections::HashMap;

use crate::config::provider::Protocol;
use crate::providers::{AuthStyle, ProviderId, ProviderHandle};
use crate::providers::capability_chat::ChatProvider;
use crate::providers::capability_embedding::EmbeddingProvider;
use crate::providers::image::ImageGenerationProvider;
use crate::providers::search::SearchProvider;
use crate::providers::tts::TtsProvider;
use crate::providers::stt::SttProvider;
use crate::providers::video::VideoGenerationProvider;
use crate::providers::provider_id::well_known;

// ── Build requests ────────────────────────────────────────────────────────────

/// Request to build a chat provider.
#[derive(Debug, Clone)]
pub struct BuildChatProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub protocol: Option<Protocol>,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
    pub extra_headers: HashMap<String, String>,
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
    pub extra_headers: HashMap<String, String>,
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
    pub extra_headers: HashMap<String, String>,
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
    pub extra_headers: HashMap<String, String>,
}

/// Request to build a search provider.
#[derive(Debug, Clone)]
pub struct BuildSearchProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
    pub extra_headers: HashMap<String, String>,
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
    pub extra_headers: HashMap<String, String>,
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
    pub extra_headers: HashMap<String, String>,
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// Central factory for constructing provider trait objects.
///
/// Uses `(provider_id, protocol)` to dispatch to the correct protocol client.
/// Falls back to `ProviderHandle` for capabilities not yet migrated.
pub struct ProviderFactory;

impl ProviderFactory {
    pub fn new() -> Self {
        Self
    }

    /// Resolve effective protocol: explicit config > provider-specific default > OpenAi.
    fn resolve_protocol(provider_id: &ProviderId, configured: Option<Protocol>) -> Protocol {
        if let Some(p) = configured {
            return p;
        }
        // Provider-specific defaults based on vendor identity.
        match provider_id.as_str() {
            well_known::ANTHROPIC => Protocol::Anthropic,
            well_known::XIAOMI => Protocol::Anthropic,
            well_known::MINIMAX => Protocol::Anthropic,
            // All others default to OpenAI-compatible.
            _ => Protocol::OpenAi,
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
            // ── OpenAI-compatible providers ──
            (well_known::OPENAI | well_known::KIMI | well_known::GENERIC, Protocol::OpenAi) => {
                let client = crate::providers::protocols::openai::chat_completions::OpenAiChatCompletionsClient::new(
                    request.api_key, request.base_url,
                );
                let client = match request.user_agent {
                    Some(ua) => client.with_user_agent(ua),
                    None => client,
                };
                Ok(Box::new(client))
            }
            // ── Anthropic-compatible providers ──
            (well_known::ANTHROPIC | well_known::XIAOMI | well_known::MINIMAX | well_known::GENERIC, Protocol::Anthropic) => {
                let client = crate::providers::protocols::anthropic::messages::AnthropicMessagesClient::new(
                    request.api_key, request.base_url,
                );
                let client = match request.user_agent {
                    Some(ua) => client.with_user_agent(ua),
                    None => client,
                };
                Ok(Box::new(client))
            }
            // ── Fallback: delegate to old ProviderHandle for unmatched combinations ──
            _ => {
                tracing::warn!(
                    provider = %request.provider_key,
                    id = %id,
                    protocol = ?protocol,
                    "no dedicated protocol client for this combination, falling back to ProviderHandle"
                );
                let handle = ProviderHandle::from_url_with_user_agent(
                    request.api_key,
                    &request.base_url,
                    request.user_agent.as_deref(),
                ).ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot determine provider type from base_url '{}' (key='{}')",
                        request.base_url,
                        request.provider_key,
                    )
                })?;
                Ok(handle.into_chat_provider())
            }
        }
    }

    /// Build a boxed `EmbeddingProvider`, if supported.
    pub fn build_embedding_provider(
        &self,
        request: BuildEmbeddingProviderRequest,
    ) -> Option<Box<dyn EmbeddingProvider>> {
        ProviderHandle::from_url_with_user_agent(
            request.api_key,
            &request.base_url,
            request.user_agent.as_deref(),
        ).and_then(|h| h.into_embedding_provider())
    }

    /// Build a boxed `ImageGenerationProvider`, if supported.
    pub fn build_image_provider(
        &self,
        request: BuildImageProviderRequest,
    ) -> Option<Box<dyn ImageGenerationProvider>> {
        ProviderHandle::from_url_with_user_agent(
            request.api_key,
            &request.base_url,
            request.user_agent.as_deref(),
        ).and_then(|h| h.into_image_provider())
    }

    /// Build a boxed `TtsProvider`, if supported.
    pub fn build_tts_provider(
        &self,
        request: BuildTtsProviderRequest,
    ) -> Option<Box<dyn TtsProvider>> {
        ProviderHandle::from_url_with_user_agent(
            request.api_key,
            &request.base_url,
            request.user_agent.as_deref(),
        ).and_then(|h| h.into_tts_provider())
    }

    /// Build a boxed `SearchProvider`, if supported.
    pub fn build_search_provider(
        &self,
        request: BuildSearchProviderRequest,
    ) -> Option<Box<dyn SearchProvider>> {
        ProviderHandle::from_url_with_user_agent(
            request.api_key,
            &request.base_url,
            request.user_agent.as_deref(),
        ).and_then(|h| h.into_search_provider())
    }

    /// Build a boxed `VideoGenerationProvider`, if supported.
    pub fn build_video_provider(
        &self,
        request: BuildVideoProviderRequest,
    ) -> Option<Box<dyn VideoGenerationProvider>> {
        ProviderHandle::from_url_with_user_agent(
            request.api_key,
            &request.base_url,
            request.user_agent.as_deref(),
        ).and_then(|h| h.into_video_provider())
    }

    /// Build a boxed `SttProvider`, if supported.
    pub fn build_stt_provider(
        &self,
        request: BuildSttProviderRequest,
    ) -> Option<Box<dyn SttProvider>> {
        ProviderHandle::from_url_with_user_agent(
            request.api_key,
            &request.base_url,
            request.user_agent.as_deref(),
        ).and_then(|h| h.into_stt_provider())
    }
}