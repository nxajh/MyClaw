fn main() {
    // Embed short git hash for --version output.
    let git_hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let version = format!("{} ({})", env!("CARGO_PKG_VERSION"), git_hash);
    println!("cargo:rustc-env=MYCLAW_VERSION={}", version);
    // Re-run if HEAD changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
}
