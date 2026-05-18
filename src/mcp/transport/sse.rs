//! SSE-based MCP transport — HTTP POST for requests, SSE for responses.

use std::borrow::Cow;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::time::{Duration, timeout};
use tokio_stream::StreamExt;

use crate::mcp::config_types::McpServerConfig;
use crate::mcp::protocol::JSONRPC_VERSION;
use crate::mcp::protocol::{INTERNAL_ERROR, JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use super::{
    McpTransportConn,
    MCP_STREAMABLE_ACCEPT, MCP_JSON_CONTENT_TYPE,
    extract_json_from_sse_text, read_first_jsonrpc_from_sse_response,
};

/// SSE-based transport (HTTP POST for requests, SSE for responses).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SseStreamState {
    Unknown,
    Connected,
    Unsupported,
}

pub struct SseTransport {
    sse_url: String,
    server_name: String,
    client: reqwest::Client,
    headers: std::collections::HashMap<String, String>,
    stream_state: SseStreamState,
    shared: std::sync::Arc<Mutex<SseSharedState>>,
    notify: std::sync::Arc<Notify>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    reader_task: Option<tokio::task::JoinHandle<()>>,
}

impl SseTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let sse_url = config
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("URL required for SSE transport"))?
            .clone();

        let client = reqwest::Client::builder()
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            sse_url,
            server_name: config.name.clone(),
            client,
            headers: config.headers.clone(),
            stream_state: SseStreamState::Unknown,
            shared: std::sync::Arc::new(Mutex::new(SseSharedState::default())),
            notify: std::sync::Arc::new(Notify::new()),
            shutdown_tx: None,
            reader_task: None,
        })
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.stream_state == SseStreamState::Unsupported {
            return Ok(());
        }
        if let Some(task) = &self.reader_task {
            if !task.is_finished() {
                self.stream_state = SseStreamState::Connected;
                return Ok(());
            }
        }

        let has_accept = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));

        let mut req = self
            .client
            .get(&self.sse_url)
            .header("Cache-Control", "no-cache");
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }

        let resp = req.send().await.context("SSE GET to MCP server failed")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND
            || resp.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            self.stream_state = SseStreamState::Unsupported;
            return Ok(());
        }
        if !resp.status().is_success() {
            return Err(anyhow!("MCP server returned HTTP {}", resp.status()));
        }
        let is_event_stream = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        if !is_event_stream {
            self.stream_state = SseStreamState::Unsupported;
            return Ok(());
        }

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);

        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let sse_url = self.sse_url.clone();
        let server_name = self.server_name.clone();

        self.reader_task = Some(tokio::spawn(async move {
            let stream = resp
                .bytes_stream()
                .map(|item| item.map_err(std::io::Error::other));
            let reader = tokio_util::io::StreamReader::new(stream);
            let mut lines = BufReader::new(reader).lines();

            let mut cur_event: Option<String> = None;
            let mut cur_id: Option<String> = None;
            let mut cur_data: Vec<String> = Vec::new();

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    line = lines.next_line() => {
                        let Ok(line_opt) = line else { break; };
                        let Some(mut line) = line_opt else { break; };
                        if line.ends_with('\r') {
                            line.pop();
                        }
                        if line.is_empty() {
                            if cur_event.is_none() && cur_id.is_none() && cur_data.is_empty() {
                                continue;
                            }
                            let event = cur_event.take();
                            let data = cur_data.join("\n");
                            cur_data.clear();
                            let id = cur_id.take();
                            handle_sse_event(&server_name, &sse_url, &shared, &notify, event.as_deref(), id.as_deref(), data).await;
                            continue;
                        }

                        if line.starts_with(':') {
                            continue;
                        }

                        if let Some(rest) = line.strip_prefix("event:") {
                            cur_event = Some(rest.trim().to_string());
                        }
                        if let Some(rest) = line.strip_prefix("data:") {
                            let rest = rest.strip_prefix(' ').unwrap_or(rest);
                            cur_data.push(rest.to_string());
                        }
                        if let Some(rest) = line.strip_prefix("id:") {
                            cur_id = Some(rest.trim().to_string());
                        }
                    }
                }
            }

            let pending = {
                let mut guard = shared.lock().await;
                std::mem::take(&mut guard.pending)
            };
            for (_, tx) in pending {
                let _ = tx.send(JsonRpcResponse {
                    jsonrpc: JSONRPC_VERSION.to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: INTERNAL_ERROR,
                        message: "SSE connection closed".to_string(),
                        data: None,
                    }),
                });
            }
        }));
        self.stream_state = SseStreamState::Connected;

        Ok(())
    }

    async fn get_message_url(&self) -> Result<(String, bool)> {
        let guard = self.shared.lock().await;
        if let Some(url) = &guard.message_url {
            return Ok((url.clone(), guard.message_url_from_endpoint));
        }
        drop(guard);

        let derived = derive_message_url(&self.sse_url, "messages")
            .or_else(|| derive_message_url(&self.sse_url, "message"))
            .ok_or_else(|| anyhow!("invalid SSE URL"))?;
        let mut guard = self.shared.lock().await;
        if guard.message_url.is_none() {
            guard.message_url = Some(derived.clone());
            guard.message_url_from_endpoint = false;
        }
        Ok((derived, false))
    }

    #[allow(dead_code)] // WIP: alternate message URL fallback
    fn maybe_try_alternate_message_url(
        &self,
        current_url: &str,
        from_endpoint: bool,
    ) -> Option<String> {
        if from_endpoint {
            return None;
        }
        let alt = if current_url.ends_with("/messages") {
            derive_message_url(&self.sse_url, "message")
        } else {
            derive_message_url(&self.sse_url, "messages")
        }?;
        if alt == current_url {
            return None;
        }
        Some(alt)
    }
}

#[derive(Default)]
struct SseSharedState {
    message_url: Option<String>,
    message_url_from_endpoint: bool,
    pending: std::collections::HashMap<u64, oneshot::Sender<JsonRpcResponse>>,
}

pub(super) fn derive_message_url(sse_url: &str, message_path: &str) -> Option<String> {
    let url = reqwest::Url::parse(sse_url).ok()?;
    let mut segments: Vec<&str> = url.path_segments()?.collect();
    if segments.is_empty() {
        return None;
    }
    if segments.last().copied() == Some("sse") {
        segments.pop();
        segments.push(message_path);
        let mut new_url = url.clone();
        new_url.set_path(&format!("/{}", segments.join("/")));
        return Some(new_url.to_string());
    }
    let mut new_url = url.clone();
    let mut path = url.path().trim_end_matches('/').to_string();
    path.push('/');
    path.push_str(message_path);
    new_url.set_path(&path);
    Some(new_url.to_string())
}

async fn handle_sse_event(
    server_name: &str,
    sse_url: &str,
    shared: &std::sync::Arc<Mutex<SseSharedState>>,
    notify: &std::sync::Arc<Notify>,
    event: Option<&str>,
    _id: Option<&str>,
    data: String,
) {
    let event = event.unwrap_or("message");
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return;
    }

    if event.eq_ignore_ascii_case("endpoint") || event.eq_ignore_ascii_case("mcp-endpoint") {
        if let Some(url) = parse_endpoint_from_data(sse_url, trimmed) {
            let mut guard = shared.lock().await;
            guard.message_url = Some(url);
            guard.message_url_from_endpoint = true;
            drop(guard);
            notify.notify_waiters();
        }
        return;
    }

    if !event.eq_ignore_ascii_case("message") {
        return;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return;
    };

    let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(value.clone()) else {
        let _ = serde_json::from_value::<JsonRpcRequest>(value);
        return;
    };

    let Some(id_val) = resp.id.clone() else {
        return;
    };
    let id = match id_val.as_u64() {
        Some(v) => v,
        None => return,
    };

    let tx = {
        let mut guard = shared.lock().await;
        guard.pending.remove(&id)
    };
    if let Some(tx) = tx {
        let _ = tx.send(resp);
    } else {
        tracing::debug!(
            "MCP SSE `{}` received response for unknown id {}",
            server_name,
            id
        );
    }
}

pub(super) fn parse_endpoint_from_data(sse_url: &str, data: &str) -> Option<String> {
    if data.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(data).ok()?;
        let endpoint = v.get("endpoint")?.as_str()?;
        return parse_endpoint_from_data(sse_url, endpoint);
    }
    if data.starts_with("http://") || data.starts_with("https://") {
        return Some(data.to_string());
    }
    let base = reqwest::Url::parse(sse_url).ok()?;
    base.join(data).ok().map(|u| u.to_string())
}

#[async_trait::async_trait]
impl McpTransportConn for SseTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        self.ensure_connected().await?;

        let id = request.id.as_ref().and_then(|v| v.as_u64());
        let body = serde_json::to_string(request)?;

        let (mut message_url, mut from_endpoint) = self.get_message_url().await?;
        if self.stream_state == SseStreamState::Connected && !from_endpoint {
            for _ in 0..3 {
                {
                    let guard = self.shared.lock().await;
                    if guard.message_url_from_endpoint {
                        if let Some(url) = &guard.message_url {
                            message_url = url.clone();
                            from_endpoint = true;
                            break;
                        }
                    }
                }
                let _ = timeout(Duration::from_millis(300), self.notify.notified()).await;
            }
        }
        let primary_url = if from_endpoint {
            message_url.clone()
        } else {
            self.sse_url.clone()
        };
        let secondary_url = if message_url == self.sse_url {
            None
        } else if primary_url == message_url {
            Some(self.sse_url.clone())
        } else {
            Some(message_url.clone())
        };
        let has_secondary = secondary_url.is_some();

        let mut rx = None;
        if let Some(id) = id {
            if self.stream_state == SseStreamState::Connected {
                let (tx, ch) = oneshot::channel();
                {
                    let mut guard = self.shared.lock().await;
                    guard.pending.insert(id, tx);
                }
                rx = Some((id, ch));
            }
        }

        let mut got_direct = None;
        let mut last_status = None;

        for (i, url) in std::iter::once(primary_url)
            .chain(secondary_url)
            .enumerate()
        {
            let has_accept = self
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("Accept"));
            let has_content_type = self
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("Content-Type"));
            let mut req = self
                .client
                .post(&url)
                .timeout(Duration::from_secs(120))
                .body(body.clone());
            if !has_content_type {
                req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
            }
            for (key, value) in &self.headers {
                req = req.header(key, value);
            }
            if !has_accept {
                req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
            }

            let resp = req.send().await.context("SSE POST to MCP server failed")?;
            let status = resp.status();
            last_status = Some(status);

            if (status == reqwest::StatusCode::NOT_FOUND
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED)
                && i == 0
            {
                continue;
            }

            if !status.is_success() {
                break;
            }

            if request.id.is_none() {
                got_direct = Some(JsonRpcResponse {
                    jsonrpc: JSONRPC_VERSION.to_string(),
                    id: None,
                    result: None,
                    error: None,
                });
                break;
            }

            let is_sse = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));

            if is_sse {
                if i == 0 && has_secondary {
                    match timeout(
                        Duration::from_secs(3),
                        read_first_jsonrpc_from_sse_response(resp),
                    )
                    .await
                    {
                        Ok(res) => {
                            if let Some(resp) = res? {
                                got_direct = Some(resp);
                            }
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                if let Some(resp) = read_first_jsonrpc_from_sse_response(resp).await? {
                    got_direct = Some(resp);
                }
                break;
            }

            let text = if i == 0 && has_secondary {
                match timeout(Duration::from_secs(3), resp.text()).await {
                    Ok(Ok(t)) => t,
                    Ok(Err(_)) => String::new(),
                    Err(_) => continue,
                }
            } else {
                resp.text().await.unwrap_or_default()
            };
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let json_str = if trimmed.contains("\ndata:") || trimmed.starts_with("data:") {
                    extract_json_from_sse_text(trimmed)
                } else {
                    Cow::Borrowed(trimmed)
                };
                if let Ok(mcp_resp) = serde_json::from_str::<JsonRpcResponse>(json_str.as_ref()) {
                    got_direct = Some(mcp_resp);
                }
            }
            break;
        }

        if let Some((id, _)) = rx.as_ref() {
            if got_direct.is_some() {
                let mut guard = self.shared.lock().await;
                guard.pending.remove(id);
            } else if let Some(status) = last_status {
                if !status.is_success() {
                    let mut guard = self.shared.lock().await;
                    guard.pending.remove(id);
                }
            }
        }

        if let Some(resp) = got_direct {
            return Ok(resp);
        }

        if let Some(status) = last_status {
            if !status.is_success() {
                bail!("MCP server returned HTTP {}", status);
            }
        } else {
            bail!("MCP request not sent");
        }

        let Some((_id, rx)) = rx else {
            bail!("MCP server returned no response");
        };

        rx.await.map_err(|_| anyhow!("SSE response channel closed"))
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        Ok(())
    }
}
