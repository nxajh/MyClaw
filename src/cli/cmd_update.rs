//! `myclaw update` — download latest artifact from GitHub and trigger hot switch.
//!
//! Writes `~/.myclaw/update-state.json` so success can be verified after the
//! current turn is interrupted by hot-switch. SIGUSR1 is delayed briefly so
//! shell tools can flush stdout back to the agent before the daemon drains.
//!
//! Contract (B, anti restart-storm):
//! - On success: stage binary → flush stdout → short delay → SIGUSR1 → **exit 0**.
//! - Do **not** wait for `UPDATE_STATUS=completed` in the same process/shell.
//! - Idempotent: if the installed binary already matches the latest artifact
//!   (or update-state already completed/switching for that run), print status
//!   and exit without sending another SIGUSR1.

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
    // Prefer the live install path (may resolve to .old while daemon holds the inode).
    let install_path = resolve_install_path(&current_exe);
    let binary_path = install_path.display().to_string();

    let old_pid = myclaw::signal::find_daemon_pid().ok().map(|p| p as u32);

    // 2. Get latest successful run ID (+ commit when available)
    println!("Checking for updates...");
    let (run_id, commit) = get_latest_run_meta()?;

    // 3. Idempotent short-circuit before download when state + on-disk sha already match.
    if let Some(msg) = already_up_to_date_message(&run_id, commit.as_deref(), &install_path)? {
        print_noop(&msg, &run_id, commit.as_deref(), old_pid, &install_path);
        return Ok(());
    }

    // 4. Download artifact to temp directory
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

    // 5. Verify downloaded binary
    let new_binary = find_downloaded_binary(&tmp_dir)?;
    let binary_sha256 = file_sha256_hex(&new_binary)?;

    // 6. If installed binary already has the same content, skip replace.
    //    - completed/unknown: noop (idempotent)
    //    - staged/switching interrupted: re-send SIGUSR1 without re-download noise
    if install_path.exists() {
        if let Ok(installed) = file_sha256_hex(&install_path) {
            if installed == binary_sha256 {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                let prior = UpdateState::load().ok().flatten();
                // Only re-signal when staged (binary replaced, switch not started).
                // If already switching, a concurrent switch is in flight — do not
                // fire another SIGUSR1 (restart-storm fuel).
                let needs_signal = prior
                    .as_ref()
                    .map(|s| {
                        matches!(s.status, UpdateStatus::Staged)
                            && s.run_id.as_deref() == Some(run_id.as_str())
                    })
                    .unwrap_or(false);

                if !needs_signal {
                    // Do not clobber an in-flight `switching` record; only
                    // refresh completed when we are truly already done / idle.
                    let prior_switching = prior
                        .as_ref()
                        .map(|s| matches!(s.status, UpdateStatus::Switching))
                        .unwrap_or(false);
                    if !prior_switching {
                        let state = UpdateState {
                            status: UpdateStatus::Completed,
                            run_id: Some(run_id.clone()),
                            commit: commit.clone(),
                            binary_path: Some(binary_path.clone()),
                            binary_sha256: Some(installed.clone()),
                            old_pid,
                            new_pid: old_pid,
                            error: None,
                            updated_at: UpdateState::now_rfc3339(),
                        };
                        let _ = state.save();
                    }
                    let reason = if prior_switching {
                        "already_switching"
                    } else {
                        "already_current_sha"
                    };
                    print_noop(
                        reason,
                        &run_id,
                        commit.as_deref(),
                        old_pid,
                        &install_path,
                    );
                    println!("binary_sha256={installed}");
                    return Ok(());
                }

                // Same binary already staged — re-signal only.
                let staged = UpdateState {
                    status: UpdateStatus::Staged,
                    run_id: Some(run_id.clone()),
                    commit: commit.clone(),
                    binary_path: Some(binary_path.clone()),
                    binary_sha256: Some(installed.clone()),
                    old_pid,
                    new_pid: None,
                    error: None,
                    updated_at: UpdateState::now_rfc3339(),
                };
                staged.save()?;
                println!("UPDATE_STATUS=staged");
                println!("run_id={run_id}");
                if let Some(ref sha) = commit {
                    println!("commit={sha}");
                }
                println!("binary_path={binary_path}");
                println!("binary_sha256={installed}");
                println!(
                    "message=Binary already staged. Re-sending SIGUSR1 for hot switch."
                );
                println!(
                    "hint=Do not wait for UPDATE_STATUS=completed in this process. Exit after SIGUSR1; check `myclaw status` in a later turn."
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::thread::sleep(SIGUSR1_DELAY);
                let mut switching = staged;
                switching.status = UpdateStatus::Switching;
                switching.updated_at = UpdateState::now_rfc3339();
                let _ = switching.save();
                send_sigusr1()?;
                println!("UPDATE_STATUS=switching");
                println!("message=SIGUSR1 sent. Exiting; hot switch proceeds asynchronously.");
                let _ = std::io::stdout().flush();
                return Ok(());
            }
        }
    }

    // 7. Rename old binary out of the way (rename works on running binaries — inode kept by process)
    let old_binary = install_path.with_extension("old");
    if old_binary.exists() {
        std::fs::remove_file(&old_binary)?;
    }
    // current_exe may be the .old inode path; always operate on install_path for replace.
    if install_path.exists() {
        std::fs::rename(&install_path, &old_binary)
            .context("failed to rename current binary")?;
    } else if current_exe.exists() {
        // Fallback: rename whatever path we are running from.
        std::fs::rename(&current_exe, &old_binary)
            .context("failed to rename current binary")?;
    }

    // 8. Move new binary into place (rename avoids "Text file busy" on running binaries)
    std::fs::rename(&new_binary, &install_path)
        .or_else(|_| {
            // Fallback: copy if cross-filesystem rename fails
            std::fs::copy(&new_binary, &install_path)?;
            std::fs::remove_file(&new_binary).ok();
            Ok::<(), std::io::Error>(())
        })
        .context("failed to replace binary")?;

    // Set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&install_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Re-hash the installed binary (same content, path is the live path).
    let installed_sha = file_sha256_hex(&install_path).unwrap_or(binary_sha256.clone());

    // 9. Clean up temp files
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // 10. Persist staged state BEFORE signaling, so it survives turn interruption.
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

    // 11. Print machine- and human-readable result, then flush.
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
    println!(
        "hint=Do not wait for UPDATE_STATUS=completed in this process. Exit after SIGUSR1; check `myclaw status` in a later turn."
    );
    println!("Updated to run #{run_id}. Hot switch scheduled.");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // 12. Delay SIGUSR1 so the shell tool can capture stdout before drain.
    std::thread::sleep(SIGUSR1_DELAY);

    // Mark switching, then signal.
    let mut switching = staged;
    switching.status = UpdateStatus::Switching;
    switching.updated_at = UpdateState::now_rfc3339();
    let _ = switching.save();

    send_sigusr1()?;
    // Exit immediately — do not poll for completed (deadlocks with drain).
    println!("UPDATE_STATUS=switching");
    println!("message=SIGUSR1 sent. Exiting; hot switch proceeds asynchronously.");
    let _ = std::io::stdout().flush();
    Ok(())
}

/// Prefer the non-.old install path for replace/idempotency checks.
fn resolve_install_path(current_exe: &Path) -> PathBuf {
    let mut s = current_exe.to_string_lossy().to_string();
    if s.ends_with(" (deleted)") {
        s.truncate(s.len() - " (deleted)".len());
    }
    if s.ends_with(".old") {
        s.truncate(s.len() - 4);
    }
    PathBuf::from(s)
}

/// If update-state already says this run is done/in-flight, skip work.
fn already_up_to_date_message(
    run_id: &str,
    commit: Option<&str>,
    install_path: &Path,
) -> Result<Option<String>> {
    let Some(state) = UpdateState::load()? else {
        return Ok(None);
    };

    let same_run = state.run_id.as_deref() == Some(run_id);
    let same_commit = match (commit, state.commit.as_deref()) {
        (Some(a), Some(b)) => a == b || a.starts_with(b) || b.starts_with(a),
        _ => true, // if either side missing commit, don't block on it
    };

    match state.status {
        UpdateStatus::Completed if same_run && same_commit => {
            // Confirm on-disk binary still matches recorded sha when available.
            if let (Some(expected), Ok(actual)) = (
                state.binary_sha256.as_deref(),
                file_sha256_hex(install_path),
            ) {
                if expected == actual {
                    return Ok(Some("already_completed".into()));
                }
            } else if state.binary_sha256.is_none() {
                return Ok(Some("already_completed".into()));
            }
            Ok(None)
        }
        UpdateStatus::Switching if same_run && same_commit => {
            // Hot switch already in progress for this run — do not re-signal.
            Ok(Some("already_switching".into()))
        }
        // Staged for same run: binary is on disk but switch may not have started.
        // Fall through so caller can re-send SIGUSR1 (still after replace skip).
        UpdateStatus::Staged => Ok(None),
        _ => Ok(None),
    }
}

fn print_noop(
    reason: &str,
    run_id: &str,
    commit: Option<&str>,
    old_pid: Option<u32>,
    install_path: &Path,
) {
    println!("UPDATE_STATUS=noop");
    println!("noop_reason={reason}");
    println!("run_id={run_id}");
    if let Some(sha) = commit {
        println!("commit={sha}");
    }
    println!("binary_path={}", install_path.display());
    if let Some(pid) = old_pid {
        println!("old_pid={pid}");
    }
    println!("message=Already up to date (or switch in progress). No SIGUSR1 sent.");
    println!(
        "hint=Do not wait for UPDATE_STATUS=completed in this process. Use `myclaw status` in a later turn if needed."
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();
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
