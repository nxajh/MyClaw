//! Shell execution tool.

use async_trait::async_trait;
use crate::providers::{Tool, ToolResult};
use serde_json::json;
use tokio::time::{Duration, timeout};

/// Execute shell commands.
#[derive(Default)]
pub struct ShellTool;

impl ShellTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout, stderr, and exit code."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 120)."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory (default: current)."
                }
            },
            "required": ["command"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'command' is required"))?;

        let timeout_secs = args["timeout_secs"]
            .as_u64()
            .unwrap_or(120);

        let workdir = args["workdir"].as_str();

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        let result = timeout(Duration::from_secs(timeout_secs), async {
            cmd.output().await
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut output_text = format!("exit code: {}\n{}", output.status.code().unwrap_or(-1), stdout.into_owned());
                if !stderr.is_empty() {
                    output_text.push_str(&format!("\nstderr:\n{}", stderr));
                }

                Ok(ToolResult {
                    success: output.status.success(),
                    output: output_text,
                    error: if output.status.success() { None } else { Some(format!("exit code {}", output.status.code().unwrap_or(-1))) },
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("failed to execute command: {}", e)),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("command timed out after {}s", timeout_secs)),
            }),
        }
    }
}
