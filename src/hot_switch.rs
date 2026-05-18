//! Hot switch — fork + execve + SO_REUSEPORT + rollback.
//!
//! When SIGUSR1 sets the shutdown flag, the old process waits at the nearest
//! checkpoint.  After the agent loop stops, `do_hot_switch` is called:
//!
//! 1. **fork** a child process
//! 2. Child calls **execve** on the current binary (which has been replaced on
//!    disk by `myclaw update`)
//! 3. New process detects `MYCLAW_HOT_SWITCH`, binds the listen port with
//!    **SO_REUSEPORT**, and sends **SIGUSR2** to the old process
//! 4. Old process receives SIGUSR2 → `exit(0)`
//! 5. If the child crashes (execv failure), `waitpid` returns and the old
//!    process **rolls back** — clears the shutdown flag and continues running.

use std::ffi::CString;
use std::sync::atomic::Ordering;

/// Environment variable indicating a hot-switch startup.
pub const ENV_HOT_SWITCH: &str = "MYCLAW_HOT_SWITCH";

/// Environment variable carrying the inherited listen socket fd.
pub const ENV_SOCKET_FD: &str = "MYCLAW_SOCKET_FD";

/// Environment variable carrying the old (pre-switch) process PID.
pub const ENV_OLD_PID: &str = "MYCLAW_OLD_PID";

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

/// Retrieve the old process PID passed from the fork parent.
pub fn old_pid() -> Option<i32> {
    std::env::var(ENV_OLD_PID)
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Build the envp for the hot-switch child.
///
/// Called in the parent before fork so we never touch std::env inside the
/// child — std::env uses a global mutex that may be held by another thread
/// at fork time, causing a deadlock if accessed in the single-threaded child.
fn build_child_envp(socket_fd: i32, current_pid: u32) -> anyhow::Result<Vec<CString>> {
    let overrides = [
        (ENV_HOT_SWITCH, "1".to_string()),
        (ENV_SOCKET_FD, socket_fd.to_string()),
        (ENV_OLD_PID, current_pid.to_string()),
    ];
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
/// Called after the agent loop has fully stopped (checkpoint exit).
/// The parent blocks on `waitpid`; if the child execs successfully, the parent
/// will receive SIGUSR2 (from the new process) and `exit(0)` before `waitpid`
/// ever returns.  If the child crashes, `waitpid` returns with a non-zero exit
/// code and we roll back (clear the shutdown flag so the daemon keeps running).
pub fn do_hot_switch(socket_fd: i32) -> anyhow::Result<()> {
    let current_exe = std::env::current_exe()?;
    let current_pid = std::process::id();

    // current_exe() resolves to /proc/self/exe → the old inode (renamed to .old).
    // Strip .old suffix to get the new binary path.
    let new_binary = {
        let s = current_exe.to_string_lossy().to_string();
        if s.ends_with(".old") {
            std::path::PathBuf::from(&s[..s.len() - 4])
        } else {
            current_exe.clone()
        }
    };

    // Build argv and envp before fork — avoids touching std::env in the child.
    let c_path = CString::new(new_binary.to_string_lossy().as_bytes())?;
    let c_run = CString::new("run")?;
    let argv: [*const libc::c_char; 3] = [c_path.as_ptr(), c_run.as_ptr(), std::ptr::null()];

    let envp_strings = build_child_envp(socket_fd, current_pid)?;
    let mut envp_ptrs: Vec<*const libc::c_char> =
        envp_strings.iter().map(|s| s.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

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
            tracing::debug!(child_pid = pid, "sd_notify MAINPID sent — systemd now tracks child");
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

    // ── Parent (old process): wait for child outcome ───────────────────
    tracing::info!(child_pid = pid, "forked child, waiting for SIGUSR2 or child exit");

    // Block until either:
    //   • SIGUSR2 arrives (new process ready) → SIGUSR2 handler calls exit(0)
    //   • Child exits (execve failure) → waitpid returns
    let mut status: libc::c_int = 0;
    let result = unsafe { libc::waitpid(pid, &mut status, 0) };

    if result > 0 && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0 {
        // Child crashed — roll back.
        tracing::error!(
            exit_code = libc::WEXITSTATUS(status),
            "child process exited with error, hot switch failed — rolling back"
        );
        crate::SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        tracing::info!("shutdown flag cleared, daemon continues running");
        return Err(anyhow::anyhow!("hot switch failed, daemon continues"));
    }

    // Child exited normally (unlikely — usually execve replaces the process image).
    tracing::info!("child process exited normally");
    Ok(())
}
