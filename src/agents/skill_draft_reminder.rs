//! Daily throttled reminder for an accumulating draft-skill backlog
//! (issue #89, layer ②; #101 P2: per-layer accounting).
//!
//! `skill_extract` writes drafts with `status: draft` into the session
//! owner's user layer (`users/{uuid}/skills/`), and `skill_loader` filters
//! them out of normal loading (invisible to the agent until reviewed).
//! Layer ① (`skill_extract::notify_drafts_written`) covers the "just wrote
//! one" case; this covers "the user was away / forgot" — once a scope's
//! backlog reaches [`THRESHOLD`], fire a system-reminder telling the agent
//! to surface the count, at most once per calendar day per scope.
//!
//! Two independently throttled scopes (RFC #101 §2.3):
//! - **user scope** — the session owner's `users/{bare_uuid}/skills/`;
//!   surfaced in that owner's sessions. State persists to
//!   `users/{bare_uuid}/skill_draft_reminder_state.json`.
//! - **agent scope** — `{base_dir}/skills/`; surfaced only in the
//!   operator's sessions (`is_operator`). State persists to the legacy
//!   `{base_dir}/skill_draft_reminder_state.json` (compatible).
//!
//! State (last-reminded date) persists to a small JSON file so the
//! throttle holds across sessions and daemon restarts.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Minimum pending drafts (per scope) before the reminder fires.
const THRESHOLD: usize = 5;

/// Reminder state file name (inside each scope's root directory).
const STATE_FILE: &str = "skill_draft_reminder_state.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct ReminderState {
    /// `YYYY-MM-DD` (in the configured timezone) the reminder last fired.
    last_reminded_date: Option<String>,
}

fn today(timezone_offset: i32) -> String {
    let local = chrono::Utc::now() + chrono::Duration::hours(timezone_offset as i64);
    local.format("%Y-%m-%d").to_string()
}

/// A fired backlog reminder, split by layer (#101 P2). A field is
/// populated only when that scope crossed [`THRESHOLD`] and had not yet
/// been reminded today; the caller renders whatever is present.
#[derive(Debug, Default, Clone)]
pub struct Backlog {
    /// Draft names in the session owner's user layer (fired only).
    pub user_layer: Vec<String>,
    /// Draft names in the agent layer (fired only; operator sessions).
    pub agent_layer: Vec<String>,
}

impl Backlog {
    pub fn is_empty(&self) -> bool {
        self.user_layer.is_empty() && self.agent_layer.is_empty()
    }
}

/// One scope's arm cycle: cheap once-a-day throttle check *before* the
/// skills-directory scan (issue #127 — a re-parse of every local SKILL.md
/// on every turn was pure waste), then the threshold check, then state
/// persistence. Returns the draft names when the reminder fires for this
/// scope, `None` otherwise. Never fails: this is a best-effort nudge that
/// must not block or fail a turn.
///
/// `scope_root` is the directory that *contains* both `skills/` and the
/// state file: `base_dir` for the agent scope (legacy layout), the
/// owner's `users/{bare_uuid}/` directory for the user scope.
fn check_scope(scope_root: &Path, timezone_offset: i32) -> Option<Vec<String>> {
    let today = today(timezone_offset);
    let path = scope_root.join(STATE_FILE);
    let already_fired_today = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<ReminderState>(&s).ok())
        .and_then(|s| s.last_reminded_date)
        .is_some_and(|d| d == today);
    if already_fired_today {
        return None;
    }

    let drafts =
        crate::agents::workspace::skill_loader::list_draft_skill_names(&scope_root.join("skills"));
    if drafts.len() < THRESHOLD {
        return None;
    }

    let state = ReminderState {
        last_reminded_date: Some(today),
    };
    if let Ok(json) = serde_json::to_string(&state) {
        // The user scope's root may not exist yet for a fresh owner —
        // create it so the throttle state can persist.
        let _ = std::fs::create_dir_all(scope_root);
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!(err = %e, path = %path.display(), "skill_draft_reminder: failed to persist state");
        }
    }

    Some(drafts)
}

/// Check both backlog scopes and arm whichever crossed [`THRESHOLD`]
/// (#101 P2 per-layer accounting):
///
/// - **user scope** (`users/{bare_dir_name(owner_fqid)}/skills/`) —
///   checked whenever `owner_fqid` is non-empty. The directory may not
///   exist (owner with no drafts yet) — that just finds zero drafts.
/// - **agent scope** (`{base_dir}/skills/`) — checked only when
///   `is_operator` (the operator is the only audience for agent-layer
///   drafts; normal users' sessions never see them).
///
/// Each scope throttles independently (its own state file, its own
/// calendar day, its own threshold count). Returns `None` when neither
/// scope fired. On any I/O error this degrades to "no reminder" — a
/// best-effort nudge that must never block or fail a turn.
///
/// This is called on every turn (`session_context.rs` / `agent.rs`
/// injections), so the throttle check runs before the directory scan
/// (issue #127).
pub fn check_and_arm(
    base_dir: &Path,
    timezone_offset: i32,
    owner_fqid: &str,
    is_operator: bool,
) -> Option<Backlog> {
    let mut backlog = Backlog::default();

    if !owner_fqid.is_empty() {
        let user_root = base_dir
            .join("users")
            .join(crate::ids::bare_dir_name(owner_fqid));
        backlog.user_layer = check_scope(&user_root, timezone_offset).unwrap_or_default();
    }

    if is_operator {
        backlog.agent_layer = check_scope(base_dir, timezone_offset).unwrap_or_default();
    }

    if backlog.is_empty() {
        None
    } else {
        Some(backlog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const OWNER_FQID: &str = "myclaw/u/01923f0e-8a5a-7b3c-9d4e-5f6a7b8c9d0e";
    const OWNER_UUID: &str = "01923f0e-8a5a-7b3c-9d4e-5f6a7b8c9d0e";

    fn write_draft_skill(skills_dir: &Path, name: &str) {
        let dir = skills_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: \"t\"\nstatus: draft\n---\nbody"),
        )
        .unwrap();
    }

    /// The owner's user-layer skills dir under a temp base_dir.
    fn user_skills_dir(base: &Path) -> PathBuf {
        base.join("users").join(OWNER_UUID).join("skills")
    }

    /// Write an already-fired-today state for one scope root.
    fn arm_scope_state(scope_root: &Path) {
        std::fs::create_dir_all(scope_root).unwrap();
        std::fs::write(
            scope_root.join(STATE_FILE),
            serde_json::to_string(&ReminderState {
                last_reminded_date: Some(today(0)),
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn below_threshold_never_fires() {
        let base = tempfile::tempdir().unwrap();
        for i in 0..THRESHOLD - 1 {
            write_draft_skill(&user_skills_dir(base.path()), &format!("draft-{i}"));
        }
        assert!(check_and_arm(base.path(), 0, OWNER_FQID, false).is_none());
    }

    #[test]
    fn at_threshold_fires_once_then_throttles_same_day() {
        let base = tempfile::tempdir().unwrap();
        for i in 0..THRESHOLD {
            write_draft_skill(&user_skills_dir(base.path()), &format!("draft-{i}"));
        }
        let first = check_and_arm(base.path(), 0, OWNER_FQID, false);
        assert_eq!(first.map(|b| b.user_layer.len()), Some(THRESHOLD));
        assert!(first.is_some_and(|b| b.agent_layer.is_empty()));

        // Second call same day: throttled even though the backlog is
        // still (more than) big enough.
        let second = check_and_arm(base.path(), 0, OWNER_FQID, false);
        assert!(second.is_none());
    }

    /// issue #127: on an already-throttled-today call, `check_and_arm`
    /// must return `None` without needing the skills directory to exist at
    /// all — a regression guard for the reordered throttle-before-scan
    /// path.
    #[test]
    fn throttled_call_does_not_require_skills_dir_to_exist() {
        let base = tempfile::tempdir().unwrap();
        // No `users/{uuid}/skills/` directory created at all.
        arm_scope_state(&base.path().join("users").join(OWNER_UUID));

        assert!(check_and_arm(base.path(), 0, OWNER_FQID, false).is_none());
        assert!(!user_skills_dir(base.path()).exists());
    }

    #[test]
    fn fires_again_on_a_new_day() {
        let base = tempfile::tempdir().unwrap();
        for i in 0..THRESHOLD {
            write_draft_skill(&user_skills_dir(base.path()), &format!("draft-{i}"));
        }
        let user_root = base.path().join("users").join(OWNER_UUID);
        std::fs::create_dir_all(&user_root).unwrap();
        std::fs::write(
            user_root.join(STATE_FILE),
            serde_json::to_string(&ReminderState {
                last_reminded_date: Some("2000-01-01".to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let result = check_and_arm(base.path(), 0, OWNER_FQID, false);
        assert!(result.is_some(), "a new day must re-arm the reminder");
    }

    /// #101 P2: a normal (non-operator) user never sees agent-layer
    /// drafts, even at/above threshold.
    #[test]
    fn agent_layer_hidden_from_normal_users() {
        let base = tempfile::tempdir().unwrap();
        for i in 0..THRESHOLD {
            write_draft_skill(&base.path().join("skills"), &format!("agent-{i}"));
        }
        assert!(
            check_and_arm(base.path(), 0, OWNER_FQID, false).is_none(),
            "non-operator sessions must not see agent-layer backlog"
        );
    }

    /// #101 P2: the operator sees agent-layer drafts (and their own user
    /// layer in the same call).
    #[test]
    fn operator_sees_agent_layer() {
        let base = tempfile::tempdir().unwrap();
        for i in 0..THRESHOLD {
            write_draft_skill(&base.path().join("skills"), &format!("agent-{i}"));
        }
        let backlog = check_and_arm(base.path(), 0, OWNER_FQID, true).unwrap();
        assert_eq!(backlog.agent_layer.len(), THRESHOLD);
        assert!(backlog.user_layer.is_empty());
    }

    /// #101 P2: each scope throttles on its own calendar day — a fired
    /// user-scope reminder does not suppress the agent scope (and vice
    /// versa).
    #[test]
    fn scopes_throttle_independently() {
        let base = tempfile::tempdir().unwrap();
        for i in 0..THRESHOLD {
            write_draft_skill(&user_skills_dir(base.path()), &format!("draft-{i}"));
            write_draft_skill(&base.path().join("skills"), &format!("agent-{i}"));
        }
        // User scope already fired today (state persisted); agent scope
        // has not. An operator call must still surface the agent layer.
        arm_scope_state(&base.path().join("users").join(OWNER_UUID));
        let backlog = check_and_arm(base.path(), 0, OWNER_FQID, true).unwrap();
        assert!(backlog.user_layer.is_empty(), "user scope stays throttled");
        assert_eq!(backlog.agent_layer.len(), THRESHOLD);
    }

    /// #101 P2: no owner and not the operator → nothing is scanned at all
    /// (legacy state at the agent path is untouched).
    #[test]
    fn empty_owner_non_operator_is_a_noop() {
        let base = tempfile::tempdir().unwrap();
        for i in 0..THRESHOLD {
            write_draft_skill(&base.path().join("skills"), &format!("agent-{i}"));
        }
        assert!(check_and_arm(base.path(), 0, "", false).is_none());
        assert!(!base.path().join(STATE_FILE).exists());
    }
}
