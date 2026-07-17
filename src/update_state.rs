//! Persistent state for `myclaw update` so CLI and daemon can report
//! whether a hot-switch is staged, in progress, completed, or failed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Canonical path: `~/.myclaw/update-state.json`.
pub fn update_state_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".myclaw/update-state.json"))
        .unwrap_or_else(|_| std::env::temp_dir().join("myclaw-update-state.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    /// Binary replaced on disk; SIGUSR1 not yet sent (or just about to be sent).
    Staged,
    /// SIGUSR1 sent; waiting for new process readiness.
    Switching,
    /// New process ready (hot switch finished).
    Completed,
    /// Staging or switch failed.
    Failed,
}

impl UpdateStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Switching => "switching",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateState {
    pub status: UpdateStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// RFC3339 timestamp of last state write.
    pub updated_at: String,
}

impl UpdateState {
    pub fn now_rfc3339() -> String {
        // Prefer chrono if available via existing deps; fallback to simple system time.
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{secs}")
    }

    pub fn load() -> Result<Option<Self>> {
        let path = update_state_path();
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let state: Self = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(state))
    }

    pub fn save(&self) -> Result<()> {
        let path = update_state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        // Atomic-ish write: temp + rename in same directory.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Promote an in-flight update (staged/switching) to completed.
    /// No-op if there is no state file or status is failed.
    pub fn mark_completed(new_pid: u32) -> Result<()> {
        let Some(mut state) = Self::load()? else {
            return Ok(());
        };
        if !matches!(
            state.status,
            UpdateStatus::Staged | UpdateStatus::Switching
        ) {
            return Ok(());
        }
        state.status = UpdateStatus::Completed;
        state.new_pid = Some(new_pid);
        state.error = None;
        state.updated_at = Self::now_rfc3339();
        state.save()?;
        Ok(())
    }

    pub fn mark_failed(error: impl Into<String>) -> Result<()> {
        let mut state = Self::load()?.unwrap_or_else(|| Self {
            status: UpdateStatus::Failed,
            run_id: None,
            commit: None,
            binary_path: None,
            binary_sha256: None,
            old_pid: None,
            new_pid: None,
            error: None,
            updated_at: Self::now_rfc3339(),
        });
        state.status = UpdateStatus::Failed;
        state.error = Some(error.into());
        state.updated_at = Self::now_rfc3339();
        state.save()?;
        Ok(())
    }
}

/// SHA-256 hex of a file on disk.
pub fn file_sha256_hex(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_status_as_str() {
        assert_eq!(UpdateStatus::Staged.as_str(), "staged");
        assert_eq!(UpdateStatus::Switching.as_str(), "switching");
        assert_eq!(UpdateStatus::Completed.as_str(), "completed");
        assert_eq!(UpdateStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn serde_roundtrip() {
        let s = UpdateState {
            status: UpdateStatus::Staged,
            run_id: Some("123".into()),
            commit: Some("abc1234".into()),
            binary_path: Some("/home/u/.local/bin/myclaw".into()),
            binary_sha256: Some("deadbeef".into()),
            old_pid: Some(1),
            new_pid: None,
            error: None,
            updated_at: "1".into(),
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: UpdateState = serde_json::from_str(&j).unwrap();
        assert_eq!(back.status, UpdateStatus::Staged);
        assert_eq!(back.run_id.as_deref(), Some("123"));
        assert_eq!(back.old_pid, Some(1));
    }
}
