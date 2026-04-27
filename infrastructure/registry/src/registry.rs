//! ServiceRegistry — capability routing center.

use std::sync::Arc;
use anyhow::Context;
use capability::capability::Capability;
use capability::chat::ChatProvider;
use capability::embedding::EmbeddingProvider;
use capability::image::ImageGenerationProvider;
use capability::service_registry::ServiceRegistry;
use capability::tts::TtsProvider;
use capability::video::VideoGenerationProvider;

use crate::routing::{RoutingConfig, RouteEntry, RoutingStrategy};

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub name: String,
    pub api: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub model_id: String,
    pub capabilities: Vec<Capability>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u32>,
    pub reasoning: bool,
}

impl ModelConfig {
    pub fn supports(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

pub struct Registry {
    providers: std::collections::HashMap<String, ProviderConfig>,
    routing: RoutingConfig,
    chat_providers: std::collections::HashMap<String, Arc<dyn ChatProvider>>,
}

impl Registry {
    pub fn new(providers: std::collections::HashMap<String, ProviderConfig>, routing: RoutingConfig) -> Self {
        Self { providers, routing, chat_providers: std::collections::HashMap::new() }
    }

    pub fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String) {
        self.chat_providers.insert(model_id, provider.into());
    }

    fn find_provider_by_model(&self, model_id: &str) -> anyhow::Result<(&ProviderConfig, &ModelConfig)> {
        for provider in self.providers.values() {
            for model in &provider.models {
                if model.model_id == model_id {
                    return Ok((provider, model));
                }
            }
        }
        anyhow::bail!("No provider found for model: {}", model_id)
    }

    fn select_model(&self, entry: &RouteEntry, capability: Capability) -> anyhow::Result<&ModelConfig> {
        match entry.strategy {
            RoutingStrategy::Fixed => {
                let model_id = entry.models.first()
                    .with_context(|| "No models in route entry")?;
                let (_, model) = self.find_provider_by_model(model_id)?;
                if !model.supports(capability) {
                    anyhow::bail!("Model {} does not support {:?}", model_id, capability);
                }
                Ok(model)
            }
            RoutingStrategy::Fallback => {
                for model_id in &entry.models {
                    if let Ok((_, model)) = self.find_provider_by_model(model_id) {
                        if model.supports(capability) {
                            return Ok(model);
                        }
                    }
                }
                anyhow::bail!("All fallback models failed for {:?}", capability)
            }
            RoutingStrategy::Cheapest | RoutingStrategy::Fastest => {
                let entry = RouteEntry {
                    strategy: RoutingStrategy::Fixed,
                    models: entry.models.clone(),
                    provider: entry.provider.clone(),
                };
                self.select_model(&entry, capability)
            }
        }
    }

    pub fn get_chat_routing(&self) -> anyhow::Result<&RouteEntry> {
        self.routing.get(Capability::Chat)
            .with_context(|| "No chat routing configured")
    }

    pub fn resolve_model(&self, model_id: &str) -> anyhow::Result<(&str, &ModelConfig)> {
        let (provider, model) = self.find_provider_by_model(model_id)?;
        Ok((&provider.name, model))
    }

    pub fn provider_names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(|s| s.as_str())
    }

    pub fn get_provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }
}

impl ServiceRegistry for Registry {
    fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String) {
        self.chat_providers.insert(model_id, provider.into());
    }

    fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> {
        let entry = self.routing.get(capability)
            .with_context(|| format!("No routing for {:?}", capability))?;
        let model = self.select_model(entry, capability)?;
        let provider = self.chat_providers.get(&model.model_id)
            .with_context(|| format!("No live provider for model: {}", model.model_id))?;
        Ok((Arc::clone(provider), model.model_id.clone()))
    }

    fn get_chat_provider_with_hint(
        &self,
        capability: Capability,
        provider_hint: Option<&str>,
    ) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> {
        if let Some(hint) = provider_hint {
            let provider_cfg = self.providers.get(hint)
                .with_context(|| format!("Unknown provider: {}", hint))?;
            let model = provider_cfg.models.first()
                .with_context(|| format!("Provider {} has no models", hint))?;
            let provider = self.chat_providers.get(&model.model_id)
                .with_context(|| format!("No live provider for model: {}", model.model_id))?;
            Ok((Arc::clone(provider), model.model_id.clone()))
        } else {
            self.get_chat_provider(capability)
        }
    }

    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)> {
        anyhow::bail!("Embedding provider not yet implemented")
    }

    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)> {
        anyhow::bail!("ImageGeneration provider not yet implemented")
    }

    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)> {
        anyhow::bail!("TTS provider not yet implemented")
    }

    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)> {
        anyhow::bail!("VideoGeneration provider not yet implemented")
    }
}