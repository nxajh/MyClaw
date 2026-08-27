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

// ── ShellKillTool ────────────────────────────────────────────────────────

pub struct ShellKillTool {
    registry: ShellRegistry,
}

impl ShellKillTool {
    pub fn new(registry: ShellRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ShellKillTool {
    fn name(&self) -> &str {
        "shell_kill"
    }

    fn description(&self) -> &str {
        "Terminate a running shell process by process_id (from `shell`, foreground or \
         background). Default signal is TERM (graceful); pass signal: \"KILL\" to force it. \
         Does nothing (and reports so) if the process has already finished."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string", "description": "The process_id to terminate." },
                "signal": { "type": "string", "enum": ["TERM", "KILL"], "description": "Signal to send (default TERM)." }
            },
            "required": ["process_id"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let process_id = args["process_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'process_id' is required"))?;
        let signal = args["signal"].as_str().unwrap_or("TERM");
        let sig = match signal {
            "KILL" => libc::SIGKILL,
            _ => libc::SIGTERM,
        };

        let found = self.registry.read().await.get(process_id).cloned();
        let entry = match found {
            Some(e) => e,
            None => {
                // issue #129: same self-correction aid as shell_poll — a
                // wrong process_id here is the same transcription-error
                // shape, just aimed at killing instead of checking.
                let listing = format_unknown_process_listing(&*self.registry.read().await, &ctx.session_id);
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("process_id '{}' not found.{}", process_id, listing)),
                });
            }
        };

        if entry.state != "running" {
            return Ok(ToolResult {
                success: true,
                output: format!(
                    "state={}\nprocess_id={}\nnote=process already not running, nothing to kill\n",
                    entry.state, process_id
                ),
                error: None,
            });
        }

        let rc = unsafe { libc::kill(entry.pid, sig) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("kill({}, {}) failed: {}", entry.pid, signal, err)),
            });
        }

        Ok(ToolResult {
            success: true,
            output: format!(
                "signal={}\nsent_to_pid={}\nprocess_id={}\nnote=signal sent; the process table updates once it actually exits, poll with shell_poll to confirm\n",
                signal, entry.pid, process_id
            ),
            error: None,
        })
    }
}
