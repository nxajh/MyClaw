//! `myclaw update` — download latest artifact from GitHub and trigger hot switch.
//!
//! Writes `~/.myclaw/update-state.json` so success can be verified after the
//! current turn is interrupted by hot-switch. SIGUSR1 is delayed briefly so
//! shell tools can flush stdout back to the agent before the daemon drains.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use myclaw::update_state::{file_sha256_hex, UpdateState, UpdateStatus};

/// How long to wait after printing success before SIGUSR1, so the shell tool
/// can capture stdout before the daemon starts draining the active turn.
const SIGUSR1_DELAY: Duration = Duration::from_millis(400);

/// Execute `myclaw update`.
pub fn run_update() -> Result<()> {
    match run_update_inner() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = UpdateState::mark_failed(e.to_string());
            Err(e)
        }
    }
}

fn run_update_inner() -> Result<()> {
    // 1. Get current binary path
    let current_exe =
        std::env::current_exe().context("failed to determine current executable path")?;
    let binary_path = current_exe.display().to_string();

    let old_pid = myclaw::signal::find_daemon_pid().ok().map(|p| p as u32);

    // 2. Get latest successful run ID (+ commit when available)
    println!("Checking for updates...");
    let (run_id, commit) = get_latest_run_meta()?;

    // 3. Download artifact to temp directory
    let tmp_dir = std::env::temp_dir().join("myclaw-update");
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    std::fs::create_dir_all(&tmp_dir)?;

    println!("Downloading from run #{}...", run_id);
    if let Some(ref sha) = commit {
        println!("commit={}", sha);
    }
    download_artifact(&run_id, &tmp_dir)?;

    // 4. Verify downloaded binary
    let new_binary = find_downloaded_binary(&tmp_dir)?;
    let binary_sha256 = file_sha256_hex(&new_binary)?;

    // 5. Rename old binary out of the way (rename works on running binaries — inode kept by process)
    let old_binary = current_exe.with_extension("old");
    if old_binary.exists() {
        std::fs::remove_file(&old_binary)?;
    }
    std::fs::rename(&current_exe, &old_binary).context("failed to rename current binary")?;

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
        std::fs::set_permissions(&current_exe, std::fs::Permissions::from_mode(0o755))?;
    }

    // Re-hash the installed binary (same content, path is the live path).
    let installed_sha = file_sha256_hex(&current_exe).unwrap_or(binary_sha256.clone());

    // 7. Clean up temp files
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // 8. Persist staged state BEFORE signaling, so it survives turn interruption.
    let staged = UpdateState {
        status: UpdateStatus::Staged,
        run_id: Some(run_id.clone()),
        commit: commit.clone(),
        binary_path: Some(binary_path.clone()),
        binary_sha256: Some(installed_sha.clone()),
        old_pid,
        new_pid: None,
        error: None,
        updated_at: UpdateState::now_rfc3339(),
    };
    staged.save()?;

    // 9. Print machine- and human-readable result, then flush.
    println!("UPDATE_STATUS=staged");
    println!("run_id={run_id}");
    if let Some(ref sha) = commit {
        println!("commit={sha}");
    }
    println!("binary_path={binary_path}");
    println!("binary_sha256={installed_sha}");
    if let Some(pid) = old_pid {
        println!("old_pid={pid}");
    }
    println!("message=Binary replaced. Hot switch will start after a short delay.");
    println!("Updated to run #{run_id}. Hot switch scheduled.");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // 10. Delay SIGUSR1 so the shell tool can capture stdout before drain.
    std::thread::sleep(SIGUSR1_DELAY);

    // Mark switching, then signal.
    let mut switching = staged;
    switching.status = UpdateStatus::Switching;
    switching.updated_at = UpdateState::now_rfc3339();
    let _ = switching.save();

    send_sigusr1()?;
    Ok(())
}

/// Prefer artifact layout `myclaw` at dest root; also accept nested CI folder.
fn find_downloaded_binary(dest: &Path) -> Result<PathBuf> {
    let candidates = [
        dest.join("myclaw"),
        dest.join("myclaw-linux-x86_64").join("myclaw"),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    // Shallow walk one level for a file named myclaw.
    if let Ok(entries) = std::fs::read_dir(dest) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let nested = p.join("myclaw");
                if nested.exists() {
                    return Ok(nested);
                }
            }
        }
    }
    anyhow::bail!("downloaded artifact does not contain 'myclaw' binary");
}

const GH_REPO: &str = "nxajh/MyClaw";

/// Query `gh` for latest successful master build: (databaseId, headSha?).
fn get_latest_run_meta() -> Result<(String, Option<String>)> {
    let output = std::process::Command::new("gh")
        .args([
            "run",
            "list",
            "--workflow=build.yml",
            "--branch=master",
            "--status=success",
            "--limit=1",
            "--repo",
            GH_REPO,
            "--json",
            "databaseId,headSha",
            "-q",
            ".[0] | \"\\(.databaseId)\\t\\(.headSha // \"\")\"",
        ])
        .output()
        .context("failed to execute 'gh' — is it installed and authenticated?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh run list failed: {}", stderr);
    }

    let line = String::from_utf8(output.stdout)?.trim().to_string();
    if line.is_empty() || line == "null" {
        anyhow::bail!("no successful builds found on master");
    }
    let mut parts = line.split('\t');
    let id = parts.next().unwrap_or("").trim().to_string();
    if id.is_empty() || id == "null" {
        anyhow::bail!("no successful builds found on master");
    }
    let sha = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.len() > 7 {
                s[..7].to_string()
            } else {
                s.to_string()
            }
        });
    Ok((id, sha))
}

/// Download the `myclaw-linux-x86_64` artifact from a given workflow run.
fn download_artifact(run_id: &str, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("gh")
        .args([
            "run",
            "download",
            run_id,
            "--repo",
            GH_REPO,
            "--name",
            "myclaw-linux-x86_64",
            "--dir",
            dest.to_str().context("temp dir path is not valid UTF-8")?,
        ])
        .status()
        .context("failed to download artifact")?;

    if !status.success() {
        anyhow::bail!("gh run download failed");
    }
    Ok(())
}

/// Send SIGUSR1 to the running myclaw daemon to trigger hot switch.
fn send_sigusr1() -> Result<()> {
    super::signal::send_sigusr1()
}
