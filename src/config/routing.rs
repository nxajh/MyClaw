//! Routing configuration — model selection strategies per capability.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::providers::Capability;

// ── RoutingStrategy ───────────────────────────────────────────────────────────

/// How to select among multiple candidate models.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Use the first available model.
    #[default]
    Fixed,
    /// Try models in order; fall back on failure.
    Fallback,
    /// Pick the cheapest model (future).
    Cheapest,
    /// Pick the fastest model (future).
    Fastest,
}

// ── RouteEntry ────────────────────────────────────────────────────────────────

/// A routing entry for one capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    /// Selection strategy.
    #[serde(default)]
    pub strategy: RoutingStrategy,
    /// Candidate model IDs (looked up across all providers).
    #[serde(default)]
    pub models: Vec<String>,
    /// Candidate provider names (for capabilities that route by provider).
    #[serde(default)]
    pub providers: Vec<String>,
}

// ── RoutingConfig ─────────────────────────────────────────────────────────────

/// All routing entries, keyed by capability name (e.g. "chat", "embedding").
///
/// In TOML, this is `[routing.chat]`, `[routing.embedding]`, etc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingConfig(HashMap<String, RouteEntry>);

impl RoutingConfig {
    /// Look up a route entry by capability.
    pub fn get(&self, cap: Capability) -> Option<&RouteEntry> {
        let key = match cap {
            Capability::Chat => "chat",
            Capability::Vision => "vision",
            Capability::NativeTools => "native_tools",
            Capability::Search => "search",
            Capability::Embedding => "embedding",
            Capability::ImageGeneration => "image_generation",
            Capability::TextToSpeech => "text_to_speech",
            Capability::SpeechToText => "speech_to_text",
            Capability::VideoGeneration => "video_generation",
        };
        self.0.get(key)
    }

    /// Insert a route entry (for programmatic construction).
    pub fn insert(&mut self, cap: Capability, entry: RouteEntry) {
        let key = match cap {
            Capability::Chat => "chat",
            Capability::Vision => "vision",
            Capability::NativeTools => "native_tools",
            Capability::Search => "search",
            Capability::Embedding => "embedding",
            Capability::ImageGeneration => "image_generation",
            Capability::TextToSpeech => "text_to_speech",
            Capability::SpeechToText => "speech_to_text",
            Capability::VideoGeneration => "video_generation",
        };
        self.0.insert(key.to_string(), entry);
    }

    /// Iterate over all route entries.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &RouteEntry)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of route entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Is the routing table empty?
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_routing() {
        let toml_str = r#"
[chat]
strategy = "fallback"
models = ["minimax-m2.7", "gpt-4o"]

[embedding]
strategy = "fixed"
models = ["jina-embeddings-v3"]
"#;
        let routing: RoutingConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(routing.len(), 2);

        let chat = routing.get(Capability::Chat).unwrap();
        assert_eq!(chat.strategy, RoutingStrategy::Fallback);
        assert_eq!(chat.models, vec!["minimax-m2.7", "gpt-4o"]);

        let emb = routing.get(Capability::Embedding).unwrap();
        assert_eq!(emb.strategy, RoutingStrategy::Fixed);
        assert_eq!(emb.models, vec!["jina-embeddings-v3"]);
    }
}
