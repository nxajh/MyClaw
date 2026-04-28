//! Domain capability trait definitions.
//!
//! Each capability is an independent trait. Provider structs implement the
//! capability traits they support.
//!
//! ServiceRegistry is the single routing point — callers use
//! `registry.get_chat_provider()` etc. to obtain a provider for a capability.

pub mod capability;
pub mod chat;
pub mod embedding;
pub mod image;
pub mod search;
pub mod service_registry;
pub mod stt;
pub mod tool;
pub mod tts;
pub mod video;

pub use capability::{Capability, ChatFeatures};
pub use chat::{
    ChatMessage, ChatProvider, ChatRequest, ChatResponse, ChatUsage, ContentPart,
    ImageDetail, StopReason, StreamEvent, ThinkingConfig, ToolCall, ToolSpec,
};
pub use embedding::{EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider, EmbeddingUsage};
pub use image::{
    ImageFormat, ImageGenerationProvider, ImageOutput, ImageQuality, ImageRequest, ImageResponse,
    ImageSize,
};
pub use search::{SearchProvider, SearchRequest, SearchResult, SearchResults};
pub use service_registry::ServiceRegistry;
pub use stt::{SttAudioInput, SttProvider, SttSegment, SttUsage, TranscriptionResponse};
pub use tts::{
    AudioResponse, TtsFormat, TtsProvider, TtsRequest, TtsUsage, TtsVoice,
};
pub use video::{
    AspectRatio, VideoGenerationProvider, VideoOutput, VideoRequest, VideoResponse,
    VideoResolution, VideoUsage,
};