//! Per-user identity and profile.
//!
//! RFC v2 §三.D: each routing_key (channel:account:sender) maps to a `user_id`
//! that namespaces memory and profile data. Profiles live at
//! `workspace/users/{user_id}/profile.toml`.
//!
//! For now `UserResolver` is the identity map — routing_key → user_id —
//! deliberately trivial (defaults to the routing_key itself). Later we can
//! plug in a channel-aware mapping (e.g. multiple Telegram routing_keys
//! collapsing to the same human).

use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

// ── UserResolver ────────────────────────────────────────────────────────────

/// Maps routing_keys to user_ids.
///
/// Default behavior: `user_id == routing_key` (one user per channel:account:sender).
/// Operator-supplied overrides via `set(routing_key, user_id)` collapse multiple
/// routing_keys to one user — e.g. a person who reaches the bot from two
/// Telegram accounts.
#[derive(Default)]
pub struct UserResolver {
    overrides: RwLock<std::collections::HashMap<String, String>>,
}

impl UserResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a routing_key to a user_id.
    pub fn resolve(&self, routing_key: &str) -> String {
        self.overrides
            .read()
            .get(routing_key)
            .cloned()
            .unwrap_or_else(|| routing_key.to_string())
    }

    /// Pin a routing_key to a specific user_id.
    pub fn set(&self, routing_key: impl Into<String>, user_id: impl Into<String>) {
        self.overrides
            .write()
            .insert(routing_key.into(), user_id.into());
    }

    /// Reverse-map: list all routing_keys that resolve to `user_id`. Used
    /// for `list_sessions_for_user` (G44). Linear in override map size.
    pub fn routing_keys_for(&self, user_id: &str) -> Vec<String> {
        self.overrides
            .read()
            .iter()
            .filter(|(_, v)| v.as_str() == user_id)
            .map(|(k, _)| k.clone())
            .collect()
    }
}

// ── UserProfile ─────────────────────────────────────────────────────────────

/// Per-user customization persisted at `workspace/users/{user_id}/profile.toml`.
///
/// All fields are optional. `custom_instructions` is the catch-all free-form
/// area where migrated USER.md content lives; the typed fields exist so the
/// LLM gets a stable name/language/timezone block at the top of the section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserProfile {
    /// Display name for greetings ("Hi <name>") and self-reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// IANA timezone (e.g. "Asia/Shanghai"). Overrides AgentConfig default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,

    /// Preferred response language (e.g. "zh-CN", "en").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_language: Option<String>,

    /// Free-form Markdown shown verbatim in the system prompt. Migrated
    /// USER.md content lands here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
}

impl UserProfile {
    /// Load from `workspace/users/{user_id}/profile.toml`.
    /// Returns `Default::default()` (all None) when the file is missing or
    /// unparseable; missing profiles are normal.
    pub fn load(workspace_dir: &Path, user_id: &str) -> Self {
        let path = Self::path(workspace_dir, user_id);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        match toml::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    user = %user_id,
                    path = %path.display(),
                    err = %e,
                    "failed to parse profile.toml; using empty profile"
                );
                Self::default()
            }
        }
    }

    /// Compute the on-disk path for a user's profile.
    pub fn path(workspace_dir: &Path, user_id: &str) -> PathBuf {
        workspace_dir
            .join("users")
            .join(user_id)
            .join("profile.toml")
    }

    /// Serialize and write to disk. Creates parent directories as needed.
    pub fn save(&self, workspace_dir: &Path, user_id: &str) -> std::io::Result<()> {
        let path = Self::path(workspace_dir, user_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        std::fs::write(&path, body)
    }

    /// True if every field is empty/None — used to decide whether to inject
    /// a "## User" section at all.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.timezone.is_none()
            && self.preferred_language.is_none()
            && self
                .custom_instructions
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
    }

    /// Render the profile as a Markdown section appended to the system prompt.
    /// Returns `None` when `is_empty()` so callers can skip the section
    /// header entirely rather than emit an empty block.
    pub fn to_prompt_section(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::from("## User\n\n");
        if let Some(name) = &self.name {
            out.push_str(&format!("Name: {name}\n"));
        }
        if let Some(lang) = &self.preferred_language {
            out.push_str(&format!("Preferred language: {lang}\n"));
        }
        if let Some(tz) = &self.timezone {
            out.push_str(&format!("Timezone: {tz}\n"));
        }
        if let Some(notes) = self.custom_instructions.as_deref() {
            let trimmed = notes.trim();
            if !trimmed.is_empty() {
                if out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(trimmed);
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_defaults_to_identity() {
        let r = UserResolver::new();
        assert_eq!(r.resolve("telegram:default:42"), "telegram:default:42");
    }

    #[test]
    fn resolver_override_collapses_keys() {
        let r = UserResolver::new();
        r.set("telegram:default:42", "alice");
        r.set("wechat:default:wxid_alice", "alice");
        assert_eq!(r.resolve("telegram:default:42"), "alice");
        assert_eq!(r.resolve("wechat:default:wxid_alice"), "alice");
        let mut keys = r.routing_keys_for("alice");
        keys.sort();
        assert_eq!(
            keys,
            vec!["telegram:default:42", "wechat:default:wxid_alice"]
        );
    }

    #[test]
    fn empty_profile_renders_none() {
        assert!(UserProfile::default().to_prompt_section().is_none());
    }

    #[test]
    fn profile_renders_known_fields() {
        let p = UserProfile {
            name: Some("Wilf".into()),
            preferred_language: Some("zh-CN".into()),
            timezone: Some("Asia/Shanghai".into()),
            custom_instructions: Some("Be terse.".into()),
        };
        let sec = p.to_prompt_section().unwrap();
        assert!(sec.starts_with("## User"));
        assert!(sec.contains("Name: Wilf"));
        assert!(sec.contains("Preferred language: zh-CN"));
        assert!(sec.contains("Timezone: Asia/Shanghai"));
        assert!(sec.contains("Be terse."));
    }

    #[test]
    fn profile_load_returns_empty_for_missing() {
        let tmp = std::env::temp_dir().join(format!("myclaw_user_test_{}", std::process::id()));
        let p = UserProfile::load(&tmp, "noone");
        assert!(p.is_empty());
    }

    #[test]
    fn profile_save_then_load_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "myclaw_user_rt_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let p = UserProfile {
            name: Some("Wilf".into()),
            timezone: None,
            preferred_language: Some("zh-CN".into()),
            custom_instructions: None,
        };
        p.save(&tmp, "wilf").unwrap();
        let loaded = UserProfile::load(&tmp, "wilf");
        assert_eq!(loaded.name.as_deref(), Some("Wilf"));
        assert_eq!(loaded.preferred_language.as_deref(), Some("zh-CN"));
        assert!(loaded.timezone.is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
