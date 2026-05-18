//! System information helpers for runtime section of the system prompt.

/// Reads a human-readable OS description from /etc/os-release (Linux),
/// falling back to the Rust compile-time OS name on other platforms.
pub fn os_version() -> String {
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
                return val.trim_matches('"').to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

/// Returns the shell name from the SHELL environment variable,
/// stripping the leading path (e.g. "/bin/bash" → "bash").
pub fn shell() -> String {
    std::env::var("SHELL")
        .map(|s| s.rsplit('/').next().unwrap_or(&s).to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Returns a one-line runtime description suitable for the ## Runtime section.
///
/// Example: `Ubuntu 24.04 LTS (linux/x86_64) | Shell: bash`
pub fn runtime_info() -> String {
    format!(
        "{} ({}/{}) | Shell: {}",
        os_version(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        shell(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_info_is_nonempty() {
        let info = runtime_info();
        assert!(!info.is_empty());
        assert!(info.contains("Shell:"));
    }
}
