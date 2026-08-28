//! Memory audit log, version archiving, and content guards (PII / threat patterns).

use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::api::tool::ToolContext;

const MEMORY_AUDIT_LOG: &str = "memory_audit.jsonl";

pub(super) fn short_sha256(content: &str) -> String {
    let hash = crate::providers::capability_chat::sha256_hex(content.as_bytes());
    hash.chars().take(16).collect()
}

/// Archive the previous version of a memory file before it is overwritten.
///
/// Copies the old content to `{memory_dir}/.versions/{name}/v{N}__{date}__{hash}.md`
/// so that distillation and fork-driven replaces are recoverable.
pub(super) fn archive_memory_version(memory_dir: &Path, name: &str, old_content: &str) {
    let versions_dir = memory_dir.join(".versions").join(name);
    if let Err(e) = std::fs::create_dir_all(&versions_dir) {
        tracing::warn!(err = %e, name, "memory version: failed to create versions dir");
        return;
    }

    // Determine next version number by counting existing archives.
    let next_v = match std::fs::read_dir(&versions_dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|s| s.strip_prefix('v'))
                    .and_then(|s| s.split("__").next())
                    .and_then(|n| n.parse::<u32>().ok())
            })
            .max()
            .map(|m| m + 1)
            .unwrap_or(1),
        Err(_) => 1,
    };

    let hash = short_sha256(old_content);
    let date = chrono::Utc::now().format("%Y%m%d").to_string();
    let archive_name = format!("v{}__{}__{}.md", next_v, date, hash);
    let archive_path = versions_dir.join(&archive_name);

    if let Err(e) = std::fs::write(&archive_path, old_content) {
        tracing::warn!(err = %e, path = %archive_path.display(), "memory version: failed to write archive");
    }
}

fn redact_audit_reason(reason: Option<&str>) -> Option<String> {
    reason
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(500).collect())
}

pub(super) struct MemoryAudit<'a> {
pub(super)     user_id: &'a str,
pub(super)     scope: &'a str,
pub(super)     action: &'a str,
pub(super)     name: &'a str,
pub(super)     old_hash: Option<String>,
pub(super)     new_hash: Option<String>,
pub(super)     args: &'a serde_json::Value,
}

pub(super) fn append_memory_audit(base_dir: &Path, ctx: &ToolContext, audit: MemoryAudit<'_>) {
    // P1-B2: audit is runtime state, lives on the data side
    // ({base_dir}/state/memory/memory_audit.jsonl), not under the
    // memory root (which is the flat agent/user memory pool).
    let audit_dir = crate::config::memory_audit_dir(base_dir);
    if let Err(e) = std::fs::create_dir_all(&audit_dir) {
        tracing::warn!(err = %e, "memory audit: failed to create audit dir");
        return;
    }

    let entry = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "session_id": ctx.session_id,
        "session_owner": ctx.owner,
        "user_id": audit.user_id,
        "scope": audit.scope,
        "action": audit.action,
        "memory_name": audit.name,
        "old_hash": audit.old_hash,
        "new_hash": audit.new_hash,
        "reason": redact_audit_reason(audit.args["reason"].as_str()),
        "model": audit.args["model"].as_str(),
        "source": "memory_manage",
    });

    let path = audit_dir.join(MEMORY_AUDIT_LOG);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{}", entry) {
                tracing::warn!(
                    err = %e,
                    path = %path.display(),
                    "memory audit: failed to write entry"
                );
            }
        }
        Err(e) => tracing::warn!(
            err = %e,
            path = %path.display(),
            "memory audit: failed to open log"
        ),
    }
}

// ── PII guard for agent-scope writes ─────────────────────────────────────

/// Check content for user-identifying patterns before an agent-scope write.
/// This is a conservative bottom-line guard; the distillation prompt is the
/// primary de-identification mechanism.
fn scan_agent_pii(content: &str) -> Option<String> {
    use regex::Regex;

    let patterns: &[(&str, &str)] = &[
        // Known channel-prefixed routing keys: telegram:myclaw:6270938644,
        // qqbot:xiaoer:E8CAAE..., client:default:web:0f53a6e9-...
        (
            r"\b(?:telegram|qqbot|wechat|whatsapp|slack|discord|client|web|webuser):[a-z0-9_]+:[A-Za-z0-9_:\-]+",
            "routing_key",
        ),
        // Long digit runs (user ids, phone numbers).
        (r"\b\d{8,}\b", "numeric_identifier"),
        // Email addresses.
        (
            r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
            "email",
        ),
        // Chinese mobile numbers (11 digits starting with 1[3-9]).
        (r"\b1[3-9]\d{9}\b", "phone"),
    ];

    // Compile once per call; the set is tiny and this path is low-frequency.
    for (pattern, label) in patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(content) {
                return Some(format!(
                    "Blocked: agent-scope memory contains a user-identifying pattern '{}'. \
                     Agent memories are shared across users and must be de-identified. \
                     Remove routing keys, user ids, emails, and phone numbers before writing, \
                     or use scope='user'.",
                    label
                ));
            }
        }
    }
    None
}

pub(super) fn scan_agent_pii_opt(content: &str) -> Result<(), String> {
    match scan_agent_pii(content) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

// ── Threat patterns for memory content scanning ──────────────────────────

fn scan_memory_content(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    let patterns = [
        ("ignore previous instructions", "prompt_injection"),
        ("ignore all instructions", "prompt_injection"),
        ("ignore above instructions", "prompt_injection"),
        ("system prompt override", "sys_prompt_override"),
        ("disregard your instructions", "disregard_rules"),
        ("do not tell the user", "deception_hide"),
    ];
    for (pattern, label) in &patterns {
        if lower.contains(pattern) {
            return Some(format!(
                "Blocked: content matches threat pattern '{}'. \
                 Memory content is injected into the system prompt and must not \
                 contain injection payloads.",
                label
            ));
        }
    }
    None
}

pub(super) fn scan_memory_content_opt(content: &str) -> Result<(), String> {
    match scan_memory_content(content) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
