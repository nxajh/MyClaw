//! HTTP-based MCP transport — POST requests to a remote endpoint.

use anyhow::{Context, Result, anyhow, bail};
use tokio::time::{Duration, timeout};

use crate::mcp::config_types::McpServerConfig;
use crate::mcp::protocol::JSONRPC_VERSION;
use crate::mcp::protocol::{JsonRpcRequest, JsonRpcResponse};
use super::{
    McpTransportConn, RECV_TIMEOUT_SECS,
    MCP_STREAMABLE_ACCEPT, MCP_JSON_CONTENT_TYPE, MCP_SESSION_ID_HEADER,
    parse_jsonrpc_response_text, read_first_jsonrpc_from_sse_response,
};

/// HTTP-based transport (POST requests).
pub struct HttpTransport {
    url: String,
    client: reqwest::Client,
    headers: std::collections::HashMap<String, String>,
    pub(super) session_id: Option<String>,
}

impl HttpTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("URL required for HTTP transport"))?
            .clone();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            url,
            client,
            headers: config.headers.clone(),
            session_id: None,
        })
    }

    pub(super) fn apply_session_header(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(session_id) = self.session_id.as_deref() {
            req.header(MCP_SESSION_ID_HEADER, session_id)
        } else {
            req
        }
    }

    pub(super) fn update_session_id_from_headers(&mut self, headers: &reqwest::header::HeaderMap) {
        if let Some(session_id) = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }
    }
}

#[async_trait::async_trait]
impl McpTransportConn for HttpTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let body = serde_json::to_string(request)?;

        let has_accept = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));
        let has_content_type = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Content-Type"));

        let mut req = self.client.post(&self.url).body(body);
        if !has_content_type {
            req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
        }
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        req = self.apply_session_header(req);
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }

        let resp = req
            .send()
            .await
            .context("HTTP request to MCP server failed")?;

        if !resp.status().is_success() {
            bail!("MCP server returned HTTP {}", resp.status());
        }

        self.update_session_id_from_headers(resp.headers());

        if request.id.is_none() {
            return Ok(JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: None,
                result: None,
                error: None,
            });
        }

        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        if is_sse {
            let maybe_resp = timeout(
                Duration::from_secs(RECV_TIMEOUT_SECS),
                read_first_jsonrpc_from_sse_response(resp),
            )
            .await
            .context("timeout waiting for MCP response from streamable HTTP SSE stream")??;
            return maybe_resp
                .ok_or_else(|| anyhow!("MCP server returned no response in SSE stream"));
        }

        let resp_text = resp.text().await.context("failed to read HTTP response")?;
        parse_jsonrpc_response_text(&resp_text)
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
