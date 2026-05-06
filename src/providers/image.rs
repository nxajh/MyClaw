//! Image generation capability.

use async_trait::async_trait;

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
