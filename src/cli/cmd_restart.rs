//! `myclaw restart` — send SIGUSR1 to the running daemon for graceful restart.

use anyhow::Result;

use crate::cli::Cli;

pub async fn run(_cli: &Cli) -> Result<()> {
    super::signal::send_sigusr1()?;
    println!("Restart signal sent. Daemon will restart shortly.");
    Ok(())
}
