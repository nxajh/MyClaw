use crate::agents::AgentDelegator;
use crate::agents::{
    AgentMessenger, DelegationCoordinator, InMemoryBackend, McpManager, Orchestrator,
    OrchestratorParts, RunMode, SessionManager, Skill, SkillManager, SystemPromptConfig,
    ToolRegistry,
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::channels::Channel;

pub(crate) async fn wait_for_signal() -> Result<()> {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    tokio::select! {
        _ = sigint.recv() => {
            tracing::debug!("received SIGINT");
            crate::TERMINATING_FLAG.store(true, Ordering::SeqCst);
        }
        _ = sigterm.recv() => {
            tracing::debug!("received SIGTERM");
            crate::TERMINATING_FLAG.store(true, Ordering::SeqCst);
        }
        _ = sigusr1.recv() => {
            tracing::debug!("received SIGUSR1 — hot switch triggered by `myclaw update`");
            crate::SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// Reset the persisted Telegram update offset so that `getUpdates` returns
/// recent messages instead of skipping everything the old process already
/// fetched.  The dedup layer in TelegramChannel will filter any duplicates.
pub(crate) fn reset_telegram_offset(base_dir: &std::path::Path) {
    let offset_path = crate::config::telegram_offset_path(base_dir);
    if offset_path.exists() {
        if let Err(e) = std::fs::remove_file(&offset_path) {
            tracing::warn!(err = %e, path = %offset_path.display(),
                "failed to remove telegram offset file");
        } else {
            tracing::info!(path = %offset_path.display(),
                "telegram offset cleared — new process will fetch fresh updates");
        }
    }
}
