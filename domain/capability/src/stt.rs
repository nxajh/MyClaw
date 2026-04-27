//! Speech-to-Text capability.

use async_trait::async_trait;

pub struct SttRequest {
    pub model: String,
    pub audio: SttAudioInput,
    /// BCP-47 language tag, e.g. "en", "zh", "zh-CN".
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