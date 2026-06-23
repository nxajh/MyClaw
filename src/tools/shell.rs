//! Shell execution tool — foreground (with timeout + partial output) and background mode.

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, timeout};

/// Entry for a tracked background process.
pub struct BgProcEntry {
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    started_at: std::time::Instant,
    command: String,
    finished: bool,
    exit_code: Option<i32>,
}

/// Shared registry for background processes, keyed by process id.
pub type BgProcRegistry = Arc<RwLock<HashMap<String, BgProcEntry>>>;

/// Execute shell commands (foreground with timeout/partial-output, or background fire-and-forget).
pub struct ShellTool {
    bg_procs: BgProcRegistry,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    pub fn new() -> Self {
        Self {
            bg_procs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Expose the background-process registry so a `ShellPollTool` can share it.
    pub fn bg_registry(&self) -> BgProcRegistry {
        self.bg_procs.clone()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout, stderr, and exit code. \
         On timeout, partial output collected so far is returned (not discarded). \
         Set `background: true` for fire-and-forget execution — returns a process_id \
         you can poll later with `shell_poll`."
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
                    "description": "Timeout in seconds (default 120). On timeout, partial output collected so far is returned."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory (default: current)."
                },
                "background": {
                    "type": "boolean",
                    "description": "If true, run the command in the background and return a process_id immediately. Use shell_poll to check status and collect output."
                }
            },
            "required": ["command"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        3_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'command' is required"))?;

        let background = args["background"].as_bool().unwrap_or(false);
        let workdir = args["workdir"].as_str();

        if background {
            return self.run_background(command, workdir).await;
        }

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120).min(300);
        self.run_foreground(command, workdir, timeout_secs).await
    }
}

impl ShellTool {
    async fn run_foreground(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: u64,
    ) -> anyhow::Result<ToolResult> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to spawn command: {}", e)),
                });
            }
        };

        // Take pipe handles before moving child into the collect task.
        let mut child_stdout = child.stdout.take();
        let mut child_stderr = child.stderr.take();

        // Single task owns the child, reads pipes, and waits for exit.
        // This avoids any race between child.wait() and pipe reads.
        let collect_task = tokio::spawn(async move {
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();

            // Read from pipes concurrently using tokio::select!.
            let mut stdout_done = child_stdout.is_none();
            let mut stderr_done = child_stderr.is_none();

            loop {
                if stdout_done && stderr_done {
                    break;
                }
                tokio::select! {
                    result = async {
                        if let Some(ref mut out) = child_stdout {
                            let mut tmp = vec![0u8; 8192];
                            out.read(&mut tmp).await.map(|n| (n, tmp))
                        } else {
                            std::future::pending().await
                        }
                    }, if !stdout_done => {
                        match result {
                            Ok((0, _)) => stdout_done = true,
                            Ok((n, tmp)) => stdout_buf.extend_from_slice(&tmp[..n]),
                            Err(_) => stdout_done = true,
                        }
                    }
                    result = async {
                        if let Some(ref mut err) = child_stderr {
                            let mut tmp = vec![0u8; 8192];
                            err.read(&mut tmp).await.map(|n| (n, tmp))
                        } else {
                            std::future::pending().await
                        }
                    }, if !stderr_done => {
                        match result {
                            Ok((0, _)) => stderr_done = true,
                            Ok((n, tmp)) => stderr_buf.extend_from_slice(&tmp[..n]),
                            Err(_) => stderr_done = true,
                        }
                    }
                }
            }

            // Wait for child to exit.
            let status = child.wait().await;

            let stdout_text = String::from_utf8_lossy(&stdout_buf).into_owned();
            let stderr_text = String::from_utf8_lossy(&stderr_buf).into_owned();

            (status, stdout_text, stderr_text)
        });

        match timeout(Duration::from_secs(timeout_secs), collect_task).await {
            Ok(Ok((status, stdout_text, stderr_text))) => {
                let exit_code = status.as_ref().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                let success = status.as_ref().map(|s| s.success()).unwrap_or(false);

                let mut output_text = format!("exit code: {}\n{}", exit_code, stdout_text);
                if !stderr_text.is_empty() {
                    output_text.push_str(&format!("\nstderr:\n{}", stderr_text));
                }

                Ok(ToolResult {
                    success,
                    output: output_text,
                    error: if success {
                        None
                    } else {
                        Some(format!("exit code {}", exit_code))
                    },
                })
            }
            Ok(Err(join_err)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("internal error: {}", join_err)),
            }),
            Err(_) => {
                // Timeout — the collect_task is abandoned. The child will be
                // orphaned but the pipes will close when the task is dropped.
                Ok(ToolResult {
                    success: false,
                    output: "command timed out".to_string(),
                    error: Some(format!("command timed out after {}s", timeout_secs)),
                })
            }
        }
    }

    async fn run_background(
        &self,
        command: &str,
        workdir: Option<&str>,
    ) -> anyhow::Result<ToolResult> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to spawn background command: {}", e)),
                });
            }
        };

        let proc_id = format!("bg_{}", uuid::Uuid::new_v4().simple());
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_buf_clone = stdout_buf.clone();
        let stderr_buf_clone = stderr_buf.clone();
        let bg_procs = self.bg_procs.clone();
        let proc_id_clone = proc_id.clone();

        tokio::spawn(async move {
            let mut stdout = stdout;
            let mut stderr = stderr;
            let mut stdout_buf_local = Vec::new();
            let mut stderr_buf_local = Vec::new();

            // Read pipes concurrently using select! with read_to_end.
            let mut stdout_done = stdout.is_none();
            let mut stderr_done = stderr.is_none();

            loop {
                if stdout_done && stderr_done {
                    break;
                }
                tokio::select! {
                    result = async {
                        if let Some(ref mut s) = stdout {
                            let mut tmp = vec![0u8; 8192];
                            s.read(&mut tmp).await.map(|n| (n, tmp))
                        } else {
                            std::future::pending().await
                        }
                    }, if !stdout_done => {
                        match result {
                            Ok((0, _)) => stdout_done = true,
                            Ok((n, tmp)) => stdout_buf_local.extend_from_slice(&tmp[..n]),
                            Err(_) => stdout_done = true,
                        }
                    }
                    result = async {
                        if let Some(ref mut s) = stderr {
                            let mut tmp = vec![0u8; 8192];
                            s.read(&mut tmp).await.map(|n| (n, tmp))
                        } else {
                            std::future::pending().await
                        }
                    }, if !stderr_done => {
                        match result {
                            Ok((0, _)) => stderr_done = true,
                            Ok((n, tmp)) => stderr_buf_local.extend_from_slice(&tmp[..n]),
                            Err(_) => stderr_done = true,
                        }
                    }
                }
            }

            // Copy collected output to shared buffers.
            if !stdout_buf_local.is_empty() {
                stdout_buf_clone.lock().await.push_str(&String::from_utf8_lossy(&stdout_buf_local));
            }
            if !stderr_buf_local.is_empty() {
                stderr_buf_clone.lock().await.push_str(&String::from_utf8_lossy(&stderr_buf_local));
            }

            let status = child.wait().await.ok();
            let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);

            if let Some(entry) = bg_procs.write().await.get_mut(&proc_id_clone) {
                entry.finished = true;
                entry.exit_code = Some(exit_code);
            }
        });

        self.bg_procs.write().await.insert(
            proc_id.clone(),
            BgProcEntry {
                stdout: stdout_buf,
                stderr: stderr_buf,
                started_at: std::time::Instant::now(),
                command: command.to_string(),
                finished: false,
                exit_code: None,
            },
        );

        Ok(ToolResult {
            success: true,
            output: format!(
                "background process started\nprocess_id: {}\ncommand: {}\nuse shell_poll to check status and collect output",
                proc_id, command
            ),
            error: None,
        })
    }
}

// ── ShellPollTool ───────────────────────────────────────────────────────────

/// Poll a background shell process — check status and collect accumulated output.
pub struct ShellPollTool {
    bg_procs: BgProcRegistry,
}

impl ShellPollTool {
    pub fn new(bg_procs: BgProcRegistry) -> Self {
        Self { bg_procs }
    }
}

#[async_trait]
impl Tool for ShellPollTool {
    fn name(&self) -> &str {
        "shell_poll"
    }

    fn description(&self) -> &str {
        "Poll a background shell process started with `background: true`. Returns accumulated stdout/stderr and process status. Set `remove: true` to clean up the process entry after reading."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "The process_id returned by `shell` when background=true."
                },
                "remove": {
                    "type": "boolean",
                    "description": "If true, remove the process entry after reading output (default: false)."
                }
            },
            "required": ["process_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        10_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let proc_id = args["process_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'process_id' is required"))?;
        let remove = args["remove"].as_bool().unwrap_or(false);

        let procs = self.bg_procs.read().await;
        let entry = match procs.get(proc_id) {
            Some(e) => e,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("process_id '{}' not found", proc_id)),
                });
            }
        };

        let stdout = entry.stdout.lock().await.clone();
        let stderr = entry.stderr.lock().await.clone();
        let elapsed = entry.started_at.elapsed();
        let finished = entry.finished;
        let exit_code = entry.exit_code;
        let command = entry.command.clone();
        drop(procs);

        if remove {
            self.bg_procs.write().await.remove(proc_id);
        }

        let status_str = if finished {
            format!("finished (exit code: {})", exit_code.unwrap_or(-1))
        } else {
            format!("still running ({}s elapsed)", elapsed.as_secs())
        };

        let mut output = format!(
            "process_id: {}\ncommand: {}\nstatus: {}\n",
            proc_id, command, status_str
        );
        if !stdout.is_empty() {
            output.push_str(&format!("\nstdout:\n{}", stdout));
        }
        if !stderr.is_empty() {
            output.push_str(&format!("\nstderr:\n{}", stderr));
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}
