//! Web fetch tool — HTTP GET to fetch web page content.

use async_trait::async_trait;
use capability::tool::{Tool, ToolResult};
use serde_json::json;
use tokio::time::{Duration, timeout};

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("MyClaw/0.1")
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for WebFetchTool {
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
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page and return its content as text. HTML pages are auto-converted to plain text."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch."
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'url' is required"))?;

        let result = timeout(Duration::from_secs(30), async {
            self.client.get(url).send().await
        })
        .await;

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
                    error: Some("request timed out after 30s".to_string()),
                });
            }
        };

        let status = response.status();
        if !status.is_success() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("HTTP {}", status)),
            });
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = response.text().await.unwrap_or_default();

        let output = if content_type.contains("html") {
            strip_html(&body)
        } else {
            body
        };

        // Truncate if too large.
        let max_len = 50_000;
        let output = if output.len() > max_len {
            format!("{}... (truncated at {} chars)", &output[..max_len], max_len)
        } else {
            output
        };

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}
