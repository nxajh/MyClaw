//! Kimi (Moonshot) provider — OpenAI-compatible protocol.

use async_trait::async_trait;
use futures_util::StreamExt;

use crate::providers::Client;
use crate::providers::shared::{parse_openai_sse, build_openai_chat_body};
use crate::providers::{BoxStream, ChatProvider, ChatRequest, StreamEvent, StopReason};

const DEFAULT_BASE_URL: &str = "https://api.moonshot.cn/v1";

#[derive(Clone)]
pub struct KimiProvider {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl KimiProvider {
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
}

#[async_trait]
impl ChatProvider for KimiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_openai_chat_body(&req);
        let body_str = serde_json::to_string_pretty(&body).unwrap_or_default();
        crate::providers::append_to_debug_log(&format!(
            "=== REQUEST ===\nURL: {}\nBody:\n{}\n",
            url, body_str
        ));
        let auth = crate::providers::shared::build_auth(&crate::providers::shared::AuthStyle::Bearer, &self.api_key);
        let client = self.client.clone();
        let user_agent = self.user_agent.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth.parse().unwrap());
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
            if let Some(ref ua) = user_agent {
                headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
            }

            let resp = match client.post(&url).headers(headers).json(&body).send().await {
                Ok(r) => r,
                Err(e) => { let _ = tx.send(StreamEvent::Error(e.to_string())).await; return; }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                crate::providers::append_to_debug_log(&format!(
                    "=== HTTP ERROR ===\nURL: {}\nStatus: {}\nBody: {}\n",
                    url, status, text
                ));
                let _ = tx.send(StreamEvent::HttpError {
                    status: status.as_u16(),
                    message: format!("HTTP {}: {}", status, text),
                }).await;
                return;
            }

            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();
            crate::providers::append_to_debug_log(&format!("=== SSE STREAM START ===\nURL: {}\n", url));

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
                    let event = parse_openai_sse(&line);
                    crate::providers::append_to_debug_log(&format!(
                        "SSE LINE: {}\nEVENT: {:?}\n",
                        line, event
                    ));
                    if let Some(event) = event {
                        let _ = tx.send(event).await;
                    }
                }
            }
            crate::providers::append_to_debug_log(&format!("=== SSE STREAM END ===\nURL: {}\n\n", url));
            let _ = tx.send(StreamEvent::Done { reason: StopReason::EndTurn }).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}