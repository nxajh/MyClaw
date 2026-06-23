//! HTTP request tool — generic HTTP client (GET/POST/PUT/DELETE/PATCH).
//!
//! Also subsumes the former `web_fetch` tool: set `strip_html: true` to
//! automatically convert HTML responses to clean text (removing scripts,
//! styles, tags, and decoding entities).

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use tokio::time::{Duration, timeout};

pub struct HttpRequestTool {
    client: reqwest::Client,
}

impl HttpRequestTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Naive HTML tag stripper — removes tags and decodes basic entities.
fn strip_html(html: &str) -> String {
    // Remove <script> and <style> blocks entirely.
    let re_script = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
    let re_style = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap();
    let text = re_script.replace_all(html, "");
    let text = re_style.replace_all(&text, "");

    // Remove HTML tags.
    let re_tag = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re_tag.replace_all(&text, "");

    // Decode basic entities.
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // Collapse whitespace.
    let re_ws = regex::Regex::new(r"\n{3,}").unwrap();
    let text = re_ws.replace_all(&text, "\n\n");

    text.trim().to_string()
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Make an HTTP request (GET, POST, PUT, DELETE, PATCH) to any URL. Returns status code and body. \
         Follows up to 5 redirects automatically. Set strip_html=true for web pages to get clean text instead of raw HTML."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "method": {
                    "type": "string",
                    "description": "HTTP method: GET, POST, PUT, DELETE, PATCH (default: GET).",
                    "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"]
                },
                "url": {
                    "type": "string",
                    "description": "The URL to request."
                },
                "headers": {
                    "type": "object",
                    "description": "Optional HTTP headers as key-value pairs.",
                    "additionalProperties": { "type": "string" }
                },
                "body": {
                    "type": "string",
                    "description": "Optional request body (sent as JSON for POST/PUT/PATCH)."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 30)."
                },
                "strip_html": {
                    "type": "boolean",
                    "description": "If true and the response is HTML, strip tags and return clean text (default: false)."
                }
            },
            "required": ["url"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'url' is required"))?;

        let method_str = args["method"].as_str().unwrap_or("GET").to_uppercase();
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30);
        let strip_html_flag = args["strip_html"].as_bool().unwrap_or(false);

        let method = match method_str.as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "DELETE" => reqwest::Method::DELETE,
            "PATCH" => reqwest::Method::PATCH,
            other => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("unsupported HTTP method: {}", other)),
                });
            }
        };

        let mut req = self.client.request(method, url);

        // Add custom headers.
        if let Some(headers) = args.get("headers").and_then(|h| h.as_object()) {
            for (key, value) in headers {
                if let Some(v) = value.as_str() {
                    req = req.header(key.as_str(), v);
                }
            }
        }

        // Add body.
        if let Some(body) = args.get("body").and_then(|b| b.as_str()) {
            // Try to parse as JSON; if it fails, send as plain text.
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(body) {
                req = req.json(&json_val);
            } else {
                req = req.body(body.to_string());
            }
        }

        let result = timeout(Duration::from_secs(timeout_secs), req.send()).await;

        let response = match result {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("request failed: {}", e)),
                });
            }
            Err(_) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("request timed out after {}s", timeout_secs)),
                });
            }
        };

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        let output = if strip_html_flag {
            let looks_like_html = body.trim_start().starts_with('<');
            if looks_like_html || body.contains("<html") || body.contains("<!DOCTYPE") {
                format!("HTTP {}\n\n{}", status, strip_html(&body))
            } else {
                format!("HTTP {}\n\n{}", status, body)
            }
        } else {
            format!("HTTP {}\n\n{}", status, body)
        };

        Ok(ToolResult {
            success: status.is_success(),
            output,
            error: if status.is_success() {
                None
            } else {
                Some(format!("HTTP {}", status))
            },
        })
    }
}
