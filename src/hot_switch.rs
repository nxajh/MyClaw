//! Hot switch — fork + execve + SO_REUSEPORT + rollback.
//!
//! Light C (anti restart-storm) sequence:
//!
//! 1. SIGUSR1 sets the shutdown flag; old process exits the agent loop and
//!    **drains in-flight turns** so tool results can persist.
//! 2. Old process **forks** a child and **execve**s the new binary (already
//!    replaced on disk by `myclaw update`).
//! 3. New process detects `MYCLAW_HOT_SWITCH`, reuses listen sockets, binds with
//!    **SO_REUSEPORT**, writes `update-state=completed`, and sends **SIGUSR2**
//!    meaning **new process is ready** (shell/status may observe completed).
//! 4. **SIGUSR2 no longer kills the old process.** The old process finishes
//!    `waitpid` bookkeeping for failed children, then **exits itself** after
//!    drain+fork bookkeeping so history persist from the in-flight turn can
//!    complete *before* the old PID dies.
//! 5. New process **defers startup recovery** until the old PID exits (or
//!    timeout), so recovery does not re-exec incomplete `myclaw update` tools
//!    that the old process is still finishing.
//! 6. If the child crashes (execv failure), `waitpid` returns and the old
//!    process **rolls back** — clears the shutdown flag and continues running.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Environment variable indicating a hot-switch startup.
pub const ENV_HOT_SWITCH: &str = "MYCLAW_HOT_SWITCH";

/// Environment variable carrying the inherited listen socket fd.
pub const ENV_SOCKET_FD: &str = "MYCLAW_SOCKET_FD";

/// Environment variable carrying the inherited client WebSocket socket fd.
pub const ENV_CLIENT_SOCKET_FD: &str = "MYCLAW_CLIENT_SOCKET_FD";

/// Environment variable carrying the old (pre-switch) process PID.
pub const ENV_OLD_PID: &str = "MYCLAW_OLD_PID";

/// Set when the new process signals readiness (SIGUSR2). Old process may exit
/// after drain; it must **not** treat this as an immediate `exit(0)` before
/// fork bookkeeping completes.
static NEW_PROCESS_READY: AtomicBool = AtomicBool::new(false);

/// How long the new process waits for the old PID to exit before running
/// startup recovery anyway.
pub const RECOVERY_WAIT_OLD_TIMEOUT: Duration = Duration::from_secs(90);

/// Detect whether the current process was started via hot switch.
pub fn is_hot_switch() -> bool {
    std::env::var(ENV_HOT_SWITCH).is_ok()
}

/// Retrieve the inherited socket fd passed from the old process.
pub fn inherited_socket_fd() -> Option<i32> {
    std::env::var(ENV_SOCKET_FD)
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Retrieve the inherited client WebSocket socket fd passed from the old process.
pub fn inherited_client_socket_fd() -> Option<i32> {
    std::env::var(ENV_CLIENT_SOCKET_FD)
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&fd| fd >= 0)
}

/// Retrieve the old process PID passed from the fork parent.
pub fn old_pid() -> Option<i32> {
    std::env::var(ENV_OLD_PID).ok().and_then(|s| s.parse().ok())
}

/// Record that SIGUSR2 arrived (new process ready). Does **not** exit.
pub fn mark_new_process_ready() {
    NEW_PROCESS_READY.store(true, Ordering::SeqCst);
}

/// Whether the new process has signalled readiness.
pub fn is_new_process_ready() -> bool {
    NEW_PROCESS_READY.load(Ordering::SeqCst)
}

/// Return true if `pid` is still alive (same logic as kill(pid, 0)).
#[cfg(unix)]
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM means the process exists but we cannot signal it.
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: i32) -> bool {
    false
}

/// Block until `old_pid` exits or `timeout` elapses. Used by the new process
/// before startup recovery so the old process can finish persisting tool results.
pub async fn wait_for_old_process_exit(old_pid: i32, timeout: Duration) {
    if old_pid <= 1 {
        return;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut logged = false;
    loop {
        if !pid_alive(old_pid) {
            tracing::info!(old_pid, "old process exited — safe to run startup recovery");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                old_pid,
                timeout_secs = timeout.as_secs(),
                "timed out waiting for old process exit — running startup recovery anyway"
            );
            return;
        }
        if !logged {
            tracing::info!(
                old_pid,
                timeout_secs = timeout.as_secs(),
                "waiting for old process to exit before startup recovery"
            );
            logged = true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Build the envp for the hot-switch child.
///
/// Called in the parent before fork so we never touch std::env inside the
/// child — std::env uses a global mutex that may be held by another thread
/// at fork time, causing a deadlock if accessed in the single-threaded child.
fn build_child_envp(
    socket_fd: i32,
    client_fd: i32,
    current_pid: u32,
) -> anyhow::Result<Vec<CString>> {
    let mut overrides = vec![
        (ENV_HOT_SWITCH, "1".to_string()),
        (ENV_SOCKET_FD, socket_fd.to_string()),
        (ENV_OLD_PID, current_pid.to_string()),
    ];
    if client_fd >= 0 {
        overrides.push((ENV_CLIENT_SOCKET_FD, client_fd.to_string()));
    }
    let overrides = overrides;
    let override_keys: std::collections::HashSet<&str> =
        overrides.iter().map(|(k, _)| *k).collect();

    // Start from the current environment, skipping keys we will override.
    let mut envp: Vec<CString> = std::env::vars()
        .filter(|(k, _)| !override_keys.contains(k.as_str()))
        .map(|(k, v)| CString::new(format!("{}={}", k, v)))
        .collect::<Result<_, _>>()?;

    // Append the hot-switch-specific vars.
    for (k, v) in &overrides {
        envp.push(CString::new(format!("{}={}", k, v))?);
    }

    Ok(envp)
}

/// Execute the hot switch: fork + execve.
///
/// Called after the agent loop has stopped and in-flight turns have drained
/// (or drain timed out). The parent forks the child; on success it waits until
/// either the child fails (rollback) or SIGUSR2 marks readiness, then the
/// parent **returns** so the caller can `exit(0)` cleanly (after any final
/// bookkeeping). Immediate `exit` from the SIGUSR2 handler is no longer used.
pub fn do_hot_switch(socket_fd: i32, client_fd: i32) -> anyhow::Result<()> {
    let current_exe = std::env::current_exe()?;
    let current_pid = std::process::id();

    // current_exe() resolves to /proc/self/exe → may point to a renamed (.old)
    // or deleted inode.  We need to resolve to the actual binary on disk.
    let new_binary = {
        let mut s = current_exe.to_string_lossy().to_string();

        // Kernel appends " (deleted)" when the directory entry was removed.
        // Strip it first so the .old check below can work.
        if s.ends_with(" (deleted)") {
            s.truncate(s.len() - " (deleted)".len());
        }

        // myclaw update renames the running binary to .old before replacing it.
        if s.ends_with(".old") {
            s.truncate(s.len() - 4);
        }

        let path = std::path::PathBuf::from(&s);
        if path.exists() {
            path
        } else {
            // Last resort: use original path (execve will fail → rollback).
            tracing::warn!(
                original = %current_exe.display(),
                resolved = %s,
                "resolved binary path does not exist, hot switch will likely fail"
            );
            current_exe.clone()
        }
    };

    // Build argv and envp before fork — avoids touching std::env in the child.
    let c_path = CString::new(new_binary.to_string_lossy().as_bytes())?;
    let c_run = CString::new("run")?;
    let argv: [*const libc::c_char; 3] = [c_path.as_ptr(), c_run.as_ptr(), std::ptr::null()];

    let envp_strings = build_child_envp(socket_fd, client_fd, current_pid)?;
    let mut envp_ptrs: Vec<*const libc::c_char> = envp_strings.iter().map(|s| s.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    // Reset readiness flag for this switch attempt.
    NEW_PROCESS_READY.store(false, Ordering::SeqCst);

    tracing::info!(
        binary = %new_binary.display(),
        pid = current_pid,
        socket_fd,
        "starting hot switch"
    );

    // ── fork ────────────────────────────────────────────────────────────
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
    }

    // Parent: immediately tell systemd to track the child as the new main PID.
    // This MUST be sent from the current main PID (us) — systemd rejects
    // MAINPID updates from any other PID.  We send it before the child has
    // even called execve so that systemd's tracking is updated before we exit.
    if pid > 0 {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::MainPid(pid as u32)]) {
            tracing::warn!(err = %e, child_pid = pid, "sd_notify MAINPID failed");
        } else {
            tracing::debug!(
                child_pid = pid,
                "sd_notify MAINPID sent — systemd now tracks child"
            );
        }
    }

    if pid == 0 {
        // ── Child: execve the new binary ───────────────────────────────
        // Do NOT call std::env::set_var here — the env mutex may be held
        // by a parent thread at fork time, causing a deadlock.  All env
        // vars were built in the parent above and passed via execve envp.
        unsafe { libc::execve(c_path.as_ptr(), argv.as_ptr(), envp_ptrs.as_ptr()) };

        // execve only returns on failure.
        eprintln!("execve failed: {}", std::io::Error::last_os_error());
        unsafe { libc::_exit(1) };
    }

    // ── Parent (old process): wait for child readiness or failure ─────
    tracing::info!(
        child_pid = pid,
        "forked child, waiting for new-process readiness (SIGUSR2) or child exit"
    );

    // Poll: SIGUSR2 handler sets NEW_PROCESS_READY; failed child makes waitpid
    // return. Use WNOHANG so we can also observe the ready flag without
    // requiring the SIGUSR2 handler to exit(0).
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        if is_new_process_ready() {
            tracing::info!(
                child_pid = pid,
                "new process ready (SIGUSR2) — old process will exit after return"
            );
            // Do not waitpid(blocking): child is the new long-running daemon.
            // Detach via a non-blocking check only.
            let mut status: libc::c_int = 0;
            let _ = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            return Ok(());
        }

        let mut status: libc::c_int = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            // ECHILD: already reaped
            if err.raw_os_error() == Some(libc::ECHILD) {
                if is_new_process_ready() {
                    return Ok(());
                }
                tracing::warn!("waitpid ECHILD before ready flag — treating as failure");
                break;
            }
            anyhow::bail!("waitpid failed: {}", err);
        }
        if result > 0 {
            let child_exited_with_error = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0;
            let child_killed_by_signal = libc::WIFSIGNALED(status);
            let child_exited_ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;

            if child_exited_with_error || child_killed_by_signal {
                if child_killed_by_signal {
                    tracing::error!(
                        signo = libc::WTERMSIG(status),
                        "child process killed by signal, hot switch failed — rolling back"
                    );
                } else {
                    tracing::error!(
                        exit_code = libc::WEXITSTATUS(status),
                        "child process exited with error, hot switch failed — rolling back"
                    );
                }

                crate::SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
                tracing::info!("shutdown flag cleared, daemon continues running");

                if let Err(e) = sd_notify::notify(
                    false,
                    &[sd_notify::NotifyState::MainPid(std::process::id())],
                ) {
                    tracing::warn!(err = %e, "sd_notify MAINPID rollback failed");
                } else {
                    tracing::debug!("sd_notify MAINPID restored to self on rollback");
                }

                return Err(anyhow::anyhow!("hot switch failed, daemon continues"));
            }

            if child_exited_ok {
                // Unusual: child exited 0 without staying up.
                tracing::info!("child process exited normally");
                return Ok(());
            }
        }

        if std::time::Instant::now() >= deadline {
            tracing::error!(
                child_pid = pid,
                "timed out waiting for new process readiness — rolling back"
            );
            crate::SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
            let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
            if let Err(e) =
                sd_notify::notify(false, &[sd_notify::NotifyState::MainPid(std::process::id())])
            {
                tracing::warn!(err = %e, "sd_notify MAINPID rollback failed");
            }
            return Err(anyhow::anyhow!(
                "hot switch timed out waiting for new process"
            ));
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    crate::SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
    Err(anyhow::anyhow!("hot switch failed, daemon continues"))
}
