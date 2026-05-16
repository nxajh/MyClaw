//! OpenAI provider — Chat + Image + TTS + Embedding.
//!
//! Chat is delegated to `OpenAiChatCompletionsClient` from the protocols layer.
//! Image/TTS/Embedding are handled directly (no protocol abstraction yet).

use async_trait::async_trait;

use crate::providers::Client;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, StreamEvent,
};
use crate::providers::{
    EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider,
};
use crate::providers::{
    ImageGenerationProvider, ImageRequest, ImageResponse, ImageFormat, ImageOutput,
};
use crate::providers::{TtsProvider, TtsRequest, TtsFormat, TtsVoice};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

#[derive(Clone)]
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, client: Client::new(), user_agent: None }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn images_url(&self) -> String { format!("{}/v1/images/generations", self.base_url.trim_end_matches('/')) }
    fn embeddings_url(&self) -> String { format!("{}/v1/embeddings", self.base_url.trim_end_matches('/')) }
    fn tts_url(&self) -> String { format!("{}/v1/audio/speech", self.base_url.trim_end_matches('/')) }

    /// Build a HeaderMap with common headers (auth, content-type, optional user-agent).
    fn common_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, self.auth().parse().unwrap());
        headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
        if let Some(ref ua) = self.user_agent {
            headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
        }
        headers
    }
}

// ── ChatProvider (delegated to protocol client) ─────────────────────────────────

#[async_trait]
impl ChatProvider for OpenAiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        use crate::providers::protocols::openai::chat_completions::OpenAiChatCompletionsClient;
        let client = OpenAiChatCompletionsClient::new(self.api_key.clone(), self.base_url.clone());
        let client = if let Some(ref ua) = self.user_agent {
            client.with_user_agent(ua.clone())
        } else {
            client
        };
        client.chat(req)
    }
}

// ── ImageGenerationProvider ────────────────────────────────────────────────────

#[async_trait]
impl ImageGenerationProvider for OpenAiProvider {
    fn generate_image(&self, req: ImageRequest) -> anyhow::Result<ImageResponse> {
        let url = self.images_url();
        let headers = self.common_headers();

        let body = serde_json::json!({
            "model": req.model,
            "prompt": req.prompt,
            "n": req.n.unwrap_or(1),
            "size": match req.size {
                Some(crate::providers::image::ImageSize::Square1024) => "1024x1024",
                Some(crate::providers::image::ImageSize::Landscape1792) => "1792x1024",
                Some(crate::providers::image::ImageSize::Portrait1024) => "1024x1792",
                None => "1024x1024",
            },
            "quality": match req.quality {
                Some(crate::providers::image::ImageQuality::HD) => "hd",
                Some(crate::providers::image::ImageQuality::Standard) | None => "standard",
            },
            "response_format": match req.response_format {
                Some(ImageFormat::Url) | None => "url",
                Some(ImageFormat::B64Json) => "b64_json",
            },
        });

        let text = futures::executor::block_on(async move {
            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let resp = resp.error_for_status()?;
            resp.text().await
        })?;

        #[derive(serde::Deserialize)]
        struct ImgResp { data: Vec<ImgData> }
        #[derive(serde::Deserialize)]
        struct ImgData { url: Option<String>, b64_json: Option<String>, revised_prompt: Option<String> }

        let resp: ImgResp = serde_json::from_str(&text)?;
        let images = resp.data.into_iter().map(|d| ImageOutput {
            url: d.url,
            b64_json: d.b64_json,
            revised_prompt: d.revised_prompt,
        }).collect();

        Ok(ImageResponse { images, usage: None })
    }
}

// ── TtsProvider ──────────────────────────────────────────────────────────────

#[async_trait]
impl TtsProvider for OpenAiProvider {
    fn synthesize(&self, req: TtsRequest) -> anyhow::Result<crate::providers::tts::AudioResponse> {
        let url = self.tts_url();
        let auth = self.auth();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
        headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

        let voice_id = match &req.voice {
            TtsVoice::Id(id) => id.clone(),
        };
        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
            "voice": voice_id,
            "response_format": match req.response_format {
                Some(TtsFormat::Mp3) | None => "mp3",
                Some(TtsFormat::Opus) => "opus",
                Some(TtsFormat::Flac) => "flac",
                Some(TtsFormat::Wav) => "wav",
            },
            "speed": req.speed.unwrap_or(1.0),
        });

        let bytes = futures::executor::block_on(async move {
            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let resp = resp.error_for_status()?;
            resp.bytes().await
        })?;

        Ok(crate::providers::tts::AudioResponse {
            audio: crate::providers::tts::AudioData {
                bytes: bytes.to_vec(),
                mime_type: "audio/mp3".to_string(),
            },
            usage: None,
        })
    }
}

// ── EmbeddingProvider ─────────────────────────────────────────────────────────

impl EmbeddingProvider for OpenAiProvider {
    fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse> {
        let url = self.embeddings_url();
        let headers = self.common_headers();

        let input = match &req.input {
            EmbedInput::Text(t) => serde_json::json!(vec![t.clone()]),
            EmbedInput::Texts(ts) => serde_json::json!(ts.clone()),
        };

        let mut body = serde_json::json!({ "model": req.model, "input": input });
        if let Some(dim) = req.dimensions {
            body["dimensions"] = serde_json::json!(dim);
        }

        let text = futures::executor::block_on(async move {
            let resp = self.client.post(&url).headers(headers).json(&body).send().await?;
            let resp = resp.error_for_status()?;
            resp.text().await
        })?;

        #[derive(serde::Deserialize)]
        struct Er { data: Vec<Ed>, usage: Option<Eu> }
        #[derive(serde::Deserialize)]
        struct Ed { embedding: Vec<f32> }
        #[derive(serde::Deserialize)]
        struct Eu { prompt_tokens: u64 }

        let resp: Er = serde_json::from_str(&text)?;
        let usage = resp.usage.map(|u| crate::providers::EmbeddingUsage { prompt_tokens: u.prompt_tokens });
        let embeddings = resp.data.into_iter().flat_map(|d| d.embedding).collect();

        Ok(EmbedResponse { embeddings, usage, model: req.model })
    }
}
