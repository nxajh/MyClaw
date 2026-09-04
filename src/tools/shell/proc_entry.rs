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

// ── Process table ────────────────────────────────────────────────────────

/// One tracked shell invocation. Persisted as JSON at
/// `{sessions_dir}/{session}/.shell_procs/{process_id}.json` whenever
/// `sessions_dir` is configured (bare CLI usage without a session dir keeps
/// entries in-memory only — there's no daemon to restart in that mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcEntry {
    pub(crate) process_id: String,
    pub(crate) session_id: String,
    pub(crate) command: String,
    pub(crate) workdir: Option<String>,
    pub(crate) pid: i32,
    /// `/proc/{pid}/stat` field 22 (starttime, in clock ticks since boot) at
    /// spawn time — cheap sanity check against PID reuse when re-verifying
    /// liveness after a hot switch.
    pub(crate) pid_start_ticks: Option<u64>,
    pub(crate) spawned_at_ms: i64,
    pub(crate) output_path: String,
    /// "running" | "exited" | "killed" | "lost_on_restart"
    pub(crate) state: String,
    pub(crate) exit_code: Option<i32>,
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
    pub(crate) notify_on_exit: bool,
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

pub(crate) fn entry_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir
        .join(crate::ids::bare_dir_name(session_id))
        .join(".shell_procs")
}

pub(crate) fn entry_json_path(dir: &Path, process_id: &str) -> PathBuf {
    dir.join(format!("{process_id}.json"))
}

pub(crate) fn entry_output_path(dir: &Path, process_id: &str) -> PathBuf {
    dir.join(format!("{process_id}.out"))
}

pub(crate) fn write_entry_disk(sessions_dir: Option<&Path>, entry: &ProcEntry) {
    let Some(dir) = sessions_dir else { return };
    let pdir = entry_dir(dir, &entry.session_id);
    if std::fs::create_dir_all(&pdir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(entry) {
        let _ = std::fs::write(entry_json_path(&pdir, &entry.process_id), json);
    }
}

pub(crate) fn remove_entry_disk(sessions_dir: Option<&Path>, entry: &ProcEntry) {
    let Some(dir) = sessions_dir else { return };
    let pdir = entry_dir(dir, &entry.session_id);
    let _ = std::fs::remove_file(entry_json_path(&pdir, &entry.process_id));
    let _ = std::fs::remove_file(&entry.output_path);
}

/// Parse `/proc/{pid}/stat` field 22 (starttime). The comm field (2nd,
/// parenthesized) can itself contain spaces/parens, so we split on the last
/// `)` rather than whitespace.
#[cfg(target_os = "linux")]
pub(crate) fn pid_start_ticks(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pid_start_ticks(_pid: i32) -> Option<u64> {
    None
}

/// Whether `pid` is still alive and (when we have a recorded start time)
/// plausibly the same process — not a reused PID.
pub(crate) fn pid_still_this_process(pid: i32, expected_start_ticks: Option<u64>) -> bool {
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
pub(crate) async fn set_state(
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
pub(crate) async fn mark_notify_on_exit(
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
pub(crate) fn terminal_result(entry: &ProcEntry, process_id: &str, display: &str, output_truncated: bool) -> ToolResult {
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
pub(crate) fn build_completion_content(process_id: &str, exit_code: Option<i32>, output_path: &Path) -> String {
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
            // issue #228: `state` already tells us which case this is —
            // `lost_on_restart` only happens on a non-hot-switch restart
            // (`adopt_after_restart`), which reclaims the whole process
            // group, so completion is definitively ruled out, not merely
            // possible. Any other terminal state here only occurs via a
            // hot-switch adoption whose real exit could not be observed
            // (see `spawn_adopted_reaper`) — genuinely unknown, unlike the
            // lost_on_restart case.
            let cause = if e.state == "lost_on_restart" {
                "a non-hot-switch restart (the whole process group was reclaimed), confirmed to NOT have completed. If you still need this task's result, it's safe to re-run — no risk of conflicting with the old process — but confirm yourself whether the command is idempotent (writes/commits/requests as side effects) to avoid duplicating it".to_string()
            } else {
                format!(
                    "adopted across a deployment hot-switch restart; its real execution result could not be confirmed due to cross-process adoption (state: {}). Check the output below to judge success before deciding whether to re-run",
                    e.state
                )
            };
            format!(
                "[System notice] Background command ended (process_id: {}, state: {}, exit_code: {}): {}.\noutput_path: {}\n\noutput{}:\n{}",
                process_id,
                e.state,
                e.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
                cause,
                e.output_path,
                if output_truncated { " (truncated)" } else { "" },
                display
            )
        }
        None => format!(
            "[System notice] Background command status unknown (process_id: {}): the daemon restarted and this process's record is no longer available (may have been cleaned up). If you still need this task's result, please re-run it.",
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
pub(crate) fn spawn_owned_reaper(
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
pub(crate) fn spawn_adopted_reaper(
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
                sweep_if_stale(sessions_dir, &entry, now);
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

/// Remove a terminal (non-`running`) entry's on-disk files once it's older
/// than `ENTRY_RETENTION_SECS`. Shared by `adopt_after_restart`'s startup
/// sweep and `sweep_terminal_entries`'s periodic one.
fn sweep_if_stale(sessions_dir: &Path, entry: &ProcEntry, now_ms: i64) {
    if entry.state == "running" {
        return;
    }
    let age_secs = (now_ms - entry.spawned_at_ms) / 1000;
    if age_secs > ENTRY_RETENTION_SECS {
        remove_entry_disk(Some(sessions_dir), entry);
    }
}

/// issue #214: `adopt_after_restart` only sweeps `.shell_procs/` once, at
/// daemon startup — a long-lived daemon that never restarts never sweeps at
/// all, so terminal entries' `.json`/`.out` files accumulate without bound
/// (1000+ `.out` files observed over 3 days in one report). Call this
/// periodically too so retention actually holds for a daemon that stays up.
pub async fn sweep_terminal_entries(sessions_dir: &Path) {
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
            sweep_if_stale(sessions_dir, &entry, now);
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
///
/// issue #228: the entry's `state` already encodes whether the process
/// survived (`running`/`exited`, only reachable when this restart was a
/// hot-switch — see `adopt_after_restart`) or was guaranteed killed
/// alongside the daemon (`lost_on_restart`, non-hot-switch, whole cgroup
/// reclaimed) — no separate `is_hot_switch()` check is needed here, the
/// state field already IS that distinction. Returns `(message, is_error)`
/// so the caller can set the synthesized tool result's error flag from the
/// real outcome instead of hardcoding it.
pub fn latest_entry_summary(sessions_dir: &Path, session_id: &str) -> Option<(String, bool)> {
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
        // issue #230: the command text isn't included here — the only
        // caller (`run_recovery`'s Case A) already has it right next to
        // this message, in the orphan tool_call's own arguments, so
        // echoing it back is zero-information redundancy. It's also
        // LLM-generated text that would otherwise land in a
        // `[recovery]` system-semantic message unneutralized and
        // untruncated (unlike `recovery_lost_content`'s own `output`
        // field, which does go through `neutralize_spoofing`). A future
        // caller without that adjacent context can add it back — with
        // neutralize + truncation this time.
        "running" => (
            format!(
                "This shell command is still running (process_id={}) — it \
                 survived a deployment hot-switch restart and was NOT re-executed to avoid \
                 running it twice. Call sessions_yield to suspend and be woken when it \
                 finishes, shell_poll for an instant peek, or shell_kill to stop it.",
                entry.process_id
            ),
            false,
        ),
        "lost_on_restart" => (
            format!(
                "This shell command's process was terminated together with the previous \
                 daemon process (a non-hot-switch restart reclaims the whole process group) \
                 — it is confirmed to NOT have completed, and there is no captured output \
                 (process_id={}, output_path={}). If you still need this, it's \
                 safe to re-run — there is no risk of a second copy already running — just \
                 make sure re-running it is idempotent (won't duplicate side effects such as \
                 a commit or a request that may already have gone out from a prior part of \
                 the same command).",
                entry.process_id, entry.output_path
            ),
            true,
        ),
        other => (
            format!(
                "This shell command's process is no longer running (state={}, exit_code={:?}, \
                 process_id={}, output_path={}) — it was adopted across a \
                 deployment hot-switch restart, so its real exit code could not be captured. \
                 Check the output before deciding whether to re-run it.",
                other, entry.exit_code, entry.process_id, entry.output_path
            ),
            true,
        ),
    })
}

#[cfg(test)]
mod recovery_wording_tests {
    use super::*;

    fn write_entry(sessions_dir: &Path, entry: &ProcEntry) {
        let dir = entry_dir(sessions_dir, &entry.session_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            entry_json_path(&dir, &entry.process_id),
            serde_json::to_string(entry).unwrap(),
        )
        .unwrap();
    }

    fn sample_entry(
        session_id: &str,
        process_id: &str,
        state: &str,
        exit_code: Option<i32>,
    ) -> ProcEntry {
        ProcEntry {
            process_id: process_id.to_string(),
            session_id: session_id.to_string(),
            command: "myclaw update".to_string(),
            workdir: None,
            pid: 12345,
            pid_start_ticks: None,
            spawned_at_ms: now_ms(),
            output_path: "/tmp/myclaw-test-does-not-exist.out".to_string(),
            state: state.to_string(),
            exit_code,
            notify_on_exit: false,
        }
    }

    /// issue #228: `lost_on_restart` only ever occurs on a non-hot-switch
    /// restart (`adopt_after_restart` reclaims the whole cgroup instead of
    /// probing), so completion is definitively ruled out — the wording must
    /// say so plainly, not hedge with "may have completed".
    #[test]
    fn latest_entry_summary_lost_on_restart_is_definite_not_hedging() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = sample_entry("sess-1", "sh_abc", "lost_on_restart", None);
        write_entry(tmp.path(), &entry);

        let (msg, is_error) = latest_entry_summary(tmp.path(), "sess-1").unwrap();
        assert!(is_error, "lost_on_restart must be reported as an error result");
        assert!(
            !msg.to_lowercase().contains("may have"),
            "must not hedge — a non-hot-switch restart guarantees the process did not \
             survive: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("confirmed"),
            "must state definitively that it did not complete: {msg}"
        );
        assert!(
            !msg.contains("myclaw update"),
            "issue #230: must not echo the raw command text — it's redundant (already \
             visible in the adjacent orphan tool_call) and unneutralized LLM-generated \
             text: {msg}"
        );
    }

    #[test]
    fn latest_entry_summary_running_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = sample_entry("sess-2", "sh_def", "running", None);
        write_entry(tmp.path(), &entry);

        let (msg, is_error) = latest_entry_summary(tmp.path(), "sess-2").unwrap();
        assert!(!is_error, "a still-running survived process is not an error outcome");
        assert!(msg.contains("still running"));
        assert!(
            !msg.contains("myclaw update"),
            "issue #230: must not echo the raw command text: {msg}"
        );
    }

    /// An adopted orphan whose exit could not be reaped (`exited` with no
    /// exit_code) is genuinely unknown, not a confirmed non-completion —
    /// must not reuse the `lost_on_restart` wording.
    #[test]
    fn latest_entry_summary_exited_reports_uncertainty_not_definite_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = sample_entry("sess-3", "sh_ghi", "exited", None);
        write_entry(tmp.path(), &entry);

        let (msg, is_error) = latest_entry_summary(tmp.path(), "sess-3").unwrap();
        assert!(is_error);
        assert!(
            !msg.to_lowercase().contains("confirmed"),
            "an adopted-orphan exit is genuinely unknown, not a confirmed outcome: {msg}"
        );
        assert!(
            !msg.contains("myclaw update"),
            "issue #230: must not echo the raw command text: {msg}"
        );
    }

    #[tokio::test]
    async fn recovery_lost_content_distinguishes_lost_on_restart_from_exited() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        let lost = sample_entry("sess-4", "sh_lost", "lost_on_restart", None);
        registry.write().await.insert(lost.process_id.clone(), lost.clone());

        let content = recovery_lost_content(&registry, &lost.process_id).await;
        assert!(content.contains("confirmed to NOT have completed"), "got: {content}");
        assert!(
            !content.to_lowercase().contains("may have"),
            "must not hedge for a confirmed-dead entry: {content}"
        );
    }

    #[tokio::test]
    async fn recovery_lost_content_exited_keeps_genuine_uncertainty() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        let exited = sample_entry("sess-5", "sh_exited", "exited", None);
        registry.write().await.insert(exited.process_id.clone(), exited.clone());

        let content = recovery_lost_content(&registry, &exited.process_id).await;
        assert!(content.contains("hot-switch"), "got: {content}");
        assert!(
            !content.contains("confirmed to NOT have completed"),
            "an adopted-orphan exit isn't a confirmed non-completion: {content}"
        );
    }

    /// issue #230: `recovery_lost_content` and `latest_entry_summary` are
    /// the same recovery domain (both feed `run_recovery`'s synthesized
    /// tool results) and had drifted into different languages by historical
    /// accident (`latest_entry_summary` English since #216,
    /// `recovery_lost_content` Chinese since #229). Structured content fed
    /// back to the model is now unified to English; the model paraphrases
    /// to the user in their own language on its next turn either way.
    #[tokio::test]
    async fn recovery_lost_content_is_english_not_chinese() {
        let registry: ShellRegistry = Arc::new(RwLock::new(HashMap::new()));
        let lost = sample_entry("sess-6", "sh_lang", "lost_on_restart", None);
        registry.write().await.insert(lost.process_id.clone(), lost.clone());

        let content = recovery_lost_content(&registry, &lost.process_id).await;
        assert!(
            !content.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "must not contain Chinese characters: {content}"
        );
    }
}
