//! `myclaw reload` — gracefully restart the daemon to apply config changes.
//!
//! Equivalent to `myclaw restart`: triggers a zero-downtime hot-switch so the
//! new process picks up the updated config file in full.

use anyhow::Result;

use crate::cli::Cli;

pub async fn run(_cli: &Cli) -> Result<()> {
    super::signal::send_sigusr1()?;
    println!("Reload signal sent. Daemon will restart and apply new config.");
    Ok(())
}
