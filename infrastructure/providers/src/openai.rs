//! OpenAI provider — implements ChatProvider + ImageGenerationProvider + TtsProvider + EmbeddingProvider.

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::shared::{parse_openai_sse, build_openai_chat_body, AuthStyle};
use crate::Client;
use capability::chat::{
    BoxStream, ChatProvider, ChatRequest, StreamEvent, StopReason,
};
use capability::image::{ImageGenerationProvider, ImageRequest, ImageResponse, ImageFormat, ImageOutput};
use capability::tts::{TtsProvider, TtsRequest, TtsFormat, TtsVoice};
use capability::embedding::{EmbedRequest, EmbedResponse, EmbeddingProvider, EmbedInput};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

#[derive(Clone)]
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    auth_style: AuthStyle,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, auth_style: AuthStyle::Bearer, client: Client::new() }
    }

    fn chat_url(&self) -> String {
        if self.base_url.contains("/v1") || self.base_url.contains("/v4") {
            format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
        } else {
            format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'))
        }
    }
    fn images_url(&self) -> String { format!("{}/v1/images/generations", self.base_url.trim_end_matches('/')) }
    fn embeddings_url(&self) -> String { format!("{}/v1/embeddings", self.base_url.trim_end_matches('/')) }
    fn tts_url(&self) -> String { format!("{}/v1/audio/speech", self.base_url.trim_end_matches('/')) }
    fn auth(&self) -> String { crate::shared::build_auth(&self.auth_style, &self.api_key) }
}

// ── ChatProvider ───────────────────────────────────────────────────────────────

#[async_trait]
impl ChatProvider for OpenAiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.chat_url();
        let body = build_openai_chat_body(&req);
        let auth = self.auth();
        let client = self.client.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

            let resp = match client.post(&url).headers(headers).json(&body).send().await {
                Ok(r) => r,
                Err(e) => { let _ = tx.send(StreamEvent::Error(e.to_string())).await; return; }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let _ = tx.send(StreamEvent::Error(format!("HTTP {}: {}", status, text))).await;
                return;
            }

            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();

            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => { let _ = tx.send(StreamEvent::Error(e.to_string())).await; return; }
                };
                utf8_buf.extend_from_slice(&bytes);
                let try_decode = std::str::from_utf8(&utf8_buf);
                let text = match try_decode {
                    Ok(s) => { let owned = s.to_string(); utf8_buf.clear(); owned }
                    Err(e) => {
                        let valid = e.valid_up_to();
                        if valid == 0 && utf8_buf.len() < 4 { continue; }
                        let t = String::from_utf8_lossy(&utf8_buf[..valid]).into_owned();
                        utf8_buf.clear();
                        t
                    }
                };
                if text.is_empty() { continue; }
                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].to_string();
                    buffer.drain(..=pos);
                    if let Some(event) = parse_openai_sse(&line) {
                        let _ = tx.send(event).await;
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done { reason: StopReason::EndTurn }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

// ── ImageGenerationProvider ────────────────────────────────────────────────────

#[async_trait]
impl ImageGenerationProvider for OpenAiProvider {
    fn generate_image(&self, req: ImageRequest) -> anyhow::Result<ImageResponse> {
        let url = self.images_url();
        let auth = self.auth();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
        headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

        let body = serde_json::json!({
            "model": req.model,
            "prompt": req.prompt,
            "n": req.n.unwrap_or(1),
            "size": match req.size {
                Some(capability::image::ImageSize::Square1024) => "1024x1024",
                Some(capability::image::ImageSize::Landscape1792) => "1792x1024",
                Some(capability::image::ImageSize::Portrait1024) => "1024x1792",
                None => "1024x1024",
            },
            "quality": match req.quality {
                Some(capability::image::ImageQuality::HD) => "hd",
                Some(capability::image::ImageQuality::Standard) | None => "standard",
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
    fn synthesize(&self, req: TtsRequest) -> anyhow::Result<capability::tts::AudioResponse> {
        let url = self.tts_url();
        let auth = self.auth();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
        headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
            "voice": match &req.voice {
                TtsVoice::Id(id) => serde_json::json!({"type": "tts-1", "voice": id})
            },
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
            resp.bytes().await
        })?;

        Ok(capability::tts::AudioResponse {
            audio: capability::tts::AudioData {
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
        let auth = self.auth();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
        headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

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
        let usage = resp.usage.map(|u| capability::embedding::EmbeddingUsage { prompt_tokens: u.prompt_tokens });
        let embeddings = resp.data.into_iter().flat_map(|d| d.embedding).collect();

        Ok(EmbedResponse { embeddings, usage, model: req.model })
    }
}