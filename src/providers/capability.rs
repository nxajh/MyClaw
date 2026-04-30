//! Capability enum, Modality enum, and per-capability model configs.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Capability ────────────────────────────────────────────────────────────────

/// Top-level routing capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Chat,
    Embedding,
    ImageGeneration,
    TextToSpeech,
    SpeechToText,
    VideoGeneration,
    Search,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::Chat => "chat",
            Capability::Embedding => "embedding",
            Capability::ImageGeneration => "image-generation",
            Capability::TextToSpeech => "text-to-speech",
            Capability::SpeechToText => "speech-to-text",
            Capability::VideoGeneration => "video-generation",
            Capability::Search => "search",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "chat" => Some(Capability::Chat),
            "embedding" => Some(Capability::Embedding),
            "image-generation" => Some(Capability::ImageGeneration),
            "text-to-speech" => Some(Capability::TextToSpeech),
            "speech-to-text" => Some(Capability::SpeechToText),
            "video-generation" => Some(Capability::VideoGeneration),
            "search" => Some(Capability::Search),
            _ => None,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ── Modality ──────────────────────────────────────────────────────────────────

/// Input/output modality for a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
}

impl Modality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Modality::Text => "text",
            Modality::Image => "image",
            Modality::Audio => "audio",
            Modality::Video => "video",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Modality::Text),
            "image" => Some(Modality::Image),
            "audio" => Some(Modality::Audio),
            "video" => Some(Modality::Video),
            _ => None,
        }
    }
}

// ── Pricing ───────────────────────────────────────────────────────────────────

/// Per-model pricing for chat models (USD per million tokens).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatPricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_read: Option<f64>,
}

/// Per-model pricing for embedding models (USD per million tokens).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbeddingPricing {
    pub input: Option<f64>,
}

/// Per-model pricing for non-token-based models.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BasicPricing {
    pub per_unit: Option<f64>,
}

// ── Per-capability model configs ──────────────────────────────────────────────

/// Configuration for a chat model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatModelConfig {
    pub input: Vec<Modality>,
    pub output: Vec<Modality>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub pricing: Option<ChatPricing>,
    #[serde(default)]
    pub reasoning: bool,
}

impl ChatModelConfig {
    /// Whether the model supports image input.
    pub fn supports_image_input(&self) -> bool {
        self.input.contains(&Modality::Image)
    }
}

/// Configuration for an embedding model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelConfig {
    pub dimensions: Option<u32>,
    pub max_tokens: Option<u32>,
    pub pricing: Option<EmbeddingPricing>,
}

/// Generic configuration for image / TTS / STT / video / search models.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BasicModelConfig {
    pub pricing: Option<BasicPricing>,
}
