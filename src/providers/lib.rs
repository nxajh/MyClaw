//! providers — Provider implementations for OpenAI, Anthropic, GLM, Kimi, MiniMax.

// NOTE: This file is DEPRECATED. All providers module code now lives in
// src/providers/mod.rs (the actual module entry for the `providers` sub-tree).
// This file exists only for historical compatibility and will be removed.
//
// All re-exports are now handled by src/providers/mod.rs

// Re-export everything from the module tree at crate::providers
pub use crate::providers::{
    // Traits
    Capability, ChatFeatures,
    ChatProvider, ChatRequest, ChatResponse, ChatMessage, ContentPart,
    StopReason, StreamEvent, ToolCall, ToolSpec, ThinkingConfig, ImageDetail, ChatUsage,
    BoxStream,
    EmbeddingProvider, EmbedRequest, EmbedResponse, EmbeddingUsage, EmbedInput,
    Tool, ToolResult,
    ImageGenerationProvider, ImageRequest, ImageResponse, ImageFormat, ImageOutput,
    SearchProvider, SearchRequest, SearchResult, SearchResults,
    SttProvider, SttRequest, TranscriptionResponse, SttSegment,
    TtsProvider, TtsFormat, TtsVoice,
    VideoGenerationProvider, VideoRequest, VideoResponse,

    // Implementations
    AnthropicProvider, GlmProvider, KimiProvider, MiniMaxProvider, OpenAiProvider,
    FallbackChatProvider,

    // Shared utilities
    AuthStyle, ProviderHandle, ProviderInstance,
    create_provider, create_provider_by_url,
    create_full_openai_provider, parse_openai_sse, build_openai_chat_body,

    // HTTP client
    Client,
};
