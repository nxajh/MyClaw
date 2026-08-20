//! Shell execution tool.
//!
//! Every command is a **tracked process**: spawned once, its stdout/stderr
//! mirrored to a file (never an in-memory pipe — see below), and recorded in
//! a per-session, on-disk process table (`{sessions_dir}/{session}/.shell_procs/`).
//! `background: false` (the default) just means "wait up to `timeout_secs`
//! for it, then hand back a `process_id` if it's still going" — hitting the
//! timeout never kills the process. Use `shell_kill` to actually terminate
//! one; `shell_poll` checks on it later.
//!
//! Commands are never split, rewritten, or checkpointed — the raw string
//! goes to `sh -c` unmodified, so heredocs and multi-line if/for/while/case
//! blocks work exactly as they would in a real shell.
//!
//! Output is written to a file rather than piped into this process's memory
//! specifically so that `myclaw restart` (SIGUSR1 hot switch, see
//! `daemon.rs` / `hot_switch.rs`) doesn't SIGPIPE the child the instant the
//! old process's pipe read-end closes — a file descriptor doesn't care
//! whether anything is reading it. On hot switch the new process re-adopts
//! every process-table entry still marked `running` (`adopt_after_restart`
//! with `hot_switch: true`) by polling `/proc/{pid}` (it isn't the real
//! parent of these reparented orphans, so it can't `wait()` on them). On any
//! other restart path (`myclaw stop`, `systemctl`, a crash) this
//! deployment's systemd default `KillMode=control-group` kills the whole
//! cgroup including shell children, so those entries are marked
//! `lost_on_restart` instead of being probed for liveness — see
//! `adopt_after_restart` with `hot_switch: false`.

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::Duration;

const MAX_OUTPUT_INLINE: usize = 30_000;

/// Cap on how much of a process's on-disk output file is read back into a
/// tool result. The file itself is never truncated on write — only what's
/// read back and shown inline is capped, with `full_output_path` pointing at
/// the (untruncated) file for `file_read` to page through.
const MAX_TRACKED_OUTPUT: usize = 64 * 1024 * 1024; // 64 MiB

/// How long entries stay in `.shell_procs/` after reaching a terminal state
/// before a startup scan sweeps them, so the directory doesn't grow forever.
const ENTRY_RETENTION_SECS: i64 = 7 * 24 * 3600;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

/// Read a file as lossy UTF-8. Output files can accumulate bytes from
/// commands that truncate mid-multibyte-character (e.g. `cut -c` on CJK),
/// which makes the whole file invalid UTF-8 — `read_to_string` would fail
/// outright and the tool would report no output at all.
fn read_lossy(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(_) => std::fs::read(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .ok(),
    }
}

/// Read up to the last `max_bytes` of a file as lossy UTF-8 (tail, not
/// head — for a long-running or finished process the most recent output
/// matters most once a file is too big to read whole). Never loads more
/// than `max_bytes` into memory regardless of how large the file on disk
/// has grown. Returns `(content, was_truncated)`.
fn read_lossy_tail(path: &Path, max_bytes: usize) -> (String, bool) {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as usize;
    if len <= max_bytes {
        return (read_lossy(path).unwrap_or_default(), false);
    }
    use std::io::{Read, Seek, SeekFrom};
    let read_tail = std::fs::File::open(path).and_then(|mut f| {
        f.seek(SeekFrom::Start((len - max_bytes) as u64))?;
        let mut buf = vec![0u8; max_bytes];
        f.read_exact(&mut buf)?;
        Ok(buf)
    });
    match read_tail {
        Ok(buf) => (String::from_utf8_lossy(&buf).into_owned(), true),
        Err(_) => (read_lossy(path).unwrap_or_default(), false),
    }
}

/// Cap `raw` (already read from `output_path`, possibly already tail-capped
/// by `read_lossy_tail`) for inline display. The file on disk is untouched —
/// `full_output_path` just points back at it.
fn cap_output_for_display(raw: &str, output_path: &Path) -> (String, bool) {
    if raw.len() <= MAX_OUTPUT_INLINE {
        return (raw.to_string(), false);
    }
    let cut = safe_char_boundary(raw, MAX_OUTPUT_INLINE);
    (
        format!(
            "{}\n\n... [{} of {} bytes shown truncated. full_output_path={}] ...\n... [Read with file_read offset/limit] ...",
            &raw[..cut],
            raw.len() - cut,
            raw.len(),
            output_path.display()
        ),
        true,
    )
}

// ── Process table ────────────────────────────────────────────────────────

/// One tracked shell invocation. Persisted as JSON at
/// `{sessions_dir}/{session}/.shell_procs/{process_id}.json` whenever
/// `sessions_dir` is configured (bare CLI usage without a session dir keeps
/// entries in-memory only — there's no daemon to restart in that mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcEntry {
    process_id: String,
    session_id: String,
    command: String,
    workdir: Option<String>,
    pid: i32,
    /// `/proc/{pid}/stat` field 22 (starttime, in clock ticks since boot) at
    /// spawn time — cheap sanity check against PID reuse when re-verifying
    /// liveness after a hot switch.
    pid_start_ticks: Option<u64>,
    spawned_at_ms: i64,
    output_path: String,
    /// "running" | "exited" | "killed" | "lost_on_restart"
    state: String,
    exit_code: Option<i32>,
}

pub type ShellRegistry = Arc<RwLock<HashMap<String, ProcEntry>>>;

fn entry_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir
        .join(crate::ids::bare_dir_name(session_id))
        .join(".shell_procs")
}

fn entry_json_path(dir: &Path, process_id: &str) -> PathBuf {
    dir.join(format!("{process_id}.json"))
}

fn entry_output_path(dir: &Path, process_id: &str) -> PathBuf {
    dir.join(format!("{process_id}.out"))
}

fn write_entry_disk(sessions_dir: Option<&Path>, entry: &ProcEntry) {
    let Some(dir) = sessions_dir else { return };
    let pdir = entry_dir(dir, &entry.session_id);
    if std::fs::create_dir_all(&pdir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(entry) {
        let _ = std::fs::write(entry_json_path(&pdir, &entry.process_id), json);
    }
}

fn remove_entry_disk(sessions_dir: Option<&Path>, entry: &ProcEntry) {
    let Some(dir) = sessions_dir else { return };
    let pdir = entry_dir(dir, &entry.session_id);
    let _ = std::fs::remove_file(entry_json_path(&pdir, &entry.process_id));
    let _ = std::fs::remove_file(&entry.output_path);
}

/// Parse `/proc/{pid}/stat` field 22 (starttime). The comm field (2nd,
/// parenthesized) can itself contain spaces/parens, so we split on the last
/// `)` rather than whitespace.
#[cfg(target_os = "linux")]
fn pid_start_ticks(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn pid_start_ticks(_pid: i32) -> Option<u64> {
    None
}

/// Whether `pid` is still alive and (when we have a recorded start time)
/// plausibly the same process — not a reused PID.
fn pid_still_this_process(pid: i32, expected_start_ticks: Option<u64>) -> bool {
    if !crate::hot_switch::pid_alive(pid) {
        return false;
    }
    match expected_start_ticks {
        Some(expected) => pid_start_ticks(pid).is_none_or(|actual| actual == expected),
        None => true,
    }
}

async fn set_state(
    registry: &ShellRegistry,
    sessions_dir: Option<&Path>,
    process_id: &str,
    state: &str,
    exit_code: Option<i32>,
) {
    let mut map = registry.write().await;
    if let Some(entry) = map.get_mut(process_id) {
        entry.state = state.to_string();
        entry.exit_code = exit_code;
        write_entry_disk(sessions_dir, entry);
    }
}

/// Watch a process this daemon instance actually spawned (owns as a real
/// child) to completion, updating the table when it exits.
fn spawn_owned_reaper(
    registry: ShellRegistry,
    sessions_dir: Option<PathBuf>,
    process_id: String,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        let status = child.wait().await.ok();
        let exit_code = status.and_then(|s| s.code());
        set_state(
            &registry,
            sessions_dir.as_deref(),
            &process_id,
            "exited",
            exit_code,
        )
        .await;
    });
}

/// Watch a reparented orphan adopted after a hot switch. We aren't its
/// parent (it was reparented to PID 1), so there's no `wait()` — poll
/// `/proc` instead. The exit code of an orphan we didn't reap is not
/// retrievable, so this only ever records that it finished, not how.
fn spawn_adopted_reaper(
    registry: ShellRegistry,
    sessions_dir: Option<PathBuf>,
    process_id: String,
    pid: i32,
    pid_start_ticks: Option<u64>,
) {
    tokio::spawn(async move {
        loop {
            if !pid_still_this_process(pid, pid_start_ticks) {
                set_state(
                    &registry,
                    sessions_dir.as_deref(),
                    &process_id,
                    "exited",
                    None,
                )
                .await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

/// Scan every session's `.shell_procs/` for entries still marked `running`
/// from a previous daemon instance and reconcile them against reality.
///
/// `hot_switch: true` (this process started via SIGUSR1/`myclaw restart`):
/// shell children survive — verify each PID is still alive and, if so,
/// resume watching it (`spawn_adopted_reaper`) so `shell_poll`/`shell_kill`
/// keep working across the restart exactly as if nothing happened.
///
/// `hot_switch: false` (`myclaw stop`, `systemctl`, or a crash — anything
/// that goes through this deployment's `KillMode=control-group`): the whole
/// cgroup including shell children is guaranteed dead. Don't probe PIDs at
/// all (a reused PID could otherwise be mistaken for survival) — mark every
/// running entry `lost_on_restart` so `shell_poll` reports it honestly.
///
/// Either way, terminal entries older than `ENTRY_RETENTION_SECS` are swept
/// so `.shell_procs/` doesn't grow without bound.
pub async fn adopt_after_restart(
    sessions_dir: &Path,
    registry: &ShellRegistry,
    hot_switch: bool,
) {
    let Ok(session_dirs) = std::fs::read_dir(sessions_dir) else {
        return;
    };
    let now = now_ms();
    for session_dir in session_dirs.flatten() {
        let procs_dir = session_dir.path().join(".shell_procs");
        let Ok(files) = std::fs::read_dir(&procs_dir) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(entry) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<ProcEntry>(&s).ok())
            else {
                continue;
            };

            if entry.state != "running" {
                let age_secs = (now - entry.spawned_at_ms) / 1000;
                if age_secs > ENTRY_RETENTION_SECS {
                    remove_entry_disk(Some(sessions_dir), &entry);
                }
                continue;
            }

            if hot_switch && pid_still_this_process(entry.pid, entry.pid_start_ticks) {
                registry
                    .write()
                    .await
                    .insert(entry.process_id.clone(), entry.clone());
                spawn_adopted_reaper(
                    registry.clone(),
                    Some(sessions_dir.to_path_buf()),
                    entry.process_id.clone(),
                    entry.pid,
                    entry.pid_start_ticks,
                );
                tracing::info!(
                    process_id = %entry.process_id,
                    session_id = %entry.session_id,
                    pid = entry.pid,
                    "adopted running shell process after hot switch"
                );
            } else {
                let mut lost = entry.clone();
                lost.state = if hot_switch {
                    "exited".to_string() // hot switch, but the PID is already gone
                } else {
                    "lost_on_restart".to_string()
                };
                write_entry_disk(Some(sessions_dir), &lost);
                registry
                    .write()
                    .await
                    .insert(lost.process_id.clone(), lost.clone());
                tracing::warn!(
                    process_id = %entry.process_id,
                    session_id = %entry.session_id,
                    state = %lost.state,
                    "shell process not recoverable after restart"
                );
            }
        }
    }
}

/// Kill every process still tracked `running` for `session_id`.
///
/// Cancelling a sub-agent's turn (delegation timeout, `agent_kill`) only
/// drops/aborts the future driving its LLM conversation loop — it does not
/// reach into the detached `tokio::spawn` reaper tasks that track any shell
/// children that sub-agent started (`spawn_tracked` hands each `Child` off
/// to one of those immediately, by design, so `myclaw restart` can adopt
/// them — see the module docs). Without this, a sub-agent's shell process
/// keeps running for real after its delegation has already been reported as
/// timed out or killed, which looks exactly like the delegation timeout
/// having no effect. Call this from both cancellation paths so a delegation
/// ending always means its shell children stop too.
///
/// Sends SIGTERM only (not SIGKILL) — same default as `shell_kill` — and
/// does not wait for confirmation; the process's own reaper task (already
/// running) updates the table once it actually exits.
pub async fn kill_processes_for_session(registry: &ShellRegistry, session_id: &str) {
    let victims: Vec<(String, i32)> = registry
        .read()
        .await
        .values()
        .filter(|e| e.session_id == session_id && e.state == "running")
        .map(|e| (e.process_id.clone(), e.pid))
        .collect();
    for (process_id, pid) in victims {
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc == 0 {
            tracing::info!(process_id, pid, session_id, "killed shell process on delegation end");
        } else {
            let err = std::io::Error::last_os_error();
            tracing::warn!(process_id, pid, session_id, err = %err, "failed to kill shell process on delegation end");
        }
    }
}

/// Describe the most recently spawned tracked process for a session, if
/// any — used by `agent.rs`'s crash-recovery path (the `exec_marker`
/// mechanism) to tell the LLM what an interrupted `shell` call was doing
/// instead of a generic "unknown". Reads directly from disk (not the live
/// registry) since recovery for a given session can run long after
/// `adopt_after_restart` resolved every entry's post-restart state at
/// daemon startup. Call this *after* `adopt_after_restart` has run, or a
/// `running` entry might still be from before restart-classification.
pub fn latest_entry_summary(sessions_dir: &Path, session_id: &str) -> Option<String> {
    let pdir = entry_dir(sessions_dir, session_id);
    let mut entries: Vec<ProcEntry> = std::fs::read_dir(&pdir)
        .ok()?
        .flatten()
        .filter(|f| f.path().extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|f| {
            std::fs::read_to_string(f.path())
                .ok()
                .and_then(|s| serde_json::from_str::<ProcEntry>(&s).ok())
        })
        .collect();
    entries.sort_by_key(|e| e.spawned_at_ms);
    let entry = entries.pop()?;

    Some(match entry.state.as_str() {
        "running" => format!(
            "This shell command is still running (process_id={}, command={:?}) — it \
             survived the restart and was NOT re-executed to avoid running it twice. \
             Use shell_poll to check on it or shell_kill to stop it.",
            entry.process_id, entry.command
        ),
        "lost_on_restart" => format!(
            "This shell command's process did not survive the restart (process_id={}, \
             command={:?}) — it may have partially or fully completed. Check the current \
             state before deciding whether to re-run it.",
            entry.process_id, entry.command
        ),
        _ => format!(
            "This shell command already finished (state={}, exit_code={:?}, \
             process_id={}, command={:?}). Use shell_poll to see its captured output.",
            entry.state, entry.exit_code, entry.process_id, entry.command
        ),
    })
}

// ── ShellTool ────────────────────────────────────────────────────────────

pub struct ShellTool {
    registry: ShellRegistry,
    sessions_dir: Option<PathBuf>,
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
        }
    }

    pub fn registry(&self) -> ShellRegistry {
        self.registry.clone()
    }

    pub fn sessions_dir(&self) -> Option<PathBuf> {
        self.sessions_dir.clone()
    }

    /// Spawn `command`, register it, and start the reaper that watches it to
    /// completion. Never kills anything — that's `shell_kill`'s job.
    async fn spawn_tracked(
        &self,
        command: &str,
        workdir: Option<&str>,
        session_id: &str,
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
        };
        write_entry_disk(self.sessions_dir.as_deref(), &entry);
        self.registry.write().await.insert(process_id.clone(), entry);

        spawn_owned_reaper(
            self.registry.clone(),
            self.sessions_dir.clone(),
            process_id.clone(),
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
         a process_id plus whatever output has been produced so far — use \
         shell_poll to keep checking it and shell_kill to terminate it. \
         Set `background: true` to skip waiting entirely and get the process_id \
         immediately. Large output (>30K chars) is truncated: the first 30K is \
         returned inline and the full output is available via full_output_path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute." },
                "timeout_secs": { "type": "integer", "description": "Seconds to wait for completion before returning (default 120, max 300). The command keeps running past this — it is not killed on timeout." },
                "workdir": { "type": "string", "description": "Working directory (default: current)." },
                "background": { "type": "boolean", "description": "If true, return a process_id immediately instead of waiting. Use shell_poll to check status and collect output." }
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
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'command' is required"))?;
        let background = args["background"].as_bool().unwrap_or(false);
        let workdir = args["workdir"].as_str();
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120).min(300);

        let process_id = match self.spawn_tracked(command, workdir, &session.id).await {
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
                    "state=running\nprocess_id={}\ncommand={}\nuse shell_poll to check status and collect output\n",
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
            // Timed out, still going — NOT killed. Report and hand back the
            // process_id; shell_poll/shell_kill take it from here.
            let output = format!(
                "state=running\nprocess_id={}\ncommand={}\ntimeout_secs={}\nnote=command is still running, it was NOT killed; use shell_poll(process_id) to keep checking or shell_kill(process_id) to terminate it\n\noutput_so_far{}:\n{}",
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

        let exit_code = entry.exit_code.unwrap_or(-1);
        let success = entry.state == "exited" && exit_code == 0;
        let output = format!(
            "state={}\nexit_code={}\nprocess_id={}\n\noutput{}:\n{}",
            entry.state,
            entry.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
            process_id,
            if output_truncated { " (truncated)" } else { "" },
            display
        );
        Ok(ToolResult {
            success,
            output,
            error: if success {
                None
            } else {
                Some(format!("exit code {}", exit_code))
            },
        })
    }
}

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
         Returns accumulated output and machine-readable state (running/exited/killed/lost_on_restart) \
         and exit_code. Set `wait_secs` to wait for completion before returning. Set `remove: true` \
         to clean up the process entry after reading — if it's still running this also terminates it \
         (equivalent to shell_kill) before removing. `lost_on_restart` means the daemon restarted \
         through a path that does not preserve child processes; the output shown is everything \
         captured before that happened."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string", "description": "The process_id returned by `shell`." },
                "remove": { "type": "boolean", "description": "If true, remove the process entry after reading output (default: false). If the process is still running, this kills it first." },
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
        let process_id = args["process_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'process_id' is required"))?;
        let remove = args["remove"].as_bool().unwrap_or(false);
        let wait_secs = args["wait_secs"].as_u64().unwrap_or(0).min(300);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
        let mut entry = match self.registry.read().await.get(process_id).cloned() {
            Some(e) => e,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("process_id '{}' not found", process_id)),
                });
            }
        };
        while entry.state == "running" && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            entry = match self.registry.read().await.get(process_id).cloned() {
                Some(e) => e,
                None => break,
            };
        }

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
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let process_id = args["process_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'process_id' is required"))?;
        let signal = args["signal"].as_str().unwrap_or("TERM");
        let sig = match signal {
            "KILL" => libc::SIGKILL,
            _ => libc::SIGTERM,
        };

        let entry = match self.registry.read().await.get(process_id).cloned() {
            Some(e) => e,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("process_id '{}' not found", process_id)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(state: &str) -> ProcEntry {
        ProcEntry {
            process_id: "sh_test".to_string(),
            session_id: "session1".to_string(),
            command: "echo hi".to_string(),
            workdir: None,
            pid: 999_999, // implausible pid, never alive
            pid_start_ticks: None,
            spawned_at_ms: now_ms(),
            output_path: "/tmp/does-not-matter".to_string(),
            state: state.to_string(),
            exit_code: None,
        }
    }

    #[test]
    fn read_lossy_tolerates_invalid_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.bin");
        // Invalid UTF-8: truncated multibyte char (e.g. from `cut -c` on CJK).
        let mut bytes = "ab".as_bytes().to_vec();
        bytes.push(0xe4); // start of a 3-byte char, never completed
        std::fs::write(&path, &bytes).unwrap();
        let s = read_lossy(&path).expect("lossy read must succeed");
        assert!(s.starts_with("ab"));
    }

    #[test]
    fn cap_output_for_display_under_limit_untouched() {
        let (text, truncated) = cap_output_for_display("short", Path::new("/tmp/x"));
        assert_eq!(text, "short");
        assert!(!truncated);
    }

    #[test]
    fn cap_output_for_display_over_limit_truncates_and_points_at_file() {
        let big = "a".repeat(MAX_OUTPUT_INLINE * 10);
        let (text, truncated) = cap_output_for_display(&big, Path::new("/tmp/out.txt"));
        assert!(truncated);
        assert!(text.contains("full_output_path=/tmp/out.txt"));
        assert!(text.len() < big.len());
    }

    #[test]
    fn entry_disk_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = test_entry("running");
        write_entry_disk(Some(tmp.path()), &entry);

        let pdir = entry_dir(tmp.path(), &entry.session_id);
        let loaded: ProcEntry =
            serde_json::from_str(&std::fs::read_to_string(entry_json_path(&pdir, &entry.process_id)).unwrap())
                .unwrap();
        assert_eq!(loaded.process_id, entry.process_id);
        assert_eq!(loaded.state, "running");

        remove_entry_disk(Some(tmp.path()), &entry);
        assert!(!entry_json_path(&pdir, &entry.process_id).exists());
    }

    #[tokio::test]
    async fn adopt_after_restart_marks_running_entries_lost_when_not_hot_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = test_entry("running");
        write_entry_disk(Some(tmp.path()), &entry);

        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        adopt_after_restart(tmp.path(), &registry, false).await;

        let map = registry.read().await;
        let got = map.get(&entry.process_id).expect("entry adopted into registry");
        assert_eq!(got.state, "lost_on_restart");
    }

    #[tokio::test]
    async fn adopt_after_restart_marks_dead_pid_exited_even_on_hot_switch() {
        let tmp = tempfile::tempdir().unwrap();
        // pid 999_999 is never alive on any real system used for these tests.
        let entry = test_entry("running");
        write_entry_disk(Some(tmp.path()), &entry);

        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        adopt_after_restart(tmp.path(), &registry, true).await;

        let map = registry.read().await;
        let got = map.get(&entry.process_id).expect("entry adopted into registry");
        assert_eq!(got.state, "exited");
    }

    #[tokio::test]
    async fn adopt_after_restart_ignores_already_terminal_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut entry = test_entry("exited");
        entry.exit_code = Some(0);
        write_entry_disk(Some(tmp.path()), &entry);

        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        adopt_after_restart(tmp.path(), &registry, false).await;

        // Not touched — stays absent from the freshly-built registry since
        // only `running` entries get inserted during adoption.
        assert!(registry.read().await.get(&entry.process_id).is_none());
        // And the on-disk copy is untouched (still "exited", not swept —
        // it's within the retention window).
        let pdir = entry_dir(tmp.path(), &entry.session_id);
        let on_disk: ProcEntry =
            serde_json::from_str(&std::fs::read_to_string(entry_json_path(&pdir, &entry.process_id)).unwrap())
                .unwrap();
        assert_eq!(on_disk.state, "exited");
    }

    #[tokio::test]
    async fn multiline_heredoc_runs_unmodified() {
        let tool = ShellTool::new(None);
        let session = crate::agents::session::Session::new("s1".to_string());
        let result = tool
            .execute(
                json!({
                    "command": "cat <<'EOF'\nline one\nline two\nEOF",
                    "timeout_secs": 10
                }),
                &session,
            )
            .await
            .unwrap();
        assert!(result.success, "output: {}", result.output);
        assert!(result.output.contains("line one"));
        assert!(result.output.contains("line two"));
    }

    #[tokio::test]
    async fn multiline_if_block_runs_unmodified() {
        let tool = ShellTool::new(None);
        let session = crate::agents::session::Session::new("s1".to_string());
        let result = tool
            .execute(
                json!({
                    "command": "if true\nthen\n  echo yes\nfi",
                    "timeout_secs": 10
                }),
                &session,
            )
            .await
            .unwrap();
        assert!(result.success, "output: {}", result.output);
        assert!(result.output.contains("yes"));
    }

    #[tokio::test]
    async fn timeout_does_not_kill_process() {
        let tool = ShellTool::new(None);
        let session = crate::agents::session::Session::new("s1".to_string());
        let result = tool
            .execute(
                json!({ "command": "sleep 1 && echo done", "timeout_secs": 0 }),
                &session,
            )
            .await
            .unwrap();
        // 0s timeout: immediately reports still-running, doesn't kill.
        assert!(result.output.contains("state=running"));
        let process_id = result
            .output
            .lines()
            .find_map(|l| l.strip_prefix("process_id="))
            .expect("process_id present")
            .to_string();

        let registry = tool.registry();
        // Poll until it actually finishes — proves the process kept running
        // past the timeout instead of being killed.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = registry.read().await.get(&process_id).map(|e| e.state.clone());
            if state.as_deref() == Some("exited") {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "process never completed — was it killed?");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn shell_kill_terminates_running_process() {
        let tool = ShellTool::new(None);
        let session = crate::agents::session::Session::new("s1".to_string());
        let result = tool
            .execute(
                json!({ "command": "sleep 30", "background": true }),
                &session,
            )
            .await
            .unwrap();
        let process_id = result
            .output
            .lines()
            .find_map(|l| l.strip_prefix("process_id="))
            .unwrap()
            .to_string();

        let kill_tool = ShellKillTool::new(tool.registry());
        let kill_result = kill_tool
            .execute(json!({ "process_id": process_id }), &session)
            .await
            .unwrap();
        assert!(kill_result.success, "output: {}", kill_result.output);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = tool.registry().read().await.get(&process_id).map(|e| e.state.clone());
            if state.as_deref() == Some("exited") {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "killed process never reaped");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn kill_processes_for_session_only_kills_that_sessions_running_entries() {
        // Simulates the delegation-timeout/agent_kill cleanup: a sub-agent's
        // session has a shell process still running when its delegation
        // ends, and cancelling the delegation's future alone would not have
        // touched it (see kill_processes_for_session's doc comment).
        let tool = ShellTool::new(None);
        let session_a = crate::agents::session::Session::new("session-a".to_string());
        let session_b = crate::agents::session::Session::new("session-b".to_string());

        let result_a = tool
            .execute(json!({ "command": "sleep 30", "background": true }), &session_a)
            .await
            .unwrap();
        let pid_a = result_a
            .output
            .lines()
            .find_map(|l| l.strip_prefix("process_id="))
            .unwrap()
            .to_string();

        let result_b = tool
            .execute(json!({ "command": "sleep 30", "background": true }), &session_b)
            .await
            .unwrap();
        let pid_b = result_b
            .output
            .lines()
            .find_map(|l| l.strip_prefix("process_id="))
            .unwrap()
            .to_string();

        kill_processes_for_session(&tool.registry(), "session-a").await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = tool.registry().read().await.get(&pid_a).map(|e| e.state.clone());
            if state.as_deref() == Some("exited") {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "session-a process never reaped");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // session-b's process was never targeted — still running.
        let state_b = tool.registry().read().await.get(&pid_b).map(|e| e.state.clone());
        assert_eq!(state_b.as_deref(), Some("running"));

        // Clean up so the test doesn't leave a sleep(30) orphan behind.
        kill_processes_for_session(&tool.registry(), "session-b").await;
    }

    #[tokio::test]
    async fn shell_poll_reports_unknown_process_id() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        let poll_tool = ShellPollTool::new(registry, None);
        let session = crate::agents::session::Session::new("s1".to_string());
        let result = poll_tool
            .execute(json!({ "process_id": "sh_nope" }), &session)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }
}
