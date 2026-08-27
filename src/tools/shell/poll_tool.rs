#![allow(unused_imports)]
use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{mpsc, RwLock};
use tokio::time::Duration;
use super::*;

// ── ShellPollTool ────────────────────────────────────────────────────────

pub struct ShellPollTool {
    registry: ShellRegistry,
    sessions_dir: Option<PathBuf>,
}

impl ShellPollTool {
    pub fn new(registry: ShellRegistry, sessions_dir: Option<PathBuf>) -> Self {
        Self {
            registry,
            sessions_dir,
        }
    }
}

#[async_trait]
impl Tool for ShellPollTool {
    fn name(&self) -> &str {
        "shell_poll"
    }

    fn description(&self) -> &str {
        "Check on a shell process by process_id (from `shell`, foreground or background). \
         Instant peek only — returns accumulated output and machine-readable state \
         (running/exited/killed/lost_on_restart) and exit_code without waiting. To wait for \
         completion, call sessions_yield instead (you're woken automatically when it finishes; \
         never poll shell_poll in a loop). Set `remove: true` to clean up the process entry \
         after reading — if it's still running this also terminates it (equivalent to \
         shell_kill) before removing. `lost_on_restart` means the daemon restarted through a \
         path that does not preserve child processes; the output shown is everything captured \
         before that happened."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string", "description": "The process_id returned by `shell`." },
                "remove": { "type": "boolean", "description": "If true, remove the process entry after reading output (default: false). If the process is still running, this kills it first." }
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
        ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let process_id = args["process_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'process_id' is required"))?;
        let remove = args["remove"].as_bool().unwrap_or(false);

        let found = self.registry.read().await.get(process_id).cloned();
        let mut entry = match found {
            Some(e) => e,
            None => {
                // issue #129: an unknown process_id is most often a
                // transcription error (the id is 32 hex chars with no
                // structure to sanity-check) — list this session's live
                // process-table entries so the model can self-correct in
                // one turn instead of guessing again.
                let listing = format_unknown_process_listing(&*self.registry.read().await, &ctx.session_id);
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("process_id '{}' not found.{}", process_id, listing)),
                });
            }
        };

        if remove && entry.state == "running" {
            unsafe {
                libc::kill(entry.pid, libc::SIGTERM);
            }
            // Give it a brief grace period, then confirm via the reaper's
            // own state update rather than assuming the kill succeeded.
            let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::time::Instant::now() < kill_deadline {
                let cur = self.registry.read().await.get(process_id).cloned();
                match cur {
                    Some(e) if e.state != "running" => {
                        entry = e;
                        break;
                    }
                    Some(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                    None => break,
                }
            }
        }

        let (raw, tail_truncated) = read_lossy_tail(Path::new(&entry.output_path), MAX_TRACKED_OUTPUT);
        let (display, inline_truncated) = cap_output_for_display(&raw, Path::new(&entry.output_path));
        let output_truncated = tail_truncated || inline_truncated;

        let removed = remove && entry.state != "running";
        if removed {
            self.registry.write().await.remove(process_id);
            remove_entry_disk(self.sessions_dir.as_deref(), &entry);
        }

        let output = format!(
            "state={}\nprocess_id={}\ncommand={}\nexit_code={}\nremoved={}\n\noutput{}:\n{}",
            entry.state,
            process_id,
            entry.command,
            entry.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
            removed,
            if output_truncated { " (truncated)" } else { "" },
            display
        );

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

/// Test-only `ProcEntry` constructor, exposed `pub(crate)` (not gated to
/// this module's own `mod tests`) so other modules' tests — issue #140's
/// `orchestrator::recovery` coverage in particular — can populate a
/// `ShellRegistry` without needing every private field.
#[cfg(test)]
#[cfg(test)]
pub(crate) fn test_proc_entry(process_id: &str, session_id: &str, state: &str) -> ProcEntry {
    ProcEntry {
        process_id: process_id.to_string(),
        session_id: session_id.to_string(),
        command: "echo test".to_string(),
        workdir: None,
        pid: 999_999,
        pid_start_ticks: None,
        spawned_at_ms: now_ms(),
        output_path: "/tmp/does-not-matter".to_string(),
        state: state.to_string(),
        exit_code: None,
        notify_on_exit: true,
    }
}
