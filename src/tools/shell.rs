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

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();

        let stdout_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        let stdout_task = {
            let buf = stdout_buf.clone();
            tokio::spawn(async move {
                if let Some(ref mut s) = stdout {
                    let mut tmp = Vec::with_capacity(8192);
                    loop {
                        tmp.clear();
                        match s.read(&mut tmp).await {
                            Ok(0) => break,
                            Ok(_) => buf.lock().await.push_str(&String::from_utf8_lossy(&tmp)),
                            Err(_) => break,
                        }
                    }
                }
            })
        };

        let stderr_task = {
            let buf = stderr_buf.clone();
            tokio::spawn(async move {
                if let Some(ref mut s) = stderr {
                    let mut tmp = Vec::with_capacity(8192);
                    loop {
                        tmp.clear();
                        match s.read(&mut tmp).await {
                            Ok(0) => break,
                            Ok(_) => buf.lock().await.push_str(&String::from_utf8_lossy(&tmp)),
                            Err(_) => break,
                        }
                    }
                }
            })
        };

        match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
            Ok(Ok(status)) => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;

                let stdout_text = stdout_buf.lock().await.clone();
                let stderr_text = stderr_buf.lock().await.clone();

                let mut output_text = format!(
                    "exit code: {}\n{}",
                    status.code().unwrap_or(-1),
                    stdout_text
                );
                if !stderr_text.is_empty() {
                    output_text.push_str(&format!("\nstderr:\n{}", stderr_text));
                }

                Ok(ToolResult {
                    success: status.success(),
                    output: output_text,
                    error: if status.success() {
                        None
                    } else {
                        Some(format!("exit code {}", status.code().unwrap_or(-1)))
                    },
                })
            }
            Ok(Err(e)) => {
                child.start_kill().ok();
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to execute command: {}", e)),
                })
            }
            Err(_) => {
                // Timeout — the child.wait() future was dropped, but `child`
                // itself is still alive. kill_on_drop won't trigger until the
                // function returns. We need to explicitly kill the child so
                // the pipes close and read tasks can drain remaining output.
                child.start_kill().ok();

                // Wait for read tasks to observe EOF and collect remaining bytes.
                let _ = timeout(Duration::from_secs(3), async {
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                })
                .await;

                let stdout_text = stdout_buf.lock().await.clone();
                let stderr_text = stderr_buf.lock().await.clone();

                let mut output_text = format!(
                    "command timed out after {}s (partial output below)\n{}",
                    timeout_secs, stdout_text
                );
                if !stderr_text.is_empty() {
                    output_text.push_str(&format!("\nstderr:\n{}", stderr_text));
                }

                Ok(ToolResult {
                    success: false,
                    output: output_text,
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
            let mut stdout_tmp = Vec::with_capacity(8192);
            let mut stderr_tmp = Vec::with_capacity(8192);

            loop {
                tokio::select! {
                    r = async {
                        if let Some(ref mut s) = stdout {
                            stdout_tmp.clear();
                            s.read(&mut stdout_tmp).await
                        } else {
                            std::future::pending::<std::io::Result<usize>>().await
                        }
                    } => {
                        match r {
                            Ok(0) | Err(_) => stdout = None,
                            Ok(_) => stdout_buf_clone.lock().await.push_str(&String::from_utf8_lossy(&stdout_tmp)),
                        }
                    }
                    r = async {
                        if let Some(ref mut s) = stderr {
                            stderr_tmp.clear();
                            s.read(&mut stderr_tmp).await
                        } else {
                            std::future::pending::<std::io::Result<usize>>().await
                        }
                    } => {
                        match r {
                            Ok(0) | Err(_) => stderr = None,
                            Ok(_) => stderr_buf_clone.lock().await.push_str(&String::from_utf8_lossy(&stderr_tmp)),
                        }
                    }
                }
                if stdout.is_none() && stderr.is_none() {
                    break;
                }
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
