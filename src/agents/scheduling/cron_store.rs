//! Cron Store — JSON-based cron job storage.
//!
//! Jobs are stored in `workspace/cron/jobs.json`. Each job tracks its
//! schedule, prompt, target, enabled state, and run history (`last_run_at`,
//! `next_run_at`). The scheduler reads this file on startup and re-reads
//! when the mtime changes (hot-reload).
//!
//! The `cronjob` tool writes to the same file so the scheduler picks up
//! changes on the next tick.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// A single cron job stored in `jobs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEntry {
    /// Unique ID (12-char hex).
    pub id: String,
    /// Cron expression (6-field: sec min hour day month weekday).
    /// e.g. "0 0 9 * * *" = every day at 09:00.
    pub schedule: String,
    /// Prompt to send to the agent when triggered.
    pub prompt: String,
    /// Where to send output: "last" | "none" | channel name.
    #[serde(default = "default_target")]
    pub target: String,
    /// Optional friendly name.
    #[serde(default)]
    pub name: Option<String>,
    /// Active hours restriction, e.g. "08:00-24:00". None = always active.
    #[serde(default)]
    pub active_hours: Option<String>,
    /// Whether this job is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// ISO 8601 timestamp of last successful run. None = never run.
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// ISO 8601 timestamp of next scheduled run.
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// ISO 8601 timestamp of job creation.
    #[serde(default)]
    pub created_at: Option<String>,
}

fn default_target() -> String { "last".to_string() }
fn default_true() -> bool { true }

/// The top-level JSON structure of `jobs.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobsFile {
    pub jobs: Vec<JobEntry>,
}

/// Manages loading and saving cron jobs from a JSON file.
pub struct CronStore {
    path: PathBuf,
    data: JobsFile,
    /// Last known mtime of the file (for hot-reload detection).
    last_mtime: Option<SystemTime>,
    /// Timezone offset in hours from UTC (e.g. 8 for UTC+8).
    timezone_offset: i32,
}

impl CronStore {
    /// Create a new store backed by the given path.
    /// Loads existing data if the file exists.
    pub fn new(path: PathBuf, timezone_offset: i32) -> Self {
        let mut store = Self {
            path,
            data: JobsFile::default(),
            last_mtime: None,
            timezone_offset,
        };
        store.load_from_disk();
        store
    }

    /// Get the jobs file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get a read-only view of all jobs.
    pub fn jobs(&self) -> &[JobEntry] {
        &self.data.jobs
    }

    /// Load jobs from disk. Returns true if the file changed.
    fn load_from_disk(&mut self) -> bool {
        if !self.path.exists() {
            return false;
        }

        let mtime = match std::fs::metadata(&self.path) {
            Ok(m) => m.modified().ok(),
            Err(_) => None,
        };

        // Skip reload if mtime unchanged.
        if mtime.is_some() && mtime == self.last_mtime {
            return false;
        }

        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "failed to read jobs.json");
                return false;
            }
        };

        match serde_json::from_str::<JobsFile>(&content) {
            Ok(data) => {
                tracing::info!(count = data.jobs.len(), "cron jobs loaded from JSON");
                self.data = data;
                self.last_mtime = mtime;
                true
            }
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "failed to parse jobs.json");
                false
            }
        }
    }

    /// Check if the file has changed on disk and reload if so.
    /// Returns true if a reload happened.
    pub fn maybe_reload(&mut self) -> bool {
        self.load_from_disk()
    }

    /// Save the current jobs to disk.
    fn save_to_disk(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.data)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }

    /// Add a new job. Returns the generated ID.
    pub fn add_job(&mut self, mut entry: JobEntry) -> anyhow::Result<String> {
        if entry.id.is_empty() {
            entry.id = generate_id();
        }
        if entry.created_at.is_none() {
            entry.created_at = Some(chrono::Utc::now().to_rfc3339());
        }
        // Compute initial next_run_at.
        entry.next_run_at = compute_next_run(&entry.schedule, None, self.timezone_offset);
        let id = entry.id.clone();
        self.data.jobs.push(entry);
        self.save_to_disk()?;
        Ok(id)
    }

    /// Update fields of an existing job. Returns true if found.
    pub fn update_job(&mut self, id: &str, update: JobUpdate) -> anyhow::Result<bool> {
        let job = match self.data.jobs.iter_mut().find(|j| j.id == id) {
            Some(j) => j,
            None => return Ok(false),
        };
        if let Some(name) = update.name {
            job.name = Some(name);
        }
        if let Some(schedule) = update.schedule {
            job.schedule = schedule;
            job.next_run_at = compute_next_run(&job.schedule, job.last_run_at.as_deref(), self.timezone_offset);
        }
        if let Some(prompt) = update.prompt {
            job.prompt = prompt;
        }
        if let Some(target) = update.target {
            job.target = target;
        }
        if let Some(active_hours) = update.active_hours {
            job.active_hours = Some(active_hours);
        }
        if let Some(enabled) = update.enabled {
            job.enabled = enabled;
            if enabled {
                // Re-compute next_run_at when re-enabling.
                job.next_run_at = compute_next_run(&job.schedule, job.last_run_at.as_deref(), self.timezone_offset);
            } else {
                job.next_run_at = None;
            }
        }
        self.save_to_disk()?;
        Ok(true)
    }

    /// Remove a job. Returns true if found and removed.
    pub fn remove_job(&mut self, id: &str) -> anyhow::Result<bool> {
        let len_before = self.data.jobs.len();
        self.data.jobs.retain(|j| j.id != id);
        if self.data.jobs.len() < len_before {
            self.save_to_disk()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Mark a job as run (updates last_run_at and next_run_at).
    pub fn mark_run(&mut self, id: &str) {
        if let Some(job) = self.data.jobs.iter_mut().find(|j| j.id == id) {
            let now = chrono::Utc::now().to_rfc3339();
            job.last_run_at = Some(now);
            job.next_run_at = compute_next_run(&job.schedule, job.last_run_at.as_deref(), self.timezone_offset);
            // Best-effort save — don't propagate errors from here.
            if let Err(e) = self.save_to_disk() {
                tracing::warn!(error = %e, "failed to save jobs.json after marking run");
            }
        }
    }

    /// Find all enabled jobs whose next_run_at <= now.
    /// Returns cloned entries to avoid borrowing issues.
    pub fn get_due_jobs(&self) -> Vec<JobEntry> {
        let now = chrono::Utc::now();
        self.data.jobs.iter()
            .filter(|j| j.enabled)
            .filter(|j| {
                match &j.next_run_at {
                    Some(next) => {
                        chrono::DateTime::parse_from_rfc3339(next)
                            .map(|dt| dt.with_timezone(&chrono::Utc) <= now)
                            .unwrap_or(false)
                    }
                    None => false,
                }
            })
            .cloned()
            .collect()
    }

    /// Migrate jobs from old markdown files in the cron directory.
    /// Returns the number of jobs migrated.
    pub fn migrate_from_markdown(&mut self, cron_dir: &Path) -> usize {
        let mut migrated = 0;

        if !cron_dir.exists() {
            return 0;
        }

        let entries = match std::fs::read_dir(cron_dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }

            // Skip if it's not a cron file (e.g. HEARTBEAT.md).
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let (front_matter, body) = crate::str_utils::parse_front_matter(&content);
            let schedule = match crate::str_utils::extract_yaml_string(&front_matter, "schedule") {
                Some(s) => s,
                None => continue, // Not a cron file.
            };

            let target = crate::str_utils::extract_yaml_string(&front_matter, "target")
                .unwrap_or_else(|| "last".to_string());

            let prompt = body.trim().to_string();
            if prompt.is_empty() {
                continue;
            }

            let active_hours = crate::str_utils::extract_yaml_string(&front_matter, "active_hours");

            // Check if this job already exists (by schedule + prompt combo).
            let already_exists = self.data.jobs.iter().any(|j| {
                j.schedule == schedule && j.prompt == prompt
            });
            if already_exists {
                continue;
            }

            let entry = JobEntry {
                id: generate_id(),
                schedule,
                prompt,
                target,
                name: path.file_stem()
                    .map(|s| s.to_string_lossy().to_string()),
                active_hours,
                enabled: true,
                last_run_at: None,
                next_run_at: None, // will be computed by add_job
                created_at: None,
            };

            if self.add_job(entry).is_ok() {
                migrated += 1;
                tracing::info!(file = %path.file_name().unwrap_or_default().to_string_lossy(),
                    "migrated cron job from markdown");
            }
        }

        migrated
    }
}

/// Partial update for a job.
pub struct JobUpdate {
    pub name: Option<String>,
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub target: Option<String>,
    pub active_hours: Option<String>,
    pub enabled: Option<bool>,
}

/// Compute the next run time for a cron schedule.
/// `last_run` is the ISO 8601 timestamp of the last run, or None for first run.
/// `timezone_offset` is hours from UTC (e.g. 8 for UTC+8).
pub fn compute_next_run(schedule: &str, last_run: Option<&str>, timezone_offset: i32) -> Option<String> {
    let cron_schedule: cron::Schedule = match schedule.parse() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(schedule = %schedule, error = %e, "invalid cron expression for next_run computation");
            return None;
        }
    };

    let offset = chrono::Duration::hours(timezone_offset as i64);
    let base_utc = match last_run {
        Some(ts) => {
            chrono::DateTime::parse_from_rfc3339(ts)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now())
        }
        None => chrono::Utc::now(),
    };

    // Find next fire time in local timezone, then convert back to UTC for storage.
    let base_local = base_utc + offset;
    cron_schedule.after(&base_local).next()
        .map(|dt| (dt - offset).to_rfc3339())
}

/// Generate a random 12-char hex ID.
fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:012x}", nanos & 0xfff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_add_and_list_jobs() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("jobs.json");
        let mut store = CronStore::new(store_path, 8);

        let id = store.add_job(JobEntry {
            id: String::new(),
            schedule: "0 0 9 * * *".to_string(),
            prompt: "test prompt".to_string(),
            target: "last".to_string(),
            name: Some("test".to_string()),
            active_hours: None,
            enabled: true,
            last_run_at: None,
            next_run_at: None,
            created_at: None,
        }).unwrap();

        assert_eq!(store.jobs().len(), 1);
        assert_eq!(store.jobs()[0].id, id);
        assert_eq!(store.jobs()[0].name.as_deref(), Some("test"));
    }

    #[test]
    fn test_update_and_remove() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("jobs.json");
        let mut store = CronStore::new(store_path, 8);

        let id = store.add_job(JobEntry {
            id: String::new(),
            schedule: "0 0 9 * * *".to_string(),
            prompt: "test".to_string(),
            target: "last".to_string(),
            name: None,
            active_hours: None,
            enabled: true,
            last_run_at: None,
            next_run_at: None,
            created_at: None,
        }).unwrap();

        // Update
        let ok = store.update_job(&id, JobUpdate {
            name: Some("updated".to_string()),
            schedule: None,
            prompt: None,
            target: None,
            active_hours: None,
            enabled: None,
        }).unwrap();
        assert!(ok);
        assert_eq!(store.jobs()[0].name.as_deref(), Some("updated"));

        // Disable
        store.update_job(&id, JobUpdate {
            name: None, schedule: None, prompt: None, target: None,
            active_hours: None, enabled: Some(false),
        }).unwrap();
        assert!(!store.jobs()[0].enabled);
        assert!(store.jobs()[0].next_run_at.is_none());

        // Remove
        assert!(store.remove_job(&id).unwrap());
        assert!(store.jobs().is_empty());
    }

    #[test]
    fn test_persistence() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("jobs.json");

        // Write
        {
            let mut store = CronStore::new(store_path.clone(), 8);
            store.add_job(JobEntry {
                id: String::new(),
                schedule: "0 0 9 * * *".to_string(),
                prompt: "persisted".to_string(),
                target: "telegram".to_string(),
                name: None,
                active_hours: None,
                enabled: true,
                last_run_at: None,
                next_run_at: None,
                created_at: None,
            }).unwrap();
        }

        // Read back
        let store = CronStore::new(store_path, 8);
        assert_eq!(store.jobs().len(), 1);
        assert_eq!(store.jobs()[0].prompt, "persisted");
        assert_eq!(store.jobs()[0].target, "telegram");
    }

    #[test]
    fn test_compute_next_run() {
        let next = compute_next_run("0 0 9 * * *", None, 8);
        assert!(next.is_some());
    }
}
