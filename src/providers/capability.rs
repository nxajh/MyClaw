//! Capability enum and ChatFeatures configuration marker.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Chat,
    Vision,
    NativeTools,
    Search,
    Embedding,
    ImageGeneration,
    TextToSpeech,
    SpeechToText,
    VideoGeneration,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::Chat => "chat",
            Capability::Vision => "vision",
            Capability::NativeTools => "native-tools",
            Capability::Search => "search",
            Capability::Embedding => "embedding",
            Capability::ImageGeneration => "image-generation",
            Capability::TextToSpeech => "text-to-speech",
            Capability::SpeechToText => "speech-to-text",
            Capability::VideoGeneration => "video-generation",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "chat" => Some(Capability::Chat),
            "vision" => Some(Capability::Vision),
            "native-tools" => Some(Capability::NativeTools),
            "search" => Some(Capability::Search),
            "embedding" => Some(Capability::Embedding),
            "image-generation" => Some(Capability::ImageGeneration),
            "text-to-speech" => Some(Capability::TextToSpeech),
            "speech-to-text" => Some(Capability::SpeechToText),
            "video-generation" => Some(Capability::VideoGeneration),
            _ => None,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Chat capability feature flags.
/// These are configuration markers, not independent capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatFeatures {
    /// Supports vision / image input
    #[serde(default)]
    pub vision: bool,
    /// Supports audio input
    #[serde(default)]
    pub audio_input: bool,
    /// Supports video input
    #[serde(default)]
    pub video_input: bool,
    /// Supports native tool calling (not via text-modeling workaround)
    #[serde(default)]
    pub native_tools: bool,
    /// Maximum image size in pixels (HxW), if known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_image_size: Option<u64>,
    /// Supported image formats
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_image_formats: Vec<String>,
}
