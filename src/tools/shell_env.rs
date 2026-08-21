//! Tool-shell environment fix-ups (issue #84).
//!
//! `spawn_tracked` (`tools/shell.rs`) runs `sh -c <command>` inheriting the
//! daemon's own process environment. The daemon is normally started by a
//! systemd user unit with no login/interactive shell in its ancestry, so
//! PATH extensions a user's `.bashrc`/`.zshrc` appends — npm's global
//! prefix, nvm, pyenv, Homebrew, `~/.local/bin`, … — are invisible to it.
//! A CLI installed successfully by the user is then unreachable to the
//! agent's own shell tool.
//!
//! Three layers, applied in increasing priority (each only *adds to* or
//! *overrides* specific keys on top of the inherited environment — never
//! clears it):
//!
//! 1. **Static fallback** (always on): the standard system directories
//!    plus MyClaw's own binary directory, appended to whatever PATH the
//!    daemon already has. Zero cost, no configuration, catches the "PATH
//!    is nearly empty because systemd gave us a minimal one" case.
//! 2. **Login-shell probe** (`[shell] login_env_probe`, default on): runs
//!    `$SHELL -lic 'env -0'` once (interactive, not just login — see
//!    `probe_login_shell_env` for why plain `-l -c` misses the exact
//!    rc-file PATH exports this exists to find), so nvm/pyenv/npm-global/
//!    Homebrew paths set in the user's rc files show up too. Guarded the
//!    way OpenClaw does it: `$SHELL` must be an absolute path listed in
//!    `/etc/shells`, the probe has a hard timeout and output cap, and any
//!    failure just degrades to layer 1 (a warning is logged once, nothing
//!    blocks). Gap-only: a key the daemon's own environment already has
//!    is never overwritten by whatever the probe found.
//! 3. **Config escape hatch** (`[shell] path_extra` / `env`): applied
//!    last, so it's the true final word — including overriding `PATH`
//!    outright if the user sets one explicitly.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::process::Command;
use tracing::warn;

use crate::config::ShellConfig;

/// Standard system directories that must always be reachable, even when
/// the daemon's own launch environment has a minimal PATH (common for
/// systemd user units).
const SANE_PATH_DIRS: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
];

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_OUTPUT_CAP: usize = 2 * 1024 * 1024; // 2 MiB

/// Shell-internal bookkeeping vars that come back from `env -0` but are
/// never useful (or safe) to layer onto a spawned command's env.
const PROBE_NOISE_KEYS: &[&str] = &["_", "SHLVL", "OLDPWD", "PWD"];

static SHELL_CONFIG: OnceLock<ShellConfig> = OnceLock::new();
/// Set once the login-shell probe finishes (success or not). `None` means
/// "still running" (or never started) — `apply()` falls back to layer 1
/// only until this is populated, and never blocks waiting for it.
static PROBED_ENV: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Call once at process startup (daemon or CLI), before any tool-shell
/// command can run. Stores the config for `apply()` and, if enabled,
/// kicks off the one-time login-shell probe as a background task —
/// non-blocking, per the issue's "daemon 启动后台任务，不阻塞启动路径".
pub fn init(config: ShellConfig) {
    let probe_enabled = config.login_env_probe;
    let _ = SHELL_CONFIG.set(config);

    if probe_enabled {
        tokio::spawn(async move {
            let probed = probe_login_shell_env().await.unwrap_or_default();
            let _ = PROBED_ENV.set(probed);
        });
    } else {
        let _ = PROBED_ENV.set(HashMap::new());
    }
}

/// Apply the env overlay to a spawned tool-shell command. Only ever adds
/// or overrides specific keys via `Command::env()` — the full inherited
/// environment (already present via normal `Command` inheritance) is
/// otherwise left untouched.
pub fn apply(cmd: &mut Command) {
    let config = SHELL_CONFIG.get();
    let mut path = static_path_fallback();

    if let Some(probed) = PROBED_ENV.get() {
        if let Some(probed_path) = probed.get("PATH") {
            path = merge_path_dirs(&path, probed_path);
        }
        for (k, v) in probed {
            // Gap-only, as documented above: an explicit systemd-unit env
            // var (API key, HTTPS_PROXY, …) always outranks whatever the
            // probed login shell happened to also set for that key.
            if k != "PATH" && !PROBE_NOISE_KEYS.contains(&k.as_str()) && std::env::var_os(k).is_none() {
                cmd.env(k, v);
            }
        }
    }

    if let Some(cfg) = config {
        if !cfg.path_extra.is_empty() {
            path = merge_path_dirs(&cfg.path_extra.join(":"), &path);
        }
    }
    cmd.env("PATH", path);

    // Config `[shell] env` — applied last, so it's the genuine final
    // layer, including a literal `PATH` override if the user set one.
    if let Some(cfg) = config {
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
    }
}

/// Layer ① — current PATH plus MyClaw's own bin dir (so a delegated shell
/// can invoke `myclaw` itself) plus any standard system dir still missing.
fn static_path_fallback() -> String {
    build_path_fallback(
        &std::env::var("PATH").unwrap_or_default(),
        own_bin_dir().as_deref(),
    )
}

/// Pure core of `static_path_fallback`, parameterized so tests don't need
/// to mutate the process's real `PATH` (unsafe under parallel test
/// execution — every test in this binary shares one process environment).
fn build_path_fallback(current_path: &str, own_bin: Option<&str>) -> String {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();

    if let Some(bin) = own_bin {
        if seen.insert(bin.to_string()) {
            dirs.push(bin.to_string());
        }
    }
    for d in current_path.split(':').filter(|s| !s.is_empty()) {
        if seen.insert(d.to_string()) {
            dirs.push(d.to_string());
        }
    }
    for d in SANE_PATH_DIRS {
        if seen.insert((*d).to_string()) {
            dirs.push((*d).to_string());
        }
    }
    dirs.join(":")
}

fn own_bin_dir() -> Option<String> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_string_lossy().to_string())
}

/// Merge two colon-separated PATH strings, `primary` first, deduplicated,
/// dropping empty segments.
fn merge_path_dirs(primary: &str, secondary: &str) -> String {
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    for d in primary.split(':').chain(secondary.split(':')) {
        if d.is_empty() {
            continue;
        }
        if seen.insert(d.to_string()) {
            dirs.push(d.to_string());
        }
    }
    dirs.join(":")
}

/// Layer ② — `$SHELL -lic 'env -0'`, NUL-separated parse, sanitized.
/// Returns `None` on any failure (missing/untrusted `$SHELL`, spawn
/// error, timeout, non-zero exit, oversized output) — callers keep
/// layer ① only, with one `warn!` explaining why.
///
/// Deliberately `-lic`, not `-lc`: a plain `-l -c` login shell is
/// non-interactive, and Debian/Ubuntu's default `.bashrc` opens with an
/// early-return guard for exactly that case —
/// `case $- in *i*) ;; *) return;; esac` — so the interactive-only PATH
/// exports this probe exists to find (npm global prefix, nvm, pyenv, …)
/// never run. `-i` makes the shell interactive so `.bashrc` executes in
/// full; `HISTFILE=/dev/null` stops that interactive shell from writing
/// a history entry for the probe command itself.
async fn probe_login_shell_env() -> Option<HashMap<String, String>> {
    let shell = match std::env::var("SHELL") {
        Ok(s) => s,
        Err(_) => {
            warn!("shell_env: $SHELL is not set, skipping login-shell probe");
            return None;
        }
    };
    if !shell_is_allowed(&shell, Path::new("/etc/shells")) {
        warn!(
            shell = %shell,
            "shell_env: $SHELL is not an absolute path listed in /etc/shells, skipping login-shell probe"
        );
        return None;
    }

    let mut cmd = Command::new(&shell);
    cmd.arg("-lic").arg("env -0");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Reaped on timeout instead of orphaned — see the timeout branch below.
    cmd.kill_on_drop(true);
    // Sanitize the probe's own environment: a stray ZDOTDIR would redirect
    // which rc files the probed shell reads; force HOME so it reads the
    // real user's rc files regardless of what the daemon's own HOME is.
    // HISTFILE=/dev/null keeps this synthetic interactive session out of
    // the user's real shell history.
    cmd.env_remove("ZDOTDIR");
    cmd.env("HISTFILE", "/dev/null");
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!(err = %e, shell = %shell, "shell_env: failed to spawn login-shell probe");
            return None;
        }
    };

    let output = match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            warn!(err = %e, "shell_env: login-shell probe I/O error");
            return None;
        }
        Err(_) => {
            warn!(
                timeout_secs = PROBE_TIMEOUT.as_secs(),
                "shell_env: login-shell probe timed out, falling back to static PATH only"
            );
            return None;
        }
    };

    if !output.status.success() {
        warn!(
            status = ?output.status.code(),
            "shell_env: login-shell probe exited non-zero, falling back to static PATH only"
        );
        return None;
    }
    if output.stdout.len() > PROBE_OUTPUT_CAP {
        warn!(
            len = output.stdout.len(),
            cap = PROBE_OUTPUT_CAP,
            "shell_env: login-shell probe output exceeded cap, discarding"
        );
        return None;
    }

    Some(parse_nul_env(&output.stdout))
}

fn parse_nul_env(bytes: &[u8]) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for entry in bytes.split(|&b| b == 0) {
        if entry.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(entry) {
            if let Some((k, v)) = s.split_once('=') {
                vars.insert(k.to_string(), v.to_string());
            }
        }
    }
    vars
}

/// `$SHELL` must be an absolute path and appear verbatim in `/etc/shells`
/// (or whatever whitelist file is passed — parameterized for testing).
fn shell_is_allowed(shell: &str, shells_file: &Path) -> bool {
    if !Path::new(shell).is_absolute() {
        return false;
    }
    match std::fs::read_to_string(shells_file) {
        Ok(contents) => contents.lines().map(str::trim).any(|l| l == shell),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_path_fallback_includes_sane_dirs() {
        let path = build_path_fallback("", None);
        assert!(path.contains("/usr/bin"));
        assert!(path.contains("/bin"));
    }

    #[test]
    fn test_build_path_fallback_dedups_existing_dirs() {
        let path = build_path_fallback("/usr/bin:/custom/bin:/usr/bin", None);
        assert_eq!(path.matches("/usr/bin").count(), 1);
        assert!(path.contains("/custom/bin"));
    }

    #[test]
    fn test_build_path_fallback_prepends_own_bin_dir() {
        let path = build_path_fallback("/usr/bin", Some("/myclaw/bin"));
        assert!(path.starts_with("/myclaw/bin:"));
    }

    #[test]
    fn test_merge_path_dirs_dedups_and_preserves_order() {
        let merged = merge_path_dirs("/a:/b", "/b:/c");
        assert_eq!(merged, "/a:/b:/c");
    }

    #[test]
    fn test_merge_path_dirs_drops_empty_segments() {
        let merged = merge_path_dirs("/a::/b", ":");
        assert_eq!(merged, "/a:/b");
    }

    #[test]
    fn test_parse_nul_env_basic() {
        let raw = b"PATH=/a:/b\0HOME=/home/u\0";
        let vars = parse_nul_env(raw);
        assert_eq!(vars.get("PATH"), Some(&"/a:/b".to_string()));
        assert_eq!(vars.get("HOME"), Some(&"/home/u".to_string()));
    }

    #[test]
    fn test_parse_nul_env_ignores_malformed_entries() {
        let raw = b"NOEQUALSSIGN\0PATH=/a\0";
        let vars = parse_nul_env(raw);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars.get("PATH"), Some(&"/a".to_string()));
    }

    #[test]
    fn test_shell_is_allowed_rejects_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let shells = dir.path().join("shells");
        std::fs::write(&shells, "/bin/bash\n").unwrap();
        assert!(!shell_is_allowed("bash", &shells));
    }

    #[test]
    fn test_shell_is_allowed_requires_whitelist_membership() {
        let dir = tempfile::tempdir().unwrap();
        let shells = dir.path().join("shells");
        std::fs::write(&shells, "/bin/bash\n/bin/zsh\n").unwrap();
        assert!(shell_is_allowed("/bin/bash", &shells));
        assert!(!shell_is_allowed("/bin/fish", &shells));
    }

    #[test]
    fn test_shell_is_allowed_missing_whitelist_file_denies() {
        assert!(!shell_is_allowed("/bin/bash", Path::new("/nonexistent-shells-file")));
    }
}
