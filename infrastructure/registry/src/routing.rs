//! Routing configuration types.

use capability::capability::Capability;
use serde::{Deserialize, Serialize};

/// Routing strategy for selecting a model/provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum RoutingStrategy {
    #[default]
    Fixed,
    Fallback,
    Cheapest,
    Fastest,
}

/// A single routing rule for one capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub strategy: RoutingStrategy,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Routing configuration for all capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<RouteEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<RouteEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<RouteEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_generation: Option<RouteEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_to_speech: Option<RouteEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_to_text: Option<RouteEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_generation: Option<RouteEntry>,
}

impl RoutingConfig {
    pub fn get(&self, capability: Capability) -> Option<&RouteEntry> {
        match capability {
            Capability::Chat => self.chat.as_ref(),
            Capability::Search => self.search.as_ref(),
            Capability::Embedding => self.embedding.as_ref(),
            Capability::ImageGeneration => self.image_generation.as_ref(),
            Capability::TextToSpeech => self.text_to_speech.as_ref(),
            Capability::SpeechToText => self.speech_to_text.as_ref(),
            Capability::VideoGeneration => self.video_generation.as_ref(),
            Capability::Vision | Capability::NativeTools => self.chat.as_ref(),
        }
    }
}