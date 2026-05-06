//! Text-to-Speech capability.

use async_trait::async_trait;

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
