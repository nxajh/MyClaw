//! Video generation capability.

use async_trait::async_trait;

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