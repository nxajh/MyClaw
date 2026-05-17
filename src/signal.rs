//! Signal utilities — send signals to the running myclaw daemon.
//!
//! Shared between CLI subcommands and the self-management tool.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Canonical path for the daemon PID file.
///
/// Stored under `~/.myclaw/` so both the daemon (which may run with
/// `PrivateTmp=true`) and CLI subcommands (running outside the service
/// namespace) can always find it.
pub fn pid_file_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".myclaw/myclaw.pid"))
        .unwrap_or_else(|_| std::env::temp_dir().join("myclaw.pid"))
}

/// Find the daemon PID from the PID file, or fallback to pgrep.
pub fn find_daemon_pid() -> Result<i32> {
    let pid_file = pid_file_path();
    if pid_file.exists() {
        let pid_str = std::fs::read_to_string(&pid_file)?;
        let pid: i32 = pid_str
            .trim()
            .parse()
            .context("invalid PID in pid file")?;
        return Ok(pid);
    }

    // Fallback: pgrep
    let output = std::process::Command::new("pgrep")
        .args(["-x", "myclaw"])
        .output()
        .context("failed to execute pgrep")?;

    if !output.status.success() {
        anyhow::bail!("no running myclaw daemon found");
    }

    let pids = String::from_utf8(output.stdout)?;
    let pid: i32 = pids
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no PID found"))?
        .trim()
        .parse()?;

    Ok(pid)
}

/// Send a signal to the running myclaw daemon.
pub fn send_signal(sig: i32) -> Result<()> {
    let pid = find_daemon_pid()?;
    let ret = unsafe { libc::kill(pid, sig) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to send signal {} to PID {}: {}", sig, pid, err);
    }
    Ok(())
}

/// Send SIGHUP to the daemon (config hot-reload).
pub fn send_sighup() -> Result<()> {
    send_signal(libc::SIGHUP)
}

/// Send SIGTERM to the daemon (graceful stop).
pub fn send_sigterm() -> Result<()> {
    send_signal(libc::SIGTERM)
}

/// Send SIGUSR1 to the daemon (hot switch / restart).
pub fn send_sigusr1() -> Result<()> {
    send_signal(libc::SIGUSR1)
}
