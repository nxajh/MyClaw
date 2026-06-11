//! Stdio-based MCP transport — spawns a local process and communicates via stdin/stdout.

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{Duration, timeout};

use super::{MAX_LINE_BYTES, McpTransportConn, RECV_TIMEOUT_SECS};
use crate::mcp::config_types::McpServerConfig;
use crate::mcp::protocol::JSONRPC_VERSION;
use crate::mcp::protocol::{JsonRpcRequest, JsonRpcResponse};

/// Stdio-based transport (spawn local process).
pub struct StdioTransport {
    _child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl StdioTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn MCP server `{}`", config.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("no stdin on MCP server `{}`", config.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no stdout on MCP server `{}`", config.name))?;
        let stdout_lines = BufReader::new(stdout).lines();

        Ok(Self {
            _child: child,
            stdin,
            stdout_lines,
        })
    }

    async fn send_raw(&mut self, line: &str) -> Result<()> {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("failed to write to MCP server stdin")?;
        self.stdin
            .write_all(b"\n")
            .await
            .context("failed to write newline to MCP server stdin")?;
        self.stdin.flush().await.context("failed to flush stdin")?;
        Ok(())
    }

    async fn recv_raw(&mut self) -> Result<String> {
        let line = self
            .stdout_lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("MCP server closed stdout"))?;
        if line.len() > MAX_LINE_BYTES {
            bail!("MCP response too large: {} bytes", line.len());
        }
        Ok(line)
    }
}

#[async_trait::async_trait]
impl McpTransportConn for StdioTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let line = serde_json::to_string(request)?;
        self.send_raw(&line).await?;
        if request.id.is_none() {
            return Ok(JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: None,
                result: None,
                error: None,
            });
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(RECV_TIMEOUT_SECS);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for MCP response");
            }
            let resp_line = timeout(remaining, self.recv_raw())
                .await
                .context("timeout waiting for MCP response")??;
            let resp: JsonRpcResponse = serde_json::from_str(&resp_line)
                .with_context(|| format!("invalid JSON-RPC response: {}", resp_line))?;
            if resp.id.is_none() {
                // Server-sent notification (e.g. `notifications/initialized`) — skip and
                // keep waiting for the actual response to our request.
                tracing::debug!(
                    "MCP stdio: skipping server notification while waiting for response"
                );
                continue;
            }
            return Ok(resp);
        }
    }

    async fn close(&mut self) -> Result<()> {
        let _ = self.stdin.shutdown().await;
        Ok(())
    }
}
