//! Hot switch — fork + execv + SO_REUSEPORT + rollback.
//!
//! When SIGUSR1 sets the shutdown flag, the old process waits at the nearest
//! checkpoint.  After the agent loop stops, `do_hot_switch` is called:
//!
//! 1. **fork** a child process
//! 2. Child calls **execv** on the current binary (which has been replaced on
//!    disk by `myclaw update`)
//! 3. New process detects `MYCLAW_HOT_SWITCH`, binds the listen port with
//!    **SO_REUSEPORT**, and sends **SIGUSR2** to the old process
//! 4. Old process receives SIGUSR2 → `exit(0)`
//! 5. If the child crashes (execv failure), `waitpid` returns and the old
//!    process **rolls back** — clears the shutdown flag and continues running.

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

/// Execute the hot switch: fork + execv.
///
/// Called after the agent loop has fully stopped (checkpoint exit).
/// The parent blocks on `waitpid`; if the child execs successfully, the parent
/// will receive SIGUSR2 (from the new process) and `exit(0)` before `waitpid`
/// ever returns.  If the child crashes, `waitpid` returns with a non-zero exit
/// code and we roll back (clear the shutdown flag so the daemon keeps running).
pub fn do_hot_switch(socket_fd: i32) -> anyhow::Result<()> {
    let current_exe = std::env::current_exe()?;
    let current_pid = std::process::id();

    tracing::info!(
        binary = %current_exe.display(),
        pid = current_pid,
        socket_fd,
        "starting hot switch"
    );

    // ── fork ────────────────────────────────────────────────────────────
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
    }

    if pid == 0 {
        // ── Child: execv the new binary ────────────────────────────────
        // Set environment variables so the new process knows this is a
        // hot-switch startup and which socket / PID to work with.
        // SAFETY: child process is single-threaded after fork, so set_var is safe.
        unsafe {
            std::env::set_var(ENV_HOT_SWITCH, "1");
            std::env::set_var(ENV_SOCKET_FD, socket_fd.to_string());
            std::env::set_var(ENV_OLD_PID, current_pid.to_string());
        }

        // current_exe() resolves to /proc/self/exe which is the old inode (now renamed to .old).
        // Strip .old suffix to get the new binary path.
        let new_binary = {
            let s = current_exe.to_string_lossy().to_string();
            if s.ends_with(".old") {
                std::path::PathBuf::from(&s[..s.len() - 4])
            } else {
                current_exe.clone()
            }
        };
        let c_path = std::ffi::CString::new(new_binary.to_string_lossy().as_bytes())?;
        let c_run = std::ffi::CString::new("run")?;
        let args = [c_path.as_ptr(), c_run.as_ptr(), std::ptr::null()];

        // execv replaces the current process — only returns on failure.
        unsafe { libc::execv(c_path.as_ptr(), args.as_ptr()) };

        // execv failed — nothing we can recover from.
        eprintln!("execv failed: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }

    // ── Parent (old process): wait for child outcome ───────────────────
    tracing::info!(child_pid = pid, "forked child, waiting for SIGUSR2 or child exit");

    // Block until either:
    //   • SIGUSR2 arrives (new process ready) → SIGUSR2 handler calls exit(0)
    //   • Child exits (execv failure) → waitpid returns
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

    // Child exited normally (unlikely path — usually execv replaces it).
    tracing::info!("child process exited normally");
    Ok(())
}
