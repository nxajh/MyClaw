//! `myclaw stop` — send SIGTERM to the running daemon for graceful shutdown.

use anyhow::Result;

use crate::cli::Cli;

pub async fn run(_cli: &Cli) -> Result<()> {
    super::signal::send_sigterm()?;
    println!("Stop signal sent. Daemon will shut down gracefully.");
    Ok(())
}
