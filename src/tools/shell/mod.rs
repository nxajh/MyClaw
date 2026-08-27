#![allow(unused_imports)]
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

pub(crate) mod proc_entry;
pub(crate) mod shell_tool;
pub(crate) mod poll_tool;
pub(crate) mod kill_tool;

pub use proc_entry::{ProcEntry, ProcSummary, ShellRegistry, is_shell_process_id, recovery_lost_content, adopt_after_restart, kill_processes_for_session, latest_entry_summary};
pub use shell_tool::ShellTool;
pub(crate) use shell_tool::format_unknown_process_listing;
pub(crate) use proc_entry::*;
pub use poll_tool::ShellPollTool;
pub use kill_tool::ShellKillTool;

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

#[cfg(test)]
mod tests {
    use super::*;
    use super::shell_tool::UNKNOWN_PROCESS_LISTING_CAP;

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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let sm = Arc::new(crate::api::session_store::InMemorySessionStore::new());
        let sctx = sm.get_or_create_context("test:routing:key");
        let sid = sctx.session_id.clone();
        assert!(!sctx.has_pending_async_work());

        let tool = ShellTool::new(None);
        tool.set_session_manager(Arc::clone(&sm));
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: sid.clone(), agent_name: "main".to_string(), ..Default::default() };
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
        let sm = Arc::new(crate::api::session_store::InMemorySessionStore::new());
        let sctx = sm.get_or_create_context("test:routing:key2");
        let sid = sctx.session_id.clone();

        let tool = ShellTool::new(None);
        tool.set_session_manager(Arc::clone(&sm));
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: sid.clone(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session_a = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "session-a".to_string(), agent_name: "main".to_string(), ..Default::default() };
        let session_b = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "session-b".to_string(), agent_name: "main".to_string(), ..Default::default() };

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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "empty-session".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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
        let session = crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "s1".to_string(), agent_name: "main".to_string(), ..Default::default() };
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

#[cfg(test)]
pub(crate) use poll_tool::test_proc_entry;
