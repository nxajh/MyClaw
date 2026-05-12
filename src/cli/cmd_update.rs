//! `myclaw update` — download latest artifact from GitHub and trigger hot switch.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Execute `myclaw update`.
pub fn run_update() -> Result<()> {
    // 1. Get current binary path
    let current_exe = std::env::current_exe()
        .context("failed to determine current executable path")?;

    // 2. Get latest successful run ID
    println!("Checking for updates...");
    let run_id = get_latest_run_id()?;

    // 3. Download artifact to temp directory
    let tmp_dir = std::env::temp_dir().join("myclaw-update");
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    std::fs::create_dir_all(&tmp_dir)?;

    println!("Downloading from run #{}...", run_id);
    download_artifact(&run_id, &tmp_dir)?;

    // 4. Verify downloaded binary
    let new_binary = tmp_dir.join("myclaw");
    if !new_binary.exists() {
        anyhow::bail!("downloaded artifact does not contain 'myclaw' binary");
    }

    // 5. Rename old binary out of the way (rename works on running binaries — inode kept by process)
    let old_binary = current_exe.with_extension("old");
    if old_binary.exists() {
        std::fs::remove_file(&old_binary)?;
    }
    std::fs::rename(&current_exe, &old_binary)
        .context("failed to rename current binary")?;

    // 6. Move new binary into place (rename avoids "Text file busy" on running binaries)
    std::fs::rename(&new_binary, &current_exe)
        .or_else(|_| {
            // Fallback: copy if cross-filesystem rename fails
            std::fs::copy(&new_binary, &current_exe)?;
            std::fs::remove_file(&new_binary).ok();
            Ok::<(), std::io::Error>(())
        })
        .context("failed to replace binary")?;

    // Set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &current_exe,
            std::fs::Permissions::from_mode(0o755),
        )?;
    }

    // 7. Clean up temp files
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // 8. Send SIGUSR1 to the running daemon
    send_sigusr1()?;

    println!("Updated to run #{}. Hot switch scheduled.", run_id);
    Ok(())
}

/// Query `gh` CLI for the latest successful master workflow run ID.
fn get_latest_run_id() -> Result<String> {
    let output = std::process::Command::new("gh")
        .args([
            "run",
            "list",
            "--workflow=build.yml",
            "--branch=master",
            "--status=success",
            "--limit=1",
            "--json",
            "databaseId",
            "-q",
            ".[0].databaseId",
        ])
        .current_dir(find_project_dir()?)
        .output()
        .context("failed to execute 'gh' — is it installed and authenticated?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh run list failed: {}", stderr);
    }

    let id = String::from_utf8(output.stdout)?.trim().to_string();
    if id.is_empty() || id == "null" {
        anyhow::bail!("no successful builds found on master");
    }
    Ok(id)
}

/// Download the `myclaw-linux-x86_64` artifact from a given workflow run.
fn download_artifact(run_id: &str, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("gh")
        .args([
            "run",
            "download",
            run_id,
            "--name",
            "myclaw-linux-x86_64",
            "--dir",
            dest.to_str().context("temp dir path is not valid UTF-8")?,
        ])
        .current_dir(find_project_dir()?)
        .status()
        .context("failed to download artifact")?;

    if !status.success() {
        anyhow::bail!("gh run download failed");
    }
    Ok(())
}

/// Determine the MyClaw project root directory (needed by `gh` which requires
/// being inside a git repository). Falls back to CWD.
fn find_project_dir() -> Result<PathBuf> {
    // Walk up from CWD looking for .git
    let mut dir = std::env::current_dir().context("failed to get current directory")?;
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    // Fallback: CWD
    std::env::current_dir().context("failed to get current directory")
}

/// Send SIGUSR1 to the running myclaw daemon to trigger hot switch.
fn send_sigusr1() -> Result<()> {
    // Try PID file first
    let pid_file = PathBuf::from("/tmp/myclaw.pid");
    if pid_file.exists() {
        let pid_str = std::fs::read_to_string(&pid_file)?;
        let pid: i32 = pid_str
            .trim()
            .parse()
            .context("invalid PID in pid file")?;
        // SAFETY: libc::kill is a simple syscall wrapper.
        let ret = unsafe { libc::kill(pid, libc::SIGUSR1) };
        if ret == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        tracing::warn!(
            pid,
            error = %err,
            "failed to send SIGUSR1 via PID file, falling back to pgrep"
        );
    }

    // Fallback: use pgrep to find the daemon
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

    let ret = unsafe { libc::kill(pid, libc::SIGUSR1) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to send SIGUSR1 to PID {}: {}", pid, err);
    }

    Ok(())
}
