//! Daemon — MyClaw server process entry point.
//!
//! Mimics ZeroClaw's `start_channels()` pattern:
//! 1. Load config from TOML file
//! 2. Initialize tracing/logging
//! 3. Create and start Orchestrator (channel listeners + message dispatch)
//! 4. Wait for shutdown signal, then graceful exit

use anyhow::{Context, Result};
use crate::orchestrator::Orchestrator;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;

/// Default config file location.
const DEFAULT_CONFIG_PATHS: &[&str] = &[
    "myclaw.toml",
    "~/.myclaw/myclaw.toml",
    "/etc/myclaw/myclaw.toml",
];

/// Global shutdown flag.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Load configuration from the first found config file.
pub fn load_config() -> Result<config::AppConfig> {
    for path in DEFAULT_CONFIG_PATHS {
        let expanded = shellexpand::tilde(path).to_string();
        let p = PathBuf::from(expanded);
        if p.exists() {
            tracing::info!(path = %p.display(), "loading config");
            return config::ConfigLoader::from_file(&p)
                .context("failed to load config");
        }
    }
    anyhow::bail!(
        "No config file found. Looked in: {}",
        DEFAULT_CONFIG_PATHS.join(", ")
    );
}

/// Load configuration from a specific path.
pub fn load_config_from(path: &str) -> Result<config::AppConfig> {
    let expanded = shellexpand::tilde(path).to_string();
    let p = PathBuf::from(expanded.clone());
    if !p.exists() {
        anyhow::bail!("Config file not found: {}", expanded);
    }
    tracing::info!(path = %p.display(), "loading config");
    config::ConfigLoader::from_file(&p).context("failed to load config")
}

/// Initialize tracing subscriber based on config.
pub fn init_tracing(config: &config::AppConfig) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let level = config
        .logging
        .level
        .as_deref()
        .unwrap_or("info");

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_thread_ids(true));

    tracing::subscriber::set_global_default(subscriber)
        .expect("failed to set tracing subscriber");
}

/// Print startup banner with config summary.
fn print_banner(config: &config::AppConfig) {
    println!();
    println!("🐾 MyClaw Daemon");
    println!("  📁 Workspace: {}", config.workspace_dir.display());

    let channels: Vec<&str> = config
        .channels
        .enabled_channels()
        .iter()
        .map(|s| &**s)
        .collect();
    println!("  📡 Channels: {}", channels.join(", "));

    let providers: Vec<&str> = config.providers.keys().map(|s| &**s).collect();
    println!("  🤖 Providers: {}", providers.join(", "));

    if let Some(chat_route) = config
        .routing
        .get(config::provider::Capability::Chat)
        .map(|e| e.models.join(" → "))
    {
        println!("  🗺️  Chat route: {}", chat_route);
    }

    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();
}

/// Run the MyClaw daemon, blocking until shutdown.
/// Returns Ok(()) on graceful shutdown, Err on error.
pub async fn run(config: config::AppConfig) -> Result<()> {
    // Build orchestrator (this spawns channel listeners).
    let (mut orchestrator, _msg_tx) =
        Orchestrator::new(&config).context("failed to create orchestrator")?;

    print_banner(&config);

    // Spawn shutdown guard.
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = shutdown_tx;
    });

    // Wait for Ctrl+C or SIGTERM, then initiate graceful shutdown.
    tokio::spawn(async move {
        let _ = wait_for_signal().await;
        tracing::info!("shutdown signal received, initiating graceful shutdown...");
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    });

    // Run the message dispatch loop (blocks until all channels disconnect).
    orchestrator.run().await.context("orchestrator run error")?;

    // Graceful shutdown: await all listener tasks.
    tracing::info!("dispatch loop ended, shutting down listeners...");
    orchestrator.shutdown_listeners().await;

    tracing::info!("myclaw daemon stopped");
    Ok(())
}

/// Wait for SIGINT or SIGTERM.
async fn wait_for_signal() -> Result<()> {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = sigint.recv() => {
            tracing::debug!("received SIGINT");
        }
        _ = sigterm.recv() => {
            tracing::debug!("received SIGTERM");
        }
    }
    Ok(())
}

/// Return true if shutdown was requested (e.g., via signal).
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}
