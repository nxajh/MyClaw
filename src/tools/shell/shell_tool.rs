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

// ── ShellTool ────────────────────────────────────────────────────────────

pub struct ShellTool {
    registry: ShellRegistry,
    sessions_dir: Option<PathBuf>,
    /// issue #129: where `background: true` completions are reported.
    /// `None` for bare CLI usage / tests — no orchestrator to wake.
    notice_tx: Option<mpsc::Sender<ShellCompletion>>,
    /// issue #140: session lookup for registering a notify-armed process as
    /// pending async work (`SessionContext::add_pending_task`), the same
    /// mechanism `agent_delegate(mode="async")` uses. Set once, after
    /// construction — `ShellTool` is built before `SessionManager` exists in
    /// `daemon.rs`'s composition order (mirrors `set_runtime`/`set_messenger`
    /// elsewhere for the same reason). `None` for bare CLI usage / tests —
    /// nothing to register against.
    session_manager:
        std::sync::OnceLock<Arc<dyn crate::api::session_store::SessionStore>>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ShellTool {
    pub fn new(sessions_dir: Option<PathBuf>) -> Self {
        Self {
            registry: Arc::new(RwLock::new(HashMap::new())),
            sessions_dir,
            notice_tx: None,
            session_manager: std::sync::OnceLock::new(),
        }
    }

    pub fn new_with_notice_sender(
        sessions_dir: Option<PathBuf>,
        notice_tx: mpsc::Sender<ShellCompletion>,
    ) -> Self {
        Self {
            registry: Arc::new(RwLock::new(HashMap::new())),
            sessions_dir,
            notice_tx: Some(notice_tx),
            session_manager: std::sync::OnceLock::new(),
        }
    }

    pub fn registry(&self) -> ShellRegistry {
        self.registry.clone()
    }

    pub fn sessions_dir(&self) -> Option<PathBuf> {
        self.sessions_dir.clone()
    }

    /// issue #140: wire the `SessionManager` in after construction (daemon.rs
    /// builds `ShellTool` before `SessionManager` exists). Idempotent no-op
    /// if already set.
    pub fn set_session_manager<S: crate::api::session_store::SessionStore + 'static>(
        &self,
        sm: Arc<S>,
    ) {
        let _ = self.session_manager.set(sm);
    }

    /// issue #140: register `process_id` as pending async work on
    /// `session_id`'s suspension — the shell-side half of the unification
    /// with delegation (`SessionContext::add_pending_task` is the exact same
    /// method `agent_delegate(mode="async")` calls). Called at every point a
    /// process becomes armed for a completion notice: `background: true` at
    /// spawn time, and the timeout-conversion branch in `execute()`. A
    /// no-op when no `SessionManager` is wired (bare CLI / tests) or the
    /// session has no live registered context (shouldn't happen — this only
    /// runs from inside that session's own tool call — but tool execution
    /// must never panic on a missing lookup).
    fn register_pending(&self, session_id: &str, process_id: &str) {
        let Some(sm) = self.session_manager.get() else {
            return;
        };
        if let Some(sctx) = sm.registered_context_by_session_id(session_id) {
            sctx.add_pending_task(process_id.to_string());
        }
    }

    /// Spawn `command`, register it, and start the reaper that watches it to
    /// completion. Never kills anything — that's `shell_kill`'s job.
    ///
    /// `background` (issue #129) gates the completion notice: only an
    /// explicit `background: true` call arms one, never the plain
    /// `timeout_secs` overrun path (same reaper, different call site).
    async fn spawn_tracked(
        &self,
        command: &str,
        workdir: Option<&str>,
        session_id: &str,
        background: bool,
    ) -> anyhow::Result<String> {
        let process_id = format!("sh_{}", uuid::Uuid::new_v4().simple());

        let output_path = match &self.sessions_dir {
            Some(dir) => {
                let pdir = entry_dir(dir, session_id);
                std::fs::create_dir_all(&pdir)?;
                entry_output_path(&pdir, &process_id)
            }
            // Bare CLI usage without a session dir — nothing to reattach
            // across a restart anyway, but still file-backed, never a pipe.
            None => std::env::temp_dir().join(format!("myclaw-shell-{process_id}.out")),
        };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).stdin(Stdio::null());
        // Issue #84: the daemon (systemd user unit, no login shell in its
        // ancestry) inherits a PATH that's missing user-installed CLIs
        // (npm global, nvm, pyenv, Homebrew, …). Layer in a fix-up before
        // spawning — see `tools::shell_env` for the three-layer strategy.
        crate::tools::shell_env::apply(&mut cmd);
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        let out_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output_path)?;
        let err_file = out_file.try_clone()?;
        cmd.stdout(Stdio::from(out_file)).stderr(Stdio::from(err_file));

        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn command: {}", e))?;
        let pid = child.id().ok_or_else(|| anyhow::anyhow!("spawned process has no pid"))? as i32;

        let entry = ProcEntry {
            process_id: process_id.clone(),
            session_id: session_id.to_string(),
            command: command.to_string(),
            workdir: workdir.map(|s| s.to_string()),
            pid,
            pid_start_ticks: pid_start_ticks(pid),
            spawned_at_ms: now_ms(),
            output_path: output_path.to_string_lossy().to_string(),
            state: "running".to_string(),
            exit_code: None,
            notify_on_exit: background,
        };
        write_entry_disk(self.sessions_dir.as_deref(), &entry);
        self.registry.write().await.insert(process_id.clone(), entry);
        if background {
            // issue #140: armed for notify from the start — register now,
            // same moment `notify_on_exit` was set on the entry above.
            self.register_pending(session_id, &process_id);
        }

        spawn_owned_reaper(
            self.registry.clone(),
            self.sessions_dir.clone(),
            process_id.clone(),
            session_id.to_string(),
            output_path,
            self.notice_tx.clone(),
            child,
        );

        Ok(process_id)
    }

    async fn snapshot(&self, process_id: &str) -> Option<ProcEntry> {
        self.registry.read().await.get(process_id).cloned()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout, stderr, and exit code. \
         Multi-line commands (heredocs, if/for/while/case blocks) run exactly \
         as they would in a real shell — the command is never split or rewritten. \
         On timeout the command is NOT killed: it keeps running, and you get back \
         a process_id plus whatever output has been produced so far — you'll be \
         notified automatically when it finishes; call sessions_yield to suspend \
         and wait for that (never poll in a loop), or shell_kill to terminate it. \
         Set `background: true` to skip waiting entirely and get the process_id \
         immediately, same wake-on-completion semantics. shell_poll still works \
         for an instant peek if you want to check sooner. Large output \
         (>30K chars) is truncated: the first 30K is returned inline and the full \
         output is available via full_output_path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute." },
                "timeout_secs": { "type": "integer", "description": "Seconds to wait for completion before returning (default 120, max 300). The command keeps running past this — it is not killed on timeout." },
                "workdir": { "type": "string", "description": "Working directory (default: current)." },
                "background": { "type": "boolean", "description": "If true, return a process_id immediately instead of waiting. You'll be notified automatically when it finishes; shell_poll still works if you want to check sooner." }
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
        ctx: &crate::api::tool::ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'command' is required"))?;
        let background = args["background"].as_bool().unwrap_or(false);
        let workdir = args["workdir"].as_str();
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120).min(300);

        let process_id = match self
            .spawn_tracked(command, workdir, &ctx.session_id, background)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        if background {
            return Ok(ToolResult {
                success: true,
                output: format!(
                    "state=running\nprocess_id={}\ncommand={}\nstarted in background — call sessions_yield to suspend and be woken automatically when it finishes (zero cost, no polling); shell_poll(process_id) still works for an instant peek if you want to check sooner.\n",
                    process_id, command
                ),
                error: None,
            });
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        let entry = loop {
            let entry = self.snapshot(&process_id).await;
            match &entry {
                Some(e) if e.state != "running" => break entry.unwrap(),
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                break entry.expect("just-inserted entry must exist");
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        };

        let (raw, tail_truncated) = read_lossy_tail(Path::new(&entry.output_path), MAX_TRACKED_OUTPUT);
        let (display, inline_truncated) =
            cap_output_for_display(&crate::str_utils::neutralize_spoofing(&raw), Path::new(&entry.output_path));
        let output_truncated = tail_truncated || inline_truncated;

        if entry.state == "running" {
            // Timed out, still going — NOT killed. issue #131 decision 2:
            // this call already reported "still running" with no terminal
            // result, so from here on it's treated the same as an explicit
            // `background: true` spawn — armed for a completion notice.
            //
            // Narrow race (xiaoer-bot review, #139): the process can exit
            // between the deadline snapshot above and this flip acquiring
            // the write lock — if the reaper wins, it already recorded a
            // terminal state with `notify_on_exit` still false, and the flip
            // below correctly no-ops (nothing "running" left to arm). No
            // notice will ever fire for it then, so detect that case via the
            // returned live state and report the terminal result inline
            // instead of promising a notify that would never come.
            if let Some(live) = mark_notify_on_exit(&self.registry, self.sessions_dir.as_deref(), &process_id).await {
                if live.state != "running" {
                    return Ok(terminal_result(&live, &process_id, &display, output_truncated));
                }
            }
            // issue #140: successfully armed (still running) — register now,
            // same moment as the `background: true` spawn-time case above.
            self.register_pending(&ctx.session_id, &process_id);
            let output = format!(
                "state=running\nprocess_id={}\ncommand={}\ntimeout_secs={}\nnote=command is still running, it was NOT killed; you'll be notified automatically when it finishes — call sessions_yield to suspend and wait for that, or shell_kill(process_id) to terminate it\n\noutput_so_far{}:\n{}",
                process_id,
                command,
                timeout_secs,
                if output_truncated { " (truncated)" } else { "" },
                display
            );
            return Ok(ToolResult {
                success: true,
                output,
                error: None,
            });
        }

        Ok(terminal_result(&entry, &process_id, &display, output_truncated))
    }
}

/// issue #129: build the "here's what's actually alive" listing appended to
/// a `shell_poll`/`shell_kill` unknown-`process_id` error. Scoped to the
/// calling session only (other sessions' process ids aren't this model's
/// business), newest-first, capped so a long-running session with many
/// tracked processes doesn't blow up the error message.
pub(crate) const UNKNOWN_PROCESS_LISTING_CAP: usize = 20;
const UNKNOWN_PROCESS_COMMAND_PREVIEW_CHARS: usize = 60;

pub(crate) fn format_unknown_process_listing(registry: &HashMap<String, ProcEntry>, session_id: &str) -> String {
    let mut entries: Vec<&ProcEntry> = registry
        .values()
        .filter(|e| e.session_id == session_id)
        .collect();
    if entries.is_empty() {
        return " No tracked processes for this session — the id may be from a prior \
                 session, or never existed."
            .to_string();
    }
    entries.sort_by(|a, b| b.spawned_at_ms.cmp(&a.spawned_at_ms));
    let total = entries.len();
    let shown: Vec<String> = entries
        .into_iter()
        .take(UNKNOWN_PROCESS_LISTING_CAP)
        .map(|e| {
            let cut = safe_char_boundary(&e.command, UNKNOWN_PROCESS_COMMAND_PREVIEW_CHARS);
            let preview = &e.command[..cut];
            let ellipsis = if cut < e.command.len() { "..." } else { "" };
            format!(
                "  {} state={} command={:?}{}",
                e.process_id, e.state, preview, ellipsis
            )
        })
        .collect();
    let omitted = total.saturating_sub(shown.len());
    let omitted_note = if omitted > 0 {
        format!("\n  ... and {omitted} more")
    } else {
        String::new()
    };
    format!(
        " This session's tracked processes:\n{}{}",
        shown.join("\n"),
        omitted_note
    )
}
