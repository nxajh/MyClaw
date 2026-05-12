//! ServiceRegistry — capability routing center.

// Declare routing submodule FIRST (before imports that use it).
pub mod routing;

use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Context;

use crate::providers::Capability;
use crate::providers::capability_chat::ChatProvider;
use crate::providers::capability_embedding::EmbeddingProvider;
use crate::providers::image::ImageGenerationProvider;
use crate::providers::search::SearchProvider;
use crate::providers::service_registry::ServiceRegistry;
use crate::providers::stt::SttProvider;
use crate::providers::tts::TtsProvider;
use crate::providers::video::VideoGenerationProvider;

use crate::registry::routing::{RouteEntry, RoutingConfig, RoutingStrategy};

// ── Config types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub api: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub models: Vec<ModelConfig>,
}

/// Registry-level model config (converted from crate::config::ModelConfig).
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

// ── Registry ───────────────────────────────────────────────────────────────────

pub struct Registry {
    providers: HashMap<String, ProviderConfig>,
    routing: RoutingConfig,
    chat_providers: HashMap<String, Arc<dyn ChatProvider>>,
    chat_model_configs: HashMap<String, crate::providers::capability::ChatModelConfig>,
    embedding_providers: HashMap<String, Arc<dyn EmbeddingProvider>>,
    image_providers: HashMap<String, Arc<dyn ImageGenerationProvider>>,
    tts_providers: HashMap<String, Arc<dyn TtsProvider>>,
    video_providers: HashMap<String, Arc<dyn VideoGenerationProvider>>,
    search_providers: HashMap<String, Arc<dyn SearchProvider>>,
    stt_providers: HashMap<String, Arc<dyn SttProvider>>,
}

impl Registry {
    pub fn new(providers: HashMap<String, ProviderConfig>, routing: RoutingConfig) -> Self {
        Self {
            providers,
            routing,
            chat_providers: HashMap::new(),
            chat_model_configs: HashMap::new(),
            embedding_providers: HashMap::new(),
            image_providers: HashMap::new(),
            tts_providers: HashMap::new(),
            video_providers: HashMap::new(),
            search_providers: HashMap::new(),
            stt_providers: HashMap::new(),
        }
    }

    fn find_provider_by_model(&self, model_id: &str) -> anyhow::Result<(&str, &ModelConfig)> {
        tracing::debug!(model_id, "find_provider_by_model");
        for (key, provider) in &self.providers {
            for model in &provider.models {
                tracing::debug!(provider = %key, model = %model.model_id, "checking");
                if model.model_id == model_id {
                    return Ok((key.as_str(), model));
                }
            }
        }
        tracing::warn!(model_id, available_providers = ?self.providers.keys().collect::<Vec<_>>(), "No provider found for model");
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
                // Not yet implemented: treated as Fixed (first model in list).
                tracing::warn!(
                    strategy = ?entry.strategy,
                    "routing strategy not implemented, falling back to Fixed"
                );
                let entry = RouteEntry {
                    strategy: RoutingStrategy::Fixed,
                    models: entry.models.clone(),
                    providers: entry.providers.clone(),
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
        let (provider_key, model) = self.find_provider_by_model(model_id)?;
        Ok((provider_key, model))
    }

    pub fn provider_names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(|s| s.as_str())
    }

    pub fn get_provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    /// Build a Registry from config types.
    pub fn from_config(
        providers: HashMap<String, crate::config::provider::ProviderConfig>,
        routing: &crate::config::routing::RoutingConfig,
    ) -> anyhow::Result<Self> {
        let registry_providers: HashMap<String, ProviderConfig> = providers
            .into_iter()
            .map(|(api, cfg)| {
                let mut models = Vec::new();
                let mut base_url: Option<String> = None;

                // Collect models from all capability sections
                if let Some(ref chat) = cfg.chat {
                    base_url = base_url.or_else(|| Some(chat.base_url.clone()));
                    for (id, mc) in &chat.models {
                        models.push(ModelConfig {
                            model_id: id.clone(),
                            capabilities: vec![Capability::Chat],
                            context_window: mc.context_window,
                            max_tokens: mc.max_output_tokens,
                            reasoning: mc.reasoning,
                        });
                    }
                }
                if let Some(ref emb) = cfg.embedding {
                    base_url = base_url.or_else(|| Some(emb.base_url.clone()));
                    for id in emb.models.keys() {
                        models.push(ModelConfig {
                            model_id: id.clone(),
                            capabilities: vec![Capability::Embedding],
                            context_window: None,
                            max_tokens: None,
                            reasoning: false,
                        });
                    }
                }
                if let Some(ref sec) = cfg.image_generation {
                    base_url = base_url.or_else(|| Some(sec.base_url.clone()));
                    for id in sec.models.keys() {
                        models.push(ModelConfig {
                            model_id: id.clone(),
                            capabilities: vec![Capability::ImageGeneration],
                            context_window: None,
                            max_tokens: None,
                            reasoning: false,
                        });
                    }
                }
                if let Some(ref sec) = cfg.tts {
                    base_url = base_url.or_else(|| Some(sec.base_url.clone()));
                    for id in sec.models.keys() {
                        models.push(ModelConfig {
                            model_id: id.clone(),
                            capabilities: vec![Capability::TextToSpeech],
                            context_window: None,
                            max_tokens: None,
                            reasoning: false,
                        });
                    }
                }
                if let Some(ref sec) = cfg.stt {
                    base_url = base_url.or_else(|| Some(sec.base_url.clone()));
                    for id in sec.models.keys() {
                        models.push(ModelConfig {
                            model_id: id.clone(),
                            capabilities: vec![Capability::SpeechToText],
                            context_window: None,
                            max_tokens: None,
                            reasoning: false,
                        });
                    }
                }
                if let Some(ref sec) = cfg.video {
                    base_url = base_url.or_else(|| Some(sec.base_url.clone()));
                    for id in sec.models.keys() {
                        models.push(ModelConfig {
                            model_id: id.clone(),
                            capabilities: vec![Capability::VideoGeneration],
                            context_window: None,
                            max_tokens: None,
                            reasoning: false,
                        });
                    }
                }
                if let Some(ref sec) = cfg.search {
                    base_url = base_url.or_else(|| Some(sec.base_url.clone()));
                    for id in sec.models.keys() {
                        models.push(ModelConfig {
                            model_id: id.clone(),
                            capabilities: vec![Capability::Search],
                            context_window: None,
                            max_tokens: None,
                            reasoning: false,
                        });
                    }
                }

                let pc = ProviderConfig {
                    api: api.clone(),
                    api_key: cfg.api_key,
                    base_url,
                    models,
                };
                (api, pc)
            })
            .collect();

        let registry_routing = RoutingConfig::from_other(routing);

        Ok(Self::new(registry_providers, registry_routing))
    }
}

// ── Type conversions (config → registry) ─────────────────────────────────────

impl From<crate::config::provider::ProviderConfig> for ProviderConfig {
    fn from(cfg: crate::config::provider::ProviderConfig) -> Self {
        Self {
            api: String::new(),
            api_key: cfg.api_key,
            base_url: None,
            models: vec![],
        }
    }
}

// ── RoutingConfig conversion (HashMap → typed struct) ─────────────────────────

impl RoutingConfig {
    /// Convert from config's RoutingConfig (HashMap<String, RouteEntry>)
    /// to registry's RoutingConfig (flat struct with typed fields).
    pub fn from_other(other: &crate::config::routing::RoutingConfig) -> Self {
        fn convert_entry(e: &crate::config::routing::RouteEntry) -> RouteEntry {
            RouteEntry {
                strategy: match e.strategy {
                    crate::config::routing::RoutingStrategy::Fixed => RoutingStrategy::Fixed,
                    crate::config::routing::RoutingStrategy::Fallback => RoutingStrategy::Fallback,
                    crate::config::routing::RoutingStrategy::Cheapest => RoutingStrategy::Cheapest,
                    crate::config::routing::RoutingStrategy::Fastest => RoutingStrategy::Fastest,
                },
                models: e.models.clone(),
                providers: e.providers.clone(),
            }
        }

        let mut cfg = RoutingConfig::default();
        for (key, entry) in other.iter() {
            let e = convert_entry(entry);
            match key {
                "chat" => cfg.chat = Some(e),
                "search" => cfg.search = Some(e),
                "embedding" => cfg.embedding = Some(e),
                "image_generation" => cfg.image_generation = Some(e),
                "text_to_speech" => cfg.text_to_speech = Some(e),
                "speech_to_text" => cfg.speech_to_text = Some(e),
                "video_generation" => cfg.video_generation = Some(e),
                _ => {}
            }
        }
        cfg
    }
}

// ── Registry registration methods ─────────────────────────────────────────────

impl Registry {
    pub fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String, model_config: crate::providers::capability::ChatModelConfig) {
        self.chat_model_configs.insert(model_id.clone(), model_config);
        self.chat_providers.insert(model_id, provider.into());
    }

    pub fn register_embedding(&mut self, provider: Box<dyn EmbeddingProvider>, model_id: String) {
        self.embedding_providers.insert(model_id, provider.into());
    }

    pub fn register_image(&mut self, provider: Box<dyn ImageGenerationProvider>, model_id: String) {
        self.image_providers.insert(model_id, provider.into());
    }

    pub fn register_tts(&mut self, provider: Box<dyn TtsProvider>, model_id: String) {
        self.tts_providers.insert(model_id, provider.into());
    }

    pub fn register_video(&mut self, provider: Box<dyn VideoGenerationProvider>, model_id: String) {
        self.video_providers.insert(model_id, provider.into());
    }

    pub fn register_search(&mut self, provider: Box<dyn SearchProvider>, model_id: String) {
        self.search_providers.insert(model_id, provider.into());
    }

    pub fn register_stt(&mut self, provider: Box<dyn SttProvider>, model_id: String) {
        self.stt_providers.insert(model_id, provider.into());
    }

    pub fn maybe_wrap_chat_fallback(&mut self, routing: &crate::config::routing::RoutingConfig) {
        use crate::providers::FallbackChatProvider;
        use crate::providers::fallback::FallbackEntry;

        let entry = match routing.get(crate::providers::Capability::Chat) {
            Some(e) => e,
            None => return,
        };

        if entry.strategy != crate::config::routing::RoutingStrategy::Fallback {
            return;
        }
        if entry.models.len() <= 1 {
            return;
        }

        let mut chain: Vec<FallbackEntry> = Vec::new();
        for model_id in &entry.models {
            if let Some(provider) = self.chat_providers.get(model_id) {
                chain.push(FallbackEntry {
                    provider: Arc::clone(provider),
                    model_id: model_id.clone(),
                    credential_pool: None,
                });
            } else {
                tracing::warn!(model = %model_id, "model in fallback routing not registered, skipping");
            }
        }

        if chain.is_empty() {
            tracing::error!("Fallback routing configured but no chat providers found");
            return;
        }

        tracing::info!(
            models = ?chain.iter().map(|e| e.model_id.as_str()).collect::<Vec<_>>(),
            "wrapping {} chat providers with FallbackChatProvider",
            chain.len()
        );

        let fallback_provider = FallbackChatProvider::new(chain);
        let primary_model = entry.models[0].clone();
        // Insert fallback wrapper under the primary model key.
        // Individual model entries remain in the map for direct access (e.g. vision routing).
        self.chat_providers.insert(
            primary_model,
            Arc::new(fallback_provider),
        );
    }
}

// ── ServiceRegistry trait impl ─────────────────────────────────────────────────

impl ServiceRegistry for Registry {
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

    fn get_chat_fallback_chain(&self, capability: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>> {
        let entry = self.routing.get(capability)
            .with_context(|| format!("No routing for {:?}", capability))?;

        let mut chain = Vec::new();
        for model_id in &entry.models {
            if let Ok((_, m)) = self.find_provider_by_model(model_id) {
                if m.supports(capability) {
                    if let Some(provider) = self.chat_providers.get(model_id.as_str()) {
                        chain.push((Arc::clone(provider), model_id.clone()));
                    }
                }
            }
        }

        if chain.is_empty() {
            anyhow::bail!("No available providers in fallback chain for {:?}", capability);
        }

        Ok(chain)
    }

    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)> {
        self.route_capability(Capability::Embedding, &self.embedding_providers)
    }

    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)> {
        self.route_capability(Capability::ImageGeneration, &self.image_providers)
    }

    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)> {
        self.route_capability(Capability::TextToSpeech, &self.tts_providers)
    }

    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)> {
        self.route_capability(Capability::VideoGeneration, &self.video_providers)
    }

    fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)> {
        self.route_capability(Capability::Search, &self.search_providers)
    }

    fn get_search_fallback_chain(&self) -> anyhow::Result<Vec<(Arc<dyn SearchProvider>, String)>> {
        let entry = self.routing.get(Capability::Search)
            .with_context(|| "No routing configured for Search")?;

        let mut chain = Vec::new();
        for model_id in &entry.models {
            if let Some(provider) = self.search_providers.get(model_id.as_str()) {
                chain.push((Arc::clone(provider), model_id.clone()));
            }
        }

        if chain.is_empty() {
            anyhow::bail!("No available search providers in fallback chain");
        }

        Ok(chain)
    }

    fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)> {
        self.route_capability(Capability::SpeechToText, &self.stt_providers)
    }

    fn get_chat_model_config(&self, model_id: &str) -> anyhow::Result<&crate::providers::capability::ChatModelConfig> {
        self.chat_model_configs.get(model_id)
            .with_context(|| format!("No chat model config found for model: {}", model_id))
    }

    fn get_chat_provider_by_model(&self, model_id: &str) -> Option<(Arc<dyn ChatProvider>, String)> {
        self.chat_providers.get(model_id)
            .map(|p| (Arc::clone(p), model_id.to_string()))
    }

    fn get_chat_routing_models(&self) -> Vec<String> {
        self.routing
            .get(Capability::Chat)
            .map(|e| e.models.clone())
            .unwrap_or_default()
    }
}

impl Registry {
    fn route_capability<T: ?Sized + Send + Sync>(
        &self,
        capability: Capability,
        store: &HashMap<String, Arc<T>>,
    ) -> anyhow::Result<(Arc<T>, String)> {
        let entry = self.routing.get(capability)
            .with_context(|| format!("No routing configured for {:?}", capability))?;
        let model = self.select_model(entry, capability)?;

        if let Some(provider) = store.get(&model.model_id) {
            return Ok((Arc::clone(provider), model.model_id.clone()));
        }

        for model_id in store.keys() {
            if let Ok((_, m)) = self.find_provider_by_model(model_id) {
                if m.supports(capability) {
                    if let Some(provider) = store.get(model_id) {
                        return Ok((Arc::clone(provider), model_id.clone()));
                    }
                }
            }
        }

        anyhow::bail!(
            "No {:?} provider registered (routing model: {}, available: [{}])",
            capability,
            model.model_id,
            store.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}
