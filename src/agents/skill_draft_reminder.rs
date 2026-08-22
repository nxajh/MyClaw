//! Daily throttled reminder for an accumulating draft-skill backlog
//! (issue #89, layer ②).
//!
//! `skill_extract` writes drafts with `status: draft`, and `skill_loader`
//! filters them out of normal loading (invisible to the agent until
//! reviewed). Layer ① (`skill_extract::notify_drafts_written`) covers the
//! "just wrote one" case; this covers "the user was away / forgot" — once
//! the backlog reaches [`THRESHOLD`], fire a system-reminder telling the
//! agent to surface the count, at most once per calendar day. State
//! (last-reminded date) persists to a small JSON file under `base_dir` so
//! the throttle holds across sessions and daemon restarts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Minimum pending drafts before the reminder fires.
const THRESHOLD: usize = 5;

fn state_path(base_dir: &Path) -> PathBuf {
    base_dir.join("skill_draft_reminder_state.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ReminderState {
    /// `YYYY-MM-DD` (in the configured timezone) the reminder last fired.
    last_reminded_date: Option<String>,
}

fn today(timezone_offset: i32) -> String {
    let local = chrono::Utc::now() + chrono::Duration::hours(timezone_offset as i64);
    local.format("%Y-%m-%d").to_string()
}

/// If the draft backlog under `{base_dir}/skills` (= `AppConfig::skills_root()`
/// — where `skill_manage` actually writes drafts, issue #102) is at or
/// above [`THRESHOLD`] and no reminder has fired yet today, returns the
/// draft names and persists today's date so it won't fire again until
/// tomorrow. Returns `None` otherwise — including on any I/O error, since
/// this is a best-effort nudge that must never block or fail a turn.
///
/// This is called on every turn (`session_context.rs`/`agent.rs`), so the
/// cheap once-a-day throttle check runs *before* the full skills-directory
/// scan (issue #127) — a re-parse of every local SKILL.md, every turn,
/// regardless of whether today's reminder already fired, was pure waste
/// (and re-triggers `parse_skill_file`'s deprecation WARNs on every turn
/// for any not-yet-migrated skill).
pub fn check_and_arm(base_dir: &Path, timezone_offset: i32) -> Option<Vec<String>> {
    let today = today(timezone_offset);
    let path = state_path(base_dir);
    let already_fired_today = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<ReminderState>(&s).ok())
        .and_then(|s| s.last_reminded_date)
        .is_some_and(|d| d == today);
    if already_fired_today {
        return None;
    }

    let skills_dir = base_dir.join("skills");
    let drafts = crate::agents::workspace::skill_loader::list_draft_skill_names(&skills_dir);
    if drafts.len() < THRESHOLD {
        return None;
    }

    let state = ReminderState {
        last_reminded_date: Some(today),
    };
    if let Ok(json) = serde_json::to_string(&state) {
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!(err = %e, path = %path.display(), "skill_draft_reminder: failed to persist state");
        }
    }

    Some(drafts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_draft_skill(skills_dir: &Path, name: &str) {
        let dir = skills_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: \"t\"\nstatus: draft\n---\nbody"),
        )
        .unwrap();
    }

    #[test]
    fn below_threshold_never_fires() {
        let base = tempfile::tempdir().unwrap();
        let skills_dir = base.path().join("skills");
        for i in 0..THRESHOLD - 1 {
            write_draft_skill(&skills_dir, &format!("draft-{i}"));
        }
        assert!(check_and_arm(base.path(), 0).is_none());
    }

    #[test]
    fn at_threshold_fires_once_then_throttles_same_day() {
        let base = tempfile::tempdir().unwrap();
        let skills_dir = base.path().join("skills");
        for i in 0..THRESHOLD {
            write_draft_skill(&skills_dir, &format!("draft-{i}"));
        }
        let first = check_and_arm(base.path(), 0);
        assert_eq!(first.map(|v| v.len()), Some(THRESHOLD));

        // Second call same day: throttled even though the backlog is
        // still (more than) big enough.
        let second = check_and_arm(base.path(), 0);
        assert!(second.is_none());
    }

    /// issue #127: on an already-throttled-today call, `check_and_arm`
    /// must return `None` without needing the skills directory to exist at
    /// all — a regression guard for the reordered throttle-before-scan
    /// path (previously the (missing) directory would have been scanned,
    /// harmlessly finding zero drafts either way, so this doesn't
    /// distinguish the two orderings by itself; it does guard against the
    /// throttle check ever being made to depend on the skills directory).
    #[test]
    fn throttled_call_does_not_require_skills_dir_to_exist() {
        let base = tempfile::tempdir().unwrap();
        // No `skills/` directory created at all.
        std::fs::write(
            state_path(base.path()),
            serde_json::to_string(&ReminderState {
                last_reminded_date: Some(today(0)),
            })
            .unwrap(),
        )
        .unwrap();

        assert!(check_and_arm(base.path(), 0).is_none());
        assert!(!base.path().join("skills").exists());
    }

    #[test]
    fn fires_again_on_a_new_day() {
        let base = tempfile::tempdir().unwrap();
        let skills_dir = base.path().join("skills");
        for i in 0..THRESHOLD {
            write_draft_skill(&skills_dir, &format!("draft-{i}"));
        }
        std::fs::write(
            state_path(base.path()),
            serde_json::to_string(&ReminderState {
                last_reminded_date: Some("2000-01-01".to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let result = check_and_arm(base.path(), 0);
        assert!(result.is_some(), "a new day must re-arm the reminder");
    }
}
