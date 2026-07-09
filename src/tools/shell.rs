//! Shell execution tool — foreground (with timeout + partial output) and background mode.

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, timeout};

const MAX_OUTPUT_INLINE: usize = 30_000;

struct TruncatedOutput {
    text: String,
    full_output_path: Option<String>,
    truncated: bool,
    total_bytes: usize,
    total_lines: usize,
}

async fn truncate_large_output(output: &str) -> TruncatedOutput {
    let total_bytes = output.len();
    let total_lines = output.lines().count();
    if output.len() <= MAX_OUTPUT_INLINE {
        return TruncatedOutput {
            text: output.to_string(),
            full_output_path: None,
            truncated: false,
            total_bytes,
            total_lines,
        };
    }

    let cut = safe_char_boundary(output, MAX_OUTPUT_INLINE);
    let head_lines = output[..cut].lines().count();
    let remaining_lines = total_lines.saturating_sub(head_lines);
    let file_path = format!("/tmp/myclaw-shell-{}.txt", uuid::Uuid::new_v4().simple());
    match tokio::fs::write(&file_path, output).await {
        Ok(()) => TruncatedOutput {
            text: format!(
                "{}\n\n... [{} of {} lines truncated. full_output_path={}] ...\n... [Read with file_read offset/limit] ...",
                &output[..cut], remaining_lines, total_lines, file_path
            ),
            full_output_path: Some(file_path),
            truncated: true,
            total_bytes,
            total_lines,
        },
        Err(_) => TruncatedOutput {
            text: format!(
                "{}\n\n... [{} of {} lines truncated; failed to persist full output] ...",
                &output[..cut], remaining_lines, total_lines
            ),
            full_output_path: None,
            truncated: true,
            total_bytes,
            total_lines,
        },
    }
}

fn safe_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

pub struct BgProcEntry {
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    started_at: std::time::Instant,
    command: String,
    finished: bool,
    exit_code: Option<i32>,
}

pub type BgProcRegistry = Arc<RwLock<HashMap<String, BgProcEntry>>>;

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
         On timeout, partial output collected so far is returned. \
         Set `background: true` for fire-and-forget execution — returns a process_id \
         you can poll later with `shell_poll`. \
         Large output (>30K chars) is truncated: the first 30K is returned inline \
         and the full output is saved to a temp file with full_output_path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute." },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120, max 300). On timeout, partial output collected so far is returned." },
                "workdir": { "type": "string", "description": "Working directory (default: current)." },
                "background": { "type": "boolean", "description": "If true, run the command in the background and return a process_id immediately. Use shell_poll to check status and collect output." }
            },
            "required": ["command"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        3_000
    }

    fn preferred_timeout_secs(&self) -> Option<u64> {
        Some(300)
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
    async fn spawn_child(command: &str, workdir: Option<&str>) -> anyhow::Result<Child> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn command: {}", e))
    }

    async fn run_foreground(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: u64,
    ) -> anyhow::Result<ToolResult> {
        let mut child = match Self::spawn_child(command, workdir).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        spawn_reader(child.stdout.take(), stdout_buf.clone());
        spawn_reader(child.stderr.take(), stderr_buf.clone());

        let wait_result = timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        let (state, exit_code, success, error) = match wait_result {
            Ok(Ok(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                (
                    "exited",
                    Some(exit_code),
                    status.success(),
                    if status.success() {
                        None
                    } else {
                        Some(format!("exit code {}", exit_code))
                    },
                )
            }
            Ok(Err(e)) => ("wait_error", None, false, Some(format!("wait failed: {}", e))),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                (
                    "timeout",
                    None,
                    false,
                    Some(format!("command timed out after {}s", timeout_secs)),
                )
            }
        };

        tokio::time::sleep(Duration::from_millis(20)).await;
        let stdout = stdout_buf.lock().await.clone();
        let stderr = stderr_buf.lock().await.clone();
        let output_text = format_shell_output(state, exit_code, timeout_secs, &stdout, &stderr);
        let truncated = truncate_large_output(&output_text).await;
        let output = add_truncation_metadata(truncated);

        Ok(ToolResult {
            success,
            output,
            error,
        })
    }

    async fn run_background(
        &self,
        command: &str,
        workdir: Option<&str>,
    ) -> anyhow::Result<ToolResult> {
        let mut child = match Self::spawn_child(command, workdir).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let proc_id = format!("bg_{}", uuid::Uuid::new_v4().simple());
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        spawn_reader(child.stdout.take(), stdout_buf.clone());
        spawn_reader(child.stderr.take(), stderr_buf.clone());

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

        let bg_procs = self.bg_procs.clone();
        let proc_id_clone = proc_id.clone();
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);
            if let Some(entry) = bg_procs.write().await.get_mut(&proc_id_clone) {
                entry.finished = true;
                entry.exit_code = Some(exit_code);
            }
        });

        Ok(ToolResult {
            success: true,
            output: format!(
                "state=running\nprocess_id={}\ncommand={}\nuse shell_poll to check status and collect output",
                proc_id, command
            ),
            error: None,
        })
    }
}

fn spawn_reader<R>(reader: Option<R>, buf: Arc<Mutex<String>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    if let Some(mut reader) = reader {
        tokio::spawn(async move {
            let mut tmp = vec![0u8; 8192];
            loop {
                match reader.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => buf.lock().await.push_str(&String::from_utf8_lossy(&tmp[..n])),
                    Err(_) => break,
                }
            }
        });
    }
}

fn format_shell_output(
    state: &str,
    exit_code: Option<i32>,
    timeout_secs: u64,
    stdout: &str,
    stderr: &str,
) -> String {
    let mut output = format!(
        "state={}\nexit_code={}\ntimeout_secs={}\nstdout_bytes={}\nstderr_bytes={}\n",
        state,
        exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
        timeout_secs,
        stdout.len(),
        stderr.len()
    );
    if !stdout.is_empty() {
        output.push_str("\nstdout:\n");
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        output.push_str("\nstderr:\n");
        output.push_str(stderr);
    }
    output
}

fn add_truncation_metadata(truncated: TruncatedOutput) -> String {
    let mut output = format!(
        "truncated={}\ntotal_bytes={}\ntotal_lines={}\nfull_output_path={}\n",
        truncated.truncated,
        truncated.total_bytes,
        truncated.total_lines,
        truncated.full_output_path.as_deref().unwrap_or("null")
    );
    output.push_str(&truncated.text);
    output
}

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
        "Poll a background shell process started with `background: true`. Returns accumulated stdout/stderr and machine-readable state/exit_code/elapsed_secs. Set `wait_secs` to wait for completion before returning. Set `remove: true` to clean up the process entry after reading; running processes are never removed. Large output is truncated with full_output_path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string", "description": "The process_id returned by `shell` when background=true." },
                "remove": { "type": "boolean", "description": "If true, remove the process entry after reading output (default: false). Ignored while the process is still running." },
                "wait_secs": { "type": "integer", "description": "Optional seconds to wait for the process to finish before returning (default 0, max 300)." }
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
        let wait_secs = args["wait_secs"].as_u64().unwrap_or(0).min(300);

        let start_wait = std::time::Instant::now();
        loop {
            let finished = {
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
                entry.finished
            };
            if finished || wait_secs == 0 || start_wait.elapsed() >= Duration::from_secs(wait_secs) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

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

        let removed = remove && finished;
        if removed {
            self.bg_procs.write().await.remove(proc_id);
        }

        let state = if finished { "exited" } else { "running" };
        let mut output = format!(
            "state={}\nprocess_id={}\ncommand={}\nexit_code={}\nelapsed_secs={}\nstdout_bytes={}\nstderr_bytes={}\nremoved={}\n",
            state,
            proc_id,
            command,
            exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
            elapsed.as_secs(),
            stdout.len(),
            stderr.len(),
            removed
        );
        if remove && !finished {
            output.push_str("note=remove_ignored_process_running\n");
        }
        if !stdout.is_empty() {
            output.push_str("\nstdout:\n");
            output.push_str(&stdout);
        }
        if !stderr.is_empty() {
            output.push_str("\nstderr:\n");
            output.push_str(&stderr);
        }

        let truncated = truncate_large_output(&output).await;
        Ok(ToolResult {
            success: true,
            output: add_truncation_metadata(truncated),
            error: None,
        })
    }
}
