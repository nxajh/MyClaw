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
use tokio::sync::{mpsc, RwLock};
use tokio::time::Duration;

/// issue #129: a `background: true` shell command reached a terminal state.
/// Sent to the orchestrator (independent of sub-agent delegation — this
/// fires in single-agent mode too) so it can wake the spawning session with
/// the result, reusing the same completion-notice pipeline delegation uses
/// (batches concurrent completions into one turn, persists for at-least-once
/// delivery across a restart).
#[derive(Debug, Clone)]
pub struct ShellCompletion {
    pub session_id: String,
    pub process_id: String,
    pub content: String,
    /// issue #140: lets the orchestrator map this completion onto
    /// `SubStatus` (Completed/Failed) for `record_terminal`, the same
    /// suspension-collection call delegation terminal events go through.
    /// `None` for an adopted orphan (never observed the real exit).
    pub exit_code: Option<i32>,
}

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
    /// issue #129/#131: whether this process's eventual exit should wake the
    /// session with a completion notice. True for an explicit `background:
    /// true` spawn from the start; a plain foreground call starts `false`
    /// and only flips to `true` if it outruns `timeout_secs` while still
    /// running (issue #131 decision 2 — a forced-background conversion,
    /// reversing #129/#130's original "timeout path never notifies"
    /// decision). A foreground call that finishes within `timeout_secs`
    /// never flips — its result was already delivered synchronously, so a
    /// notice would just duplicate it. `#[serde(default)]` so process-table
    /// entries persisted before this field existed still deserialize (as
    /// `false`, the old behavior).
    #[serde(default)]
    notify_on_exit: bool,
}

impl ProcEntry {
    /// issue #131: owning session, for `OrchestratorCtx::running_shell_processes`
    /// to filter the shared registry per-session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_running(&self) -> bool {
        self.state == "running"
    }

    pub fn summary(&self) -> ProcSummary {
        ProcSummary {
            process_id: self.process_id.clone(),
            command: self.command.clone(),
            spawned_at_ms: self.spawned_at_ms,
        }
    }
}

/// issue #131: read-only view of a tracked process for the orchestrator's
/// background-work status reminder (injected into a user turn that
/// interrupts pending async work) — deliberately narrower than `ProcEntry`
/// so that module stays the only place that knows the full entry shape.
pub struct ProcSummary {
    pub process_id: String,
    pub command: String,
    pub spawned_at_ms: i64,
}

pub type ShellRegistry = Arc<RwLock<HashMap<String, ProcEntry>>>;

/// issue #140: `spawn_tracked` always prefixes process ids with `sh_` — used
/// by `recover_suspension` (orchestrator startup recovery) to tell a shell
/// pending entry apart from a sub-agent session id in the shared, generic
/// `TurnSuspension.pending` list.
pub fn is_shell_process_id(id: &str) -> bool {
    id.starts_with("sh_")
}

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

/// Returns the updated entry (cloned) so callers can decide whether to
/// notify without a second registry lookup racing a concurrent update.
async fn set_state(
    registry: &ShellRegistry,
    sessions_dir: Option<&Path>,
    process_id: &str,
    state: &str,
    exit_code: Option<i32>,
) -> Option<ProcEntry> {
    let mut map = registry.write().await;
    let entry = map.get_mut(process_id)?;
    entry.state = state.to_string();
    entry.exit_code = exit_code;
    write_entry_disk(sessions_dir, entry);
    Some(entry.clone())
}

/// issue #131 decision 2: flip a still-running process to "notify on exit"
/// once it has outrun `timeout_secs` — the synchronous `shell` call is about
/// to report it as still going (no terminal result to deliver inline), so
/// unify it with the explicit `background: true` case and notify when it
/// eventually finishes.
///
/// Returns the entry's state *after* the attempted flip (still cloned even
/// when no flip happened) so the caller can detect the narrow race where the
/// process exits between `execute()`'s deadline snapshot and this function
/// acquiring the write lock: the reaper may already have recorded a
/// terminal state with `notify_on_exit` still `false` (this function then
/// correctly no-ops rather than flipping a state that's no longer
/// `"running"`), in which case no notice will ever fire for it and the
/// caller must report the terminal result inline instead of promising one
/// (review finding on #139 — xiaoer-bot). `None` only if the entry was
/// removed from the registry entirely in that same window.
async fn mark_notify_on_exit(
    registry: &ShellRegistry,
    sessions_dir: Option<&Path>,
    process_id: &str,
) -> Option<ProcEntry> {
    let mut map = registry.write().await;
    let entry = map.get_mut(process_id)?;
    if entry.state == "running" && !entry.notify_on_exit {
        entry.notify_on_exit = true;
        write_entry_disk(sessions_dir, entry);
    }
    Some(entry.clone())
}

/// Render the terminal-state tool output shared by `execute()`'s normal
/// completion path and its race-recovery path (an entry that turned out to
/// already be terminal when `mark_notify_on_exit` looked).
///
/// issue #141: `success` reflects whether the TOOL did its job (spawned,
/// tracked, captured output to a clean exit) — not whether the command's
/// own exit code happened to be zero. A nonzero exit code is data about the
/// command's result (`grep` no-match, `git diff --quiet` has changes, `gh
/// pr checks` pending), not a tool malfunction; `state=`/`exit_code=` are
/// the first two output lines specifically so the model can read that data
/// itself. This aligns the foreground path with `shell_poll`, which has
/// always treated reaching any terminal state as tool-success. `killed` /
/// `lost_on_restart` stay `success: false` — those ARE abnormal
/// terminations (the process didn't get to run to its own completion).
fn terminal_result(entry: &ProcEntry, process_id: &str, display: &str, output_truncated: bool) -> ToolResult {
    let success = entry.state == "exited";
    let output = format!(
        "state={}\nexit_code={}\nprocess_id={}\n\noutput{}:\n{}",
        entry.state,
        entry.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
        process_id,
        if output_truncated { " (truncated)" } else { "" },
        display
    );
    ToolResult {
        success,
        output,
        error: if success {
            None
        } else {
            Some(format!("process did not exit normally (state={})", entry.state))
        },
    }
}

/// issue #129: build a `background: true` command's completion-notice
/// content — process_id, exit_code, and the same inline-truncated output
/// tail `shell`/`shell_poll` already show, plus `output_path` for the full
/// capture. `exit_code: None` covers the adopted-orphan reaper, which can
/// only observe that the process is gone, never how it exited.
fn build_completion_content(process_id: &str, exit_code: Option<i32>, output_path: &Path) -> String {
    let (raw, _) = read_lossy_tail(output_path, MAX_TRACKED_OUTPUT);
    let (display, output_truncated) =
        cap_output_for_display(&crate::str_utils::neutralize_spoofing(&raw), output_path);
    format!(
        "[系统通知] 后台命令已完成 (process_id: {}, exit_code: {})。\noutput_path: {}\n\noutput{}:\n{}",
        process_id,
        exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
        output_path.display(),
        if output_truncated { "（已截断）" } else { "" },
        display
    )
}

/// issue #140: build the restart-recovery notice for a `pending` shell
/// process_id that did NOT survive the restart — checked by the caller
/// against the live registry (a `running` entry after `adopt_after_restart`
/// means it DID survive and its adopted reaper will complete it naturally;
/// this is only for the rest). Reuses whatever the registry captured
/// (state/exit_code/output tail, same shape as `build_completion_content`)
/// for a richer notice than a generic "unknown"; falls back to a minimal
/// message when the entry itself is gone (a much older restart already
/// swept it via `ENTRY_RETENTION_SECS`).
pub async fn recovery_lost_content(registry: &ShellRegistry, process_id: &str) -> String {
    let entry = registry.read().await.get(process_id).cloned();
    match entry {
        Some(e) => {
            let (raw, _) = read_lossy_tail(Path::new(&e.output_path), MAX_TRACKED_OUTPUT);
            let (display, output_truncated) = cap_output_for_display(
                &crate::str_utils::neutralize_spoofing(&raw),
                Path::new(&e.output_path),
            );
            format!(
                "[系统通知] 后台命令已中断 (process_id: {}, state: {}, exit_code: {}): daemon 重启，该进程未能存活（可能已完成但未及记录，也可能被中止）。\noutput_path: {}\n\noutput{}:\n{}",
                process_id,
                e.state,
                e.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
                e.output_path,
                if output_truncated { "（已截断）" } else { "" },
                display
            )
        }
        None => format!(
            "[系统通知] 后台命令状态未知 (process_id: {}): daemon 重启，该进程记录已不可查（可能已被清理）。若仍需要这次任务的结果，请重新执行。",
            process_id
        ),
    }
}

/// Watch a process this daemon instance actually spawned (owns as a real
/// child) to completion, updating the table when it exits.
///
/// `notice_tx` is the tool's completion-notice sender whenever one is
/// configured (issue #129) — whether a notice actually fires depends on the
/// entry's live `notify_on_exit` at the moment of exit (issue #131 decision
/// 2), NOT a value captured at spawn time: a plain foreground call starts
/// `notify_on_exit=false` but `execute()`'s timeout branch can flip it to
/// `true` (via `mark_notify_on_exit`) after this reaper is already running,
/// once it outruns `timeout_secs` while still going.
#[allow(clippy::too_many_arguments)]
fn spawn_owned_reaper(
    registry: ShellRegistry,
    sessions_dir: Option<PathBuf>,
    process_id: String,
    session_id: String,
    output_path: PathBuf,
    notice_tx: Option<mpsc::Sender<ShellCompletion>>,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        let status = child.wait().await.ok();
        let exit_code = status.and_then(|s| s.code());
        let updated = set_state(
            &registry,
            sessions_dir.as_deref(),
            &process_id,
            "exited",
            exit_code,
        )
        .await;
        if let (Some(tx), Some(entry)) = (notice_tx, updated) {
            if entry.notify_on_exit {
                let content = build_completion_content(&process_id, exit_code, &output_path);
                let _ = tx
                    .send(ShellCompletion {
                        session_id,
                        process_id,
                        content,
                        exit_code,
                    })
                    .await;
            }
        }
    });
}

/// Watch a reparented orphan adopted after a hot switch. We aren't its
/// parent (it was reparented to PID 1), so there's no `wait()` — poll
/// `/proc` instead. The exit code of an orphan we didn't reap is not
/// retrievable, so this only ever records that it finished, not how.
///
/// `notice_tx` mirrors `spawn_owned_reaper`: whether a notice actually fires
/// is decided from the live entry's `notify_on_exit` at exit time, not a
/// value captured at adopt time (issue #131 decision 2).
#[allow(clippy::too_many_arguments)]
fn spawn_adopted_reaper(
    registry: ShellRegistry,
    sessions_dir: Option<PathBuf>,
    process_id: String,
    session_id: String,
    output_path: PathBuf,
    notice_tx: Option<mpsc::Sender<ShellCompletion>>,
    pid: i32,
    pid_start_ticks: Option<u64>,
) {
    tokio::spawn(async move {
        loop {
            if !pid_still_this_process(pid, pid_start_ticks) {
                let updated = set_state(
                    &registry,
                    sessions_dir.as_deref(),
                    &process_id,
                    "exited",
                    None,
                )
                .await;
                if let (Some(tx), Some(entry)) = (notice_tx, updated) {
                    if entry.notify_on_exit {
                        let content = build_completion_content(&process_id, None, &output_path);
                        let _ = tx
                            .send(ShellCompletion {
                                session_id,
                                process_id,
                                content,
                                exit_code: None,
                            })
                            .await;
                    }
                }
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
    notice_tx: Option<mpsc::Sender<ShellCompletion>>,
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
                    entry.session_id.clone(),
                    PathBuf::from(&entry.output_path),
                    notice_tx.clone(),
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
             Call sessions_yield to suspend and be woken when it finishes, shell_poll for \
             an instant peek, or shell_kill to stop it.",
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
    session_manager: std::sync::OnceLock<Arc<crate::agents::SessionManager>>,
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
    pub fn set_session_manager(&self, sm: Arc<crate::agents::SessionManager>) {
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
        super::shell_env::apply(&mut cmd);
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
        session: &crate::agents::session::Session,
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
const UNKNOWN_PROCESS_LISTING_CAP: usize = 20;
const UNKNOWN_PROCESS_COMMAND_PREVIEW_CHARS: usize = 60;

fn format_unknown_process_listing(registry: &HashMap<String, ProcEntry>, session_id: &str) -> String {
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
        session: &crate::agents::session::Session,
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
        session: &crate::agents::session::Session,
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
            notify_on_exit: false,
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
        adopt_after_restart(tmp.path(), &registry, false, None).await;

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
        adopt_after_restart(tmp.path(), &registry, true, None).await;

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
        adopt_after_restart(tmp.path(), &registry, false, None).await;

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

    /// issue #129: a `background: true` process still running across a hot
    /// switch keeps its notice armed — `adopt_after_restart` must thread
    /// `notify_on_exit` (persisted on the entry) and the notice sender into
    /// `spawn_adopted_reaper` so the eventual exit still wakes the session.
    /// (Adopted processes were never our child, so `exit_code` is
    /// unobservable — `None`/`null`, unlike the owned-reaper path.)
    #[tokio::test]
    async fn adopted_background_process_sends_notice_on_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 0.2")
            .spawn()
            .expect("spawn real child for adoption test");
        let pid = child.id().expect("child has a pid") as i32;

        let mut entry = test_entry("running");
        entry.pid = pid;
        entry.pid_start_ticks = pid_start_ticks(pid);
        entry.notify_on_exit = true;
        entry.output_path = tmp.path().join("out.txt").to_string_lossy().to_string();
        std::fs::write(&entry.output_path, b"").unwrap();
        write_entry_disk(Some(tmp.path()), &entry);

        let (tx, mut rx) = mpsc::channel::<ShellCompletion>(8);
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        adopt_after_restart(tmp.path(), &registry, true, Some(tx)).await;

        // Reap the real child ourselves too so the test doesn't leak a
        // zombie — the adopted reaper only polls /proc, it never wait()s.
        let _ = child.wait().await;

        let notice = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("completion notice must arrive for an adopted background process")
            .expect("channel not closed");
        assert_eq!(notice.session_id, entry.session_id);
        assert_eq!(notice.process_id, entry.process_id);
        assert!(notice.content.contains("exit_code: null"));
    }

    #[tokio::test]
    async fn multiline_heredoc_runs_unmodified() {
        let tool = ShellTool::new(None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
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

    /// issue #141: `terminal_result`'s `success` reflects whether the tool
    /// did its job, not whether the command's own exit code was zero — a
    /// nonzero exit code is data (`grep` no-match, `git diff --quiet` has
    /// changes, `gh pr checks` pending), not a tool malfunction. Only a
    /// non-`exited` terminal state (`lost_on_restart` — the daemon restarted
    /// through a path that didn't preserve the child) is a real tool-level
    /// failure. Tested directly against `terminal_result` rather than
    /// through `execute()`: in practice `execute()` only ever calls it with
    /// `state == "exited"` for the process it just spawned itself (a
    /// `lost_on_restart` entry only arises from `adopt_after_restart`
    /// recovering an OLD daemon instance's entries, never from a live
    /// spawn-and-wait cycle within one `execute()` call).
    #[test]
    fn terminal_result_success_only_reflects_tool_state_not_exit_code() {
        let mut exited_nonzero = test_entry("exited");
        exited_nonzero.exit_code = Some(8);
        let r = terminal_result(&exited_nonzero, "sh_x", "", false);
        assert!(r.success, "a nonzero exit code must still be tool success");
        assert!(r.error.is_none());

        let mut exited_zero = test_entry("exited");
        exited_zero.exit_code = Some(0);
        let r2 = terminal_result(&exited_zero, "sh_x", "", false);
        assert!(r2.success);

        let lost = test_entry("lost_on_restart");
        let r3 = terminal_result(&lost, "sh_x", "", false);
        assert!(!r3.success, "a non-exited terminal state is a real tool failure");
        assert!(r3.error.is_some());
    }

    /// issue #141: end-to-end through the real `execute()` path (not just
    /// `terminal_result` in isolation) — this is what actually reaches
    /// `agent.rs`'s `is_error`, which feeds the Telegram preview's `_failed_`
    /// suffix, the model-facing `is_error` flag on the provider protocols,
    /// and the skill-extraction turn-had-error gate.
    #[tokio::test]
    async fn nonzero_exit_code_is_tool_success_end_to_end() {
        let tool = ShellTool::new(None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = tool
            .execute(json!({ "command": "exit 8", "timeout_secs": 5 }), &session)
            .await
            .unwrap();
        assert!(result.success, "output: {}", result.output);
        assert!(result.error.is_none());
        assert!(result.output.contains("state=exited"));
        assert!(result.output.contains("exit_code=8"));
    }

    #[tokio::test]
    async fn timeout_does_not_kill_process() {
        let tool = ShellTool::new(None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
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

    /// issue #131 decision 2 (reverses #129/#130's original behavior): a
    /// plain (non-background) call that outran `timeout_secs` is forced into
    /// the same "notify on exit" semantics as an explicit `background: true`
    /// spawn — the synchronous return already reported no terminal result,
    /// so the eventual completion must still wake the session.
    #[tokio::test]
    async fn timeout_path_now_sends_completion_notice() {
        let (tx, mut rx) = mpsc::channel::<ShellCompletion>(8);
        let tool = ShellTool::new_with_notice_sender(None, tx);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = tool
            .execute(
                json!({ "command": "sleep 1 && echo done", "timeout_secs": 0 }),
                &session,
            )
            .await
            .unwrap();
        assert!(result.output.contains("state=running"));

        let notice = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout-forced-background process must send a completion notice")
            .expect("channel not closed");
        assert_eq!(notice.session_id, "s1");
        assert!(notice.content.contains("done"));
    }

    /// issue #131 decision 2's other half: a plain foreground call that
    /// finishes WITHIN `timeout_secs` already delivered its terminal result
    /// synchronously — it must NOT also fire an async completion notice
    /// (that would just duplicate what the model already saw in this turn).
    #[tokio::test]
    async fn fast_foreground_completion_sends_no_duplicate_notice() {
        let (tx, mut rx) = mpsc::channel::<ShellCompletion>(8);
        let tool = ShellTool::new_with_notice_sender(None, tx);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = tool
            .execute(
                json!({ "command": "echo done", "timeout_secs": 5 }),
                &session,
            )
            .await
            .unwrap();
        assert!(result.output.contains("state=exited"));

        assert!(
            tokio::time::timeout(Duration::from_millis(300), rx.recv())
                .await
                .is_err(),
            "a foreground call that finished within timeout_secs must not send a completion notice"
        );
    }

    /// issue #131 review (xiaoer-bot, #139): `mark_notify_on_exit` flips a
    /// still-running entry and returns its live (now-armed) state.
    #[tokio::test]
    async fn mark_notify_on_exit_flips_running_entry() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        registry
            .write()
            .await
            .insert("sh_x".to_string(), test_entry("running"));

        let live = mark_notify_on_exit(&registry, None, "sh_x")
            .await
            .expect("entry present");
        assert_eq!(live.state, "running");
        assert!(live.notify_on_exit);
    }

    /// issue #131 review (xiaoer-bot, #139): the narrow race this guards —
    /// if the process has already reached a terminal state by the time
    /// `mark_notify_on_exit` gets the write lock (the reaper won), it must
    /// no-op (nothing "running" to arm) and hand back the terminal state so
    /// `execute()` can report it inline instead of promising a notify that
    /// will never fire.
    #[tokio::test]
    async fn mark_notify_on_exit_is_a_noop_once_already_terminal() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        let mut entry = test_entry("exited");
        entry.exit_code = Some(0);
        registry.write().await.insert("sh_x".to_string(), entry);

        let live = mark_notify_on_exit(&registry, None, "sh_x")
            .await
            .expect("entry present");
        assert_eq!(live.state, "exited");
        assert!(
            !live.notify_on_exit,
            "a terminal entry must not be armed — the reaper already decided notify_on_exit for it"
        );
    }

    /// issue #129: `background: true` must get a completion notice once the
    /// process exits, carrying process_id, exit_code, and an output tail.
    #[tokio::test]
    async fn background_completion_sends_notice_with_exit_code_and_output() {
        let (tx, mut rx) = mpsc::channel::<ShellCompletion>(8);
        let tool = ShellTool::new_with_notice_sender(None, tx);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = tool
            .execute(
                json!({ "command": "echo hello-from-bg", "background": true }),
                &session,
            )
            .await
            .unwrap();
        let process_id = result
            .output
            .lines()
            .find_map(|l| l.strip_prefix("process_id="))
            .expect("process_id present")
            .to_string();

        let notice = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("completion notice must arrive")
            .expect("channel not closed");
        assert_eq!(notice.session_id, "s1");
        assert_eq!(notice.process_id, process_id);
        assert_eq!(notice.exit_code, Some(0));
        assert!(notice.content.contains(&process_id));
        assert!(notice.content.contains("exit_code: 0"));
        assert!(notice.content.contains("hello-from-bg"));
    }

    /// issue #140: a `background: true` spawn registers itself as pending
    /// async work on its own session's suspension — the same mechanism
    /// `agent_delegate(mode="async")` uses (`SessionContext::add_pending_task`)
    /// — once `ShellTool` has a `SessionManager` wired in.
    #[tokio::test]
    async fn background_spawn_registers_pending_via_session_manager() {
        let sm = Arc::new(crate::agents::session::SessionManager::default());
        let sctx = sm.get_or_create_context("test:routing:key");
        let sid = sctx.session_id.clone();
        assert!(!sctx.has_pending_async_work());

        let tool = ShellTool::new(None);
        tool.set_session_manager(Arc::clone(&sm));
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: sid.clone(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = tool
            .execute(json!({ "command": "sleep 30", "background": true }), &session)
            .await
            .unwrap();
        let process_id = result
            .output
            .lines()
            .find_map(|l| l.strip_prefix("process_id="))
            .expect("process_id present")
            .to_string();

        assert!(sctx.has_pending_async_work());
        let snap = sctx.suspension_snapshot().expect("suspension created");
        assert!(snap.pending.contains(&process_id));

        // Clean up so the test doesn't leak a sleep(30) orphan.
        let kill_tool = ShellKillTool::new(tool.registry());
        kill_tool
            .execute(json!({ "process_id": process_id }), &session)
            .await
            .unwrap();
    }

    /// issue #140: the timeout-conversion branch registers pending too, at
    /// the moment it flips `notify_on_exit` — not just the `background: true`
    /// spawn-time path.
    #[tokio::test]
    async fn timeout_conversion_registers_pending_via_session_manager() {
        let sm = Arc::new(crate::agents::session::SessionManager::default());
        let sctx = sm.get_or_create_context("test:routing:key2");
        let sid = sctx.session_id.clone();

        let tool = ShellTool::new(None);
        tool.set_session_manager(Arc::clone(&sm));
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: sid.clone(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = tool
            .execute(
                json!({ "command": "sleep 1 && echo done", "timeout_secs": 0 }),
                &session,
            )
            .await
            .unwrap();
        assert!(result.output.contains("state=running"));
        let process_id = result
            .output
            .lines()
            .find_map(|l| l.strip_prefix("process_id="))
            .expect("process_id present")
            .to_string();

        assert!(sctx.has_pending_async_work());
        let snap = sctx.suspension_snapshot().expect("suspension created");
        assert!(snap.pending.contains(&process_id));
    }

    /// issue #140: without a wired `SessionManager` (bare CLI / tests), a
    /// `background: true` spawn must not panic — `register_pending` is a
    /// silent no-op.
    #[tokio::test]
    async fn background_spawn_without_session_manager_does_not_panic() {
        let tool = ShellTool::new(None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = tool
            .execute(json!({ "command": "echo hi", "background": true }), &session)
            .await
            .unwrap();
        assert!(result.output.contains("state=running"));
    }

    /// issue #129: multiple background completions from the same session
    /// each arrive as their own notice on the channel — coalescing them into
    /// one turn is the orchestrator's job (drain_delegation_notices), not
    /// this tool's; this only proves neither completion is dropped or
    /// merged away at the source.
    #[tokio::test]
    async fn multiple_background_completions_all_arrive() {
        let (tx, mut rx) = mpsc::channel::<ShellCompletion>(8);
        let tool = ShellTool::new_with_notice_sender(None, tx);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        for i in 0..3 {
            tool.execute(
                json!({ "command": format!("echo run-{i}"), "background": true }),
                &session,
            )
            .await
            .unwrap();
        }

        let mut seen = 0;
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("completion notice must arrive")
                .expect("channel not closed");
            seen += 1;
        }
        assert_eq!(seen, 3);
    }

    #[tokio::test]
    async fn shell_kill_terminates_running_process() {
        let tool = ShellTool::new(None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
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
        let session_a = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "session-a".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let session_b = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "session-b".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };

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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = poll_tool
            .execute(json!({ "process_id": "sh_nope" }), &session)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    fn entry_for(process_id: &str, session_id: &str, command: &str, spawned_at_ms: i64) -> ProcEntry {
        ProcEntry {
            process_id: process_id.to_string(),
            session_id: session_id.to_string(),
            command: command.to_string(),
            workdir: None,
            pid: 999_999,
            pid_start_ticks: None,
            spawned_at_ms,
            output_path: "/tmp/does-not-matter".to_string(),
            state: "running".to_string(),
            exit_code: None,
            notify_on_exit: false,
        }
    }

    /// issue #129: an unknown process_id (usually a transcription error —
    /// the id is 32 hex chars with no self-check) must come back with a
    /// listing of what's actually alive, scoped to the calling session and
    /// excluding other sessions' entries.
    #[tokio::test]
    async fn shell_poll_unknown_process_id_lists_this_sessions_processes_only() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        {
            let mut w = registry.write().await;
            w.insert(
                "sh_mine1".to_string(),
                entry_for("sh_mine1", "s1", "echo one", 1000),
            );
            w.insert(
                "sh_mine2".to_string(),
                entry_for("sh_mine2", "s1", "echo two", 2000),
            );
            w.insert(
                "sh_other".to_string(),
                entry_for("sh_other", "s2", "echo not-mine", 3000),
            );
        }
        let poll_tool = ShellPollTool::new(registry, None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = poll_tool
            .execute(json!({ "process_id": "sh_typo" }), &session)
            .await
            .unwrap();
        let err = result.error.unwrap();
        assert!(err.contains("sh_mine1"));
        assert!(err.contains("sh_mine2"));
        assert!(err.contains("echo one"));
        assert!(err.contains("echo two"));
        assert!(!err.contains("sh_other"), "must not leak another session's process ids");
        assert!(!err.contains("not-mine"));
        // Newest-first: sh_mine2 (spawned later) appears before sh_mine1.
        assert!(err.find("sh_mine2").unwrap() < err.find("sh_mine1").unwrap());
    }

    #[tokio::test]
    async fn shell_poll_unknown_process_id_with_no_tracked_processes_says_so() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        let poll_tool = ShellPollTool::new(registry, None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "empty-session".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = poll_tool
            .execute(json!({ "process_id": "sh_nope" }), &session)
            .await
            .unwrap();
        assert!(result.error.unwrap().contains("No tracked processes"));
    }

    #[tokio::test]
    async fn shell_poll_unknown_process_id_listing_caps_long_lists() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        {
            let mut w = registry.write().await;
            for i in 0..(UNKNOWN_PROCESS_LISTING_CAP + 5) {
                w.insert(
                    format!("sh_{i}"),
                    entry_for(&format!("sh_{i}"), "s1", "echo x", i as i64),
                );
            }
        }
        let poll_tool = ShellPollTool::new(registry, None);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = poll_tool
            .execute(json!({ "process_id": "sh_typo" }), &session)
            .await
            .unwrap();
        let err = result.error.unwrap();
        assert!(err.contains("and 5 more"));
    }

    /// issue #129: shell_kill gets the same self-correction aid as
    /// shell_poll — same wrong-id transcription failure mode.
    #[tokio::test]
    async fn shell_kill_unknown_process_id_lists_this_sessions_processes() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        registry.write().await.insert(
            "sh_alive".to_string(),
            entry_for("sh_alive", "s1", "sleep 100", 1000),
        );
        let kill_tool = ShellKillTool::new(registry);
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), reply_target: None, last_message: None, parent_session_id: None, agent_name: "main".to_string(), turn_silenced: false, turn_headless: false, channel: None };
        let result = kill_tool
            .execute(json!({ "process_id": "sh_typo" }), &session)
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("not found"));
        assert!(err.contains("sh_alive"));
    }
}
