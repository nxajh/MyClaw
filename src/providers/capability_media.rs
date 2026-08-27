//! Non-chat media & embedding capabilities: embedding, speech-to-text,
//! text-to-speech, video generation, image generation.
//!
//! Merged from `capability_embedding.rs` / `stt.rs` / `tts.rs` / `video.rs` /
//! `image.rs` (#151 Phase 10, pure relocation — no behavior change).

use async_trait::async_trait;

// ── Embedding (was capability_embedding.rs) ────────────────────────────────────────────────────

pub struct EmbedRequest {
    pub input: EmbedInput,
    pub model: String,
    /// Embedding dimensions (supported by a subset of providers).
    pub dimensions: Option<u32>,
}

pub enum EmbedInput {
    Text(String),
    Texts(Vec<String>),
}

pub struct EmbedResponse {
    pub embeddings: Vec<f32>,
    pub usage: Option<EmbeddingUsage>,
    pub model: String,
}

pub struct EmbeddingUsage {
    pub prompt_tokens: u64,
}

/// Provider handles batching internally: single Text → Vec::from([text]), multiple Texts → forwarded.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse>;
}

// ── Speech-to-Text (STT) (was stt.rs) ────────────────────────────────────────────────────

pub struct SttRequest {
    pub model: String,
    pub audio: SttAudioInput,
    pub language: Option<String>,
    pub auto_detect: Option<bool>,
}

pub enum SttAudioInput {
    Url(String),
    Bytes { data: Vec<u8>, mime_type: String },
}

pub struct TranscriptionResponse {
    pub text: String,
    pub language: Option<String>,
    pub duration_secs: Option<f32>,
    pub segments: Option<Vec<SttSegment>>,
    pub usage: Option<SttUsage>,
}

pub struct SttSegment {
    pub start_secs: f32,
    pub end_secs: f32,
    pub text: String,
}

pub struct SttUsage {
    pub audio_duration_secs: f32,
    pub prompt_tokens: Option<u64>,
}

#[async_trait]
pub trait SttProvider: Send + Sync {
    fn transcribe(&self, req: SttRequest) -> anyhow::Result<TranscriptionResponse>;
}

// ── Text-to-Speech (TTS) (was tts.rs) ────────────────────────────────────────────────────

pub struct TtsRequest {
    pub model: String,
    pub input: String,
    pub voice: TtsVoice,
    pub response_format: Option<TtsFormat>,
    /// Playback speed 0.25–4.0, default 1.0.
    pub speed: Option<f32>,
}

pub enum TtsVoice {
    Id(String),
}

pub enum TtsFormat {
    Mp3,
    Opus,
    Flac,
    Wav,
}

pub struct AudioResponse {
    pub audio: AudioData,
    pub usage: Option<TtsUsage>,
}

pub struct AudioData {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

pub struct TtsUsage {
    pub characters: u64,
    pub audio_duration_secs: Option<f32>,
}

#[async_trait]
pub trait TtsProvider: Send + Sync {
    fn synthesize(&self, req: TtsRequest) -> anyhow::Result<AudioResponse>;
}

// ── Video generation (was video.rs) ────────────────────────────────────────────────────

pub struct VideoRequest {
    pub model: String,
    pub prompt: String,
    pub duration_secs: Option<u32>,
    pub resolution: Option<VideoResolution>,
    pub aspect_ratio: Option<AspectRatio>,
}

#[derive(Debug, Clone, Copy)]
pub enum VideoResolution {
    Standard,
    HD,
}

#[derive(Debug, Clone, Copy)]
pub enum AspectRatio {
    Landscape16x9,
    Portrait9x16,
    Square1x1,
}

pub struct VideoResponse {
    pub videos: Vec<VideoOutput>,
    pub usage: Option<VideoUsage>,
}

pub struct VideoOutput {
    pub url: Option<String>,
    pub path: Option<String>,
    pub revised_prompt: Option<String>,
}

pub struct VideoUsage {
    pub video_duration_secs: u32,
    pub prompt_tokens: u64,
}

#[async_trait]
pub trait VideoGenerationProvider: Send + Sync {
    fn generate_video(&self, req: VideoRequest) -> anyhow::Result<VideoResponse>;
}

// ── Image generation (was image.rs) ────────────────────────────────────────────────────

pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    pub response_format: Option<ImageFormat>,
    pub size: Option<ImageSize>,
    pub quality: Option<ImageQuality>,
    pub n: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub enum ImageFormat {
    Url,
    B64Json,
}

#[derive(Debug, Clone, Copy)]
pub enum ImageSize {
    Square1024,
    Landscape1792,
    Portrait1024,
}

#[derive(Debug, Clone, Copy)]
pub enum ImageQuality {
    Standard,
    HD,
}

pub struct ImageResponse {
    pub images: Vec<ImageOutput>,
    pub usage: Option<ImageGenerationUsage>,
}

pub struct ImageOutput {
    pub url: Option<String>,
    pub b64_json: Option<String>,
    pub revised_prompt: Option<String>,
}

pub struct ImageGenerationUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: Option<u64>,
}

#[async_trait]
pub trait ImageGenerationProvider: Send + Sync {
    fn generate_image(&self, req: ImageRequest) -> anyhow::Result<ImageResponse>;
}

