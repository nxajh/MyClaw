//! Idle-time skill internalization proposer (RFC #101 §2.4, P3).
//!
//! Structural sibling of `memory_distill`: when the system is idle and
//! user-layer skills have changed, scan all `users/*/skills/*/SKILL.md`
//! (active only — drafts belong to `myclaw-skill-triage`), classify each
//! skill by cross-user generality (same criterion family as memory
//! distillation: is it still bound to a single user's context?), and:
//!
//! - Tier A (universally usable, zero personal identifiers — verified by a
//!   code-level hard gate, not just the LLM): promote directly by moving the
//!   skill directory into the agent layer (`{base_dir}/skills/`). The watcher
//!   (#204) picks the move up and hot-reloads. The proposer is the only
//!   automatic writer to the agent skill layer, mirroring "distillation is
//!   the only writer to the agent memory layer".
//! - Tier B (methodology is general but the body carries instance bindings —
//!   local paths, hostnames, account names): write a proposal file with a
//!   de-identification checklist and the extracted instance parameters
//!   (which belong in user-layer memory, not in the skill). Promotion
//!   requires the operator's signature — the proposer never rewrites skill
//!   bodies on its own.
//! - Tier C (bound to personal assets/hostnames/business flows): stays in
//!   the user layer.
//!
//! Design intent: the proposer discovers, the operator signs edits. Tier A
//! moves are lossless (byte-identical relocation), so they do not need a
//! signature; anything that rewrites content does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Semaphore;

use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest};

// Runtime-verified on first production pass (35 backlog skills): one glm-5.2
// batch call alone takes ~5min, so 3 serial batches ≈ 15min — the 300s
// envelope inherited from memory_distill cut the pass mid-flight. This is an
// idle background task guarded by a semaphore; give it generous room.
// Subsequent passes are incremental (1-2 skills) and finish in one batch.
const OVERALL_TIMEOUT: Duration = Duration::from_secs(1200);
const MAX_INPUT_CHARS: usize = 60_000;
const PER_SKILL_CHARS: usize = 4_000;

static PROPOSER_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

/// Input bundle for one proposer pass. Owned/`Arc` so it can move into
/// `tokio::spawn` / `timeout`.
pub struct ProposerInput {
    /// Model ID (same family as the main agent — prefix-cache friendly).
    pub model_id: String,
    pub provider: Arc<dyn ChatProvider>,
    /// Users root (`{base_dir}/users`) — user-layer skills live under
    /// `users/{uuid}/skills/{name}/`.
    pub users_dir: PathBuf,
    /// Agent-layer skills root (`{base_dir}/skills`).
    pub skills_root: PathBuf,
    /// Directory for proposal files (`{base_dir}/skill-proposals`).
    pub proposals_dir: PathBuf,
    /// Sha index from the caller's persistent state (name → content sha at
    /// last classification) — the increment filter for candidate collection.
    pub classified: HashMap<String, String>,
}

/// A user-layer skill candidate collected for classification.
struct Candidate {
    name: String,
    /// Owner uuid directory name (bare).
    owner: String,
    dir: PathBuf,
    content: String,
    sha: String,
    /// Personal identifiers found by the code-level hard gate.
    identifiers: Vec<String>,
}

/// Persistent proposer progress state (`.state/skill-proposer/proposer.json`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProposerState {
    /// RFC3339 timestamp of the last successful pass.
    pub last_propose_ts: Option<String>,
    /// RFC3339 timestamp of the last attempted pass (success or failure).
    pub last_attempt_ts: Option<String>,
    /// Consecutive failures — after 3, back off for 2 hours (same policy as
    /// memory distill).
    pub consecutive_failures: u32,
    /// Content hashes of skills already classified as B/C (not promoted).
    /// Unchanged skills are skipped on later passes; a content change
    /// re-opens classification.
    pub classified_shas: HashMap<String, String>,
}

impl ProposerState {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("proposer.json")
    }

    /// Load state from disk; defaults when missing or unparsable.
    pub fn load(state_dir: &Path) -> ProposerState {
        match std::fs::read_to_string(Self::path(state_dir)) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => ProposerState::default(),
        }
    }

    /// Save state atomically enough for runtime state (best-effort).
    pub fn save(&self, state_dir: &Path) {
        let path = Self::path(state_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&path, content) {
                tracing::warn!(err = %e, path = %path.display(), "skill_proposer: failed to save state");
            }
        }
    }

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    /// Whether a pass may run given backoff state.
    pub fn in_backoff(&self) -> bool {
        if self.consecutive_failures < 3 {
            return false;
        }
        let Some(last) = self.last_attempt_ts.as_deref() else {
            return false;
        };
        chrono::DateTime::parse_from_rfc3339(last)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .is_some_and(|dt| {
                chrono::Utc::now().signed_duration_since(dt) < chrono::Duration::hours(2)
            })
    }

    /// Mark an attempt and persist.
    pub fn record_attempt(&mut self, success: bool, state_dir: &Path) {
        self.last_attempt_ts = Some(Self::now_rfc3339());
        if success {
            self.last_propose_ts = Some(Self::now_rfc3339());
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        }
        self.save(state_dir);
    }
}

/// Whether any user-layer SKILL.md was modified after `last_propose_ts`.
/// A `None` timestamp means "never proposed" → pending.
pub fn has_pending_user_skills(users_dir: &Path, last_propose_ts: Option<&str>) -> bool {
    let last = last_propose_ts
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    for skill_md in iter_user_skill_files(users_dir) {
        if !is_active(&skill_md) {
            continue; // drafts are triage territory, not proposer input
        }
        let Ok(meta) = std::fs::metadata(&skill_md) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let mtime: chrono::DateTime<chrono::Utc> = mtime.into();
        match last {
            None => return true,
            Some(last) if mtime > last => return true,
            Some(_) => {}
        }
    }
    false
}

/// All `users/{uuid}/skills/{name}/SKILL.md` paths.
fn iter_user_skill_files(users_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(users) = std::fs::read_dir(users_dir) else {
        return out;
    };
    for user in users.flatten() {
        let skills = user.path().join("skills");
        let Ok(entries) = std::fs::read_dir(&skills) else {
            continue;
        };
        for entry in entries.flatten() {
            let md = entry.path().join("SKILL.md");
            if md.is_file() {
                out.push(md);
            }
        }
    }
    out
}

/// A skill is active iff its frontmatter has no `status: draft` line.
fn is_active(skill_md: &Path) -> bool {
    match std::fs::read_to_string(skill_md) {
        Ok(content) => {
            let fm_end = content.find("\n---").unwrap_or(content.len());
            !content[..fm_end].lines().any(|l| l.trim() == "status: draft")
        }
        Err(_) => false,
    }
}

/// Code-level hard gate for Tier A: scan for personal identifiers that must
/// not leak into the (globally visible) agent layer. Combines regex families
/// with dynamic instance values (hostname, user uuid directory names).
pub fn scan_personal_identifiers(text: &str, users_dir: &Path) -> Vec<String> {
    let mut hits: Vec<String> = Vec::new();

    // Regex families (structural patterns). Review feedback (PR #206): the
    // gate's stated intent is "code-level, not just the LLM", so common PII
    // families must not rely on LLM backstop. rust regex has no lookahead —
    // universal IPv4 constants are filtered after matching instead.
    let families: &[(&str, &str)] = &[
        (r"/home/[A-Za-z0-9_.-]+", "home path"),
        (
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            "uuid",
        ),
        (
            r"(client|wechat|qqbot|telegram):[A-Za-z0-9_-]+:[A-Za-z0-9_@.-]+",
            "routing key",
        ),
        (r"instance-[0-9]{4,}", "cloud instance name"),
        (r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "email"),
        (r"\b(?:\+?86[-\s]?)?1[3-9]\d{9}\b", "mobile number"),
        (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", "ipv4 address"),
        (r"\bSHA256:[A-Za-z0-9+/]{43}\b", "ssh fingerprint"),
        (r"\b(?:[0-9a-f]{2}:){15}[0-9a-f]{2}\b", "ssh fingerprint"),
        (
            r"\b(?:sk-[A-Za-z0-9]{20,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xoxb-[A-Za-z0-9-]{20,}|AKIA[0-9A-Z]{16})\b",
            "api token",
        ),
    ];
    for (pat, label) in families {
        if let Ok(re) = regex::Regex::new(pat) {
            for m in re.find_iter(text) {
                let hit = m.as_str();
                // Structurally IPv4 but universal constants (loopback,
                // wildcard, broadcast, public DNS resolvers): examples in
                // generic ops skills, not personal identifiers.
                if matches!(
                    hit,
                    "127.0.0.1" | "0.0.0.0" | "255.255.255.255" | "8.8.8.8" | "1.1.1.1"
                ) {
                    continue;
                }
                hits.push(format!("{}: {}", label, hit));
            }
        }
    }

    // Dynamic instance values: bare uuid directory names under users/.
    if let Ok(users) = std::fs::read_dir(users_dir) {
        for user in users.flatten() {
            if let Some(name) = user.file_name().to_str() {
                if name.len() >= 8 && text.contains(name) {
                    hits.push(format!("user dir: {}", name));
                }
            }
        }
    }

    // Current hostname (/etc/hostname).
    if let Ok(hostname) = std::fs::read_to_string("/etc/hostname") {
        let hostname = hostname.trim();
        if hostname.len() >= 4 && text.contains(hostname) {
            hits.push(format!("hostname: {}", hostname));
        }
    }

    hits.sort();
    hits.dedup();
    hits
}

fn sha256_hex(data: &str) -> String {
    use std::fmt::Write as _;
    let digest = <sha2::Sha256 as sha2::Digest>::digest(data.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        let _ = write!(hex, "{:02x}", b);
    }
    hex
}

/// Collect classification candidates: active user-layer skills that are not
/// shadowed by an agent-layer name and whose sha changed since last pass.
/// `classified_shas` is the incremental index (name → content sha at last
/// classification); a skill whose sha matches is skipped.
fn collect_candidates(
    users_dir: &Path,
    skills_root: &Path,
    classified_shas: &HashMap<String, String>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for skill_md in iter_user_skill_files(users_dir) {
        let Some(skill_dir) = skill_md.parent() else {
            continue;
        };
        let Some(owner_dir) = skill_dir.parent().and_then(|p| p.parent()) else {
            continue;
        };
        let Some(owner) = owner_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        let Some(name) = skill_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        if !is_active(&skill_md) {
            continue;
        }
        // Already in the agent layer (promoted earlier / name collision):
        // user-layer copy stays owner-private, not proposer business.
        if skills_root.join(&name).exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        let sha = sha256_hex(&content);
        // Increment: unchanged since it was last classified B/C.
        if classified_shas.get(&name).is_some_and(|prev| prev == &sha) {
            continue;
        }
        let identifiers = scan_personal_identifiers(&content, users_dir);
        out.push(Candidate {
            name,
            owner,
            dir: skill_dir.to_path_buf(),
            content,
            sha,
            identifiers,
        });
    }
    out
}

/// Build the classification prompt for one batch of candidates.
fn build_classification_prompt(batch: &[&Candidate]) -> String {
    let mut doc = String::new();
    for c in batch {
        let body: String = c.content.chars().take(PER_SKILL_CHARS).collect();
        let ids = if c.identifiers.is_empty() {
            "(none found by hard gate)".to_string()
        } else {
            c.identifiers.join("; ")
        };
        doc.push_str(&format!(
            "<skill name=\"{}\">\n<hard_gate_identifiers>{}</hard_gate_identifiers>\n{}\n</skill>\n\n",
            c.name, ids, body
        ));
    }

    format!(
        "You are the skill internalization classifier (RFC #101 §2.4). Classify each user-layer \
         skill below by cross-user generality — the same criterion memory distillation uses: \
         is this skill still bound to a single user's context, or is it universally usable?\n\
         \n\
         ## Tiers\n\
         - A: universally usable as-is. Public-service operations (market quotes, flights, \
         knowledge-base CLIs), tool-agnostic methodology with zero instance bindings. The hard \
         gate found no personal identifiers (see <hard_gate_identifiers>). A-tier skills are \
         relocated verbatim to the globally visible agent layer — if ANY identifier is listed, \
         the skill CANNOT be tier A.\n\
         - B: the methodology is general but the body carries instance bindings (local paths, \
         hostnames, account names, ports). Generalizable only after de-identification \
         (placeholders like <myclaw-repo>, <gateway-port>) with the extracted instance values \
         moved to user-layer memory. Requires operator signature.\n\
         - C: bound to personal assets, hostnames, business flows (personal blog/公众号 pipelines, \
         personal host fleets, personal content products). Stays in the user layer.\n\
         \n\
         ## Skills\n\
         {doc}\
         \n\
         ## Output format — one line per skill, nothing else:\n\
         skillname | A | one-line justification\n\
         skillname | B | one-line justification || identifiers to replace: item1; item2\n\
         skillname | C | one-line justification\n\
         \n\
         Judge from the methodology's nature, not the user's frequency of use."
    )
}

/// Parse the classifier output into (name, tier, note) triples.
fn parse_classification(output: &str) -> Vec<(String, char, String)> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '|').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            continue;
        }
        let tier = parts[1].chars().next().unwrap_or('C').to_ascii_uppercase();
        if !matches!(tier, 'A' | 'B' | 'C') {
            continue;
        }
        out.push((parts[0].to_string(), tier, parts[2].to_string()));
    }
    out
}

/// Append the round's section to the proposal file
/// ({proposals_dir}/{date}.md). Review feedback (PR #206): multiple passes
/// can fire on the same day, so each pass appends a `## pass HH:MM` section
/// instead of overwriting — earlier rounds stay auditable.
fn write_proposal_file(
    proposals_dir: &Path,
    promoted: &[(String, String)], // (name, owner)
    tier_b: &[(String, String)],   // (name, note)
    tier_c: &[(String, String)],
) -> std::path::PathBuf {
    let _ = std::fs::create_dir_all(proposals_dir);
    let now = chrono::Local::now();
    let date = now.format("%Y-%m-%d").to_string();
    let time = now.format("%H:%M").to_string();
    let path = proposals_dir.join(format!("{}.md", date));

    let mut body = String::new();
    if !path.exists() {
        body.push_str(&format!(
            "# 内化提议 {}\n\n由 skill_proposer 自动生成。A 档已直接晋升（无损搬移，可 mv 回滚）；\
             B 档待 operator 签名：确认去标识化 diff 后在会话内执行（改写入 agent 层 + 实例参数\
             写入 user 层记忆 + 删除 user 层原始版）。\n",
            date
        ));
    }
    body.push_str(&format!(
        "\n## pass {}（A {} / B {} / C {}）\n",
        time,
        promoted.len(),
        tier_b.len(),
        tier_c.len()
    ));
    body.push_str(&format!("### A 档（已晋升，{} 个）\n", promoted.len()));
    for (name, owner) in promoted {
        body.push_str(&format!(
            "- `{}` ← users/{}/skills（mv 搬移，回滚：mv 回去）\n",
            name, owner
        ));
    }
    body.push_str(&format!("### B 档（待签名，{} 个）\n", tier_b.len()));
    for (name, note) in tier_b {
        body.push_str(&format!("- `{}`：{}\n", name, note));
    }
    body.push_str(&format!("### C 档（留层，{} 个）\n", tier_c.len()));
    for (name, note) in tier_c {
        body.push_str(&format!("- `{}`：{}\n", name, note));
    }

    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(body.as_bytes()) {
                tracing::warn!(err = %e, path = %path.display(), "skill_proposer: failed to append proposal section");
            }
        }
        Err(e) => {
            tracing::warn!(err = %e, path = %path.display(), "skill_proposer: failed to open proposal file");
        }
    }
    path
}

/// Run one proposer pass. Returns (promoted_count, tier_b_count, classified)
/// on success, where `classified` maps skill name → content sha for every
/// non-promoted candidate the pass classified (B or C). The caller owns the
/// persistent ProposerState (single writer) and merges this into
/// `classified_shas` when recording the attempt — the pass itself never
/// loads or saves state (the previous two-instance pattern let the caller's
/// stale copy overwrite the pass's sha index on record_attempt).
/// Concurrent passes are prevented by a global semaphore.
pub async fn run_skill_proposer(
    input: ProposerInput,
) -> Result<(usize, usize, HashMap<String, String>)> {
    let semaphore = PROPOSER_SEMAPHORE.get_or_init(|| Semaphore::new(1));
    let permit = match semaphore.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::debug!("skill_proposer: skipped because another pass is running");
            return Ok((0, 0, HashMap::new()));
        }
    };

    tracing::info!(model = %input.model_id, "skill_proposer: starting");
    let result = tokio::time::timeout(OVERALL_TIMEOUT, run_skill_proposer_inner(input)).await;
    drop(permit);

    match result {
        Ok(Ok((promoted, tier_b, classified))) => {
            tracing::info!(
                promoted = promoted,
                tier_b = tier_b,
                "skill_proposer: finished"
            );
            Ok((promoted, tier_b, classified))
        }
        Ok(Err(e)) => {
            tracing::warn!(err = %e, "skill_proposer: failed");
            Err(e)
        }
        Err(_) => {
            let msg = format!("skill_proposer timed out after {}s", OVERALL_TIMEOUT.as_secs());
            tracing::warn!(timeout_secs = OVERALL_TIMEOUT.as_secs(), "skill_proposer: timed out");
            Err(anyhow::anyhow!(msg))
        }
    }
}

async fn run_skill_proposer_inner(
    input: ProposerInput,
) -> Result<(usize, usize, HashMap<String, String>)> {
    let candidates = collect_candidates(&input.users_dir, &input.skills_root, &input.classified);
    if candidates.is_empty() {
        tracing::debug!("skill_proposer: no new/changed user-layer skills, nothing to do");
        return Ok((0, 0, HashMap::new()));
    }

    // Batch by input budget.
    let mut batches: Vec<Vec<&Candidate>> = Vec::new();
    let mut current: Vec<&Candidate> = Vec::new();
    let mut chars = 0usize;
    for c in &candidates {
        let budget = c.content.chars().count().min(PER_SKILL_CHARS) + 512;
        if !current.is_empty() && chars + budget > MAX_INPUT_CHARS {
            batches.push(std::mem::take(&mut current));
            chars = 0;
        }
        chars += budget;
        current.push(c);
    }
    if !current.is_empty() {
        batches.push(current);
    }

    let mut promoted: Vec<(String, String)> = Vec::new();
    let mut tier_b: Vec<(String, String)> = Vec::new();
    let mut tier_c: Vec<(String, String)> = Vec::new();
    let mut by_name: HashMap<&str, &Candidate> = HashMap::new();
    for c in &candidates {
        by_name.insert(c.name.as_str(), c);
    }

    for batch in &batches {
        let prompt = build_classification_prompt(batch);
        let messages = vec![ChatMessage::user_text(prompt)];
        let req = ChatRequest {
            model: &input.model_id,
            messages: &messages,
            temperature: None,
            max_tokens: None,
            thinking: None,
            stop: None,
            seed: None,
            tools: None,
            stream: true,
        };
        let text = collect_text(input.provider.chat(req)?).await?;
        for (name, tier, note) in parse_classification(&text) {
            let Some(cand) = by_name.get(name.as_str()) else {
                continue;
            };
            match tier {
                'A' => {
                    // Hard-gate double check: the LLM cannot promote past it.
                    if !cand.identifiers.is_empty() {
                        tier_b.push((
                            name.clone(),
                            format!(
                                "LLM 判 A 但硬闸命中标识符，降级 B：{}",
                                cand.identifiers.join("; ")
                            ),
                        ));
                        continue;
                    }
                    // Name collision re-check (agent layer may have grown
                    // while we were running).
                    if input.skills_root.join(&name).exists() {
                        tier_c.push((name.clone(), "agent 层同名冲突，留层".to_string()));
                        continue;
                    }
                    match std::fs::rename(&cand.dir, input.skills_root.join(&name)) {
                        Ok(()) => {
                            tracing::info!(
                                skill = %name,
                                owner = %cand.owner,
                                "skill_proposer: tier A promoted (mv)"
                            );
                            promoted.push((name.clone(), cand.owner.clone()));
                        }
                        Err(e) => {
                            tracing::warn!(
                                err = %e,
                                skill = %name,
                                "skill_proposer: tier A move failed, deferring to B"
                            );
                            tier_b.push((name.clone(), format!("搬移失败（{}），请手动处理", e)));
                        }
                    }
                }
                'B' => tier_b.push((name.clone(), note)),
                _ => tier_c.push((name.clone(), note)),
            }
        }
    }

    // Sha index for every non-promoted candidate the pass classified (B/C).
    // Promoted skills left the user layer so they will not be re-collected.
    let mut classified = HashMap::new();
    for c in &candidates {
        if !promoted.iter().any(|(n, _)| n == &c.name) {
            classified.insert(c.name.clone(), c.sha.clone());
        }
    }

    if promoted.is_empty() && tier_b.is_empty() && tier_c.is_empty() {
        return Ok((0, 0, classified));
    }
    let path = write_proposal_file(&input.proposals_dir, &promoted, &tier_b, &tier_c);
    tracing::info!(
        proposal = %path.display(),
        promoted = promoted.len(),
        tier_b = tier_b.len(),
        tier_c = tier_c.len(),
        "skill_proposer: proposal written"
    );
    Ok((promoted.len(), tier_b.len(), classified))
}

/// Collect a chat response stream into plain text.
async fn collect_text(
    mut stream: crate::providers::BoxStream<crate::providers::StreamEvent>,
) -> Result<String> {
    use crate::providers::StreamEvent;
    use futures_util::StreamExt;
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        if let StreamEvent::Delta { text } = ev {
            out.push_str(&text);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("skill_proposer_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn draft_skills_are_inactive() {
        let d = tmp_dir("draft");
        std::fs::create_dir_all(d.join("skills/x")).unwrap();
        std::fs::write(d.join("skills/x/SKILL.md"), "---\nname: x\n  status: draft\n---\nbody").unwrap();
        assert!(!is_active(&d.join("skills/x/SKILL.md")));
        std::fs::write(d.join("skills/x/SKILL.md"), "---\nname: x\n---\nbody").unwrap();
        assert!(is_active(&d.join("skills/x/SKILL.md")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hard_gate_finds_home_paths_and_uuids() {
        let users = tmp_dir("gate");
        std::fs::create_dir_all(users.join("01a0151d-997f-7980-9ad1-cd9caf893d87")).unwrap();
        let hits = scan_personal_identifiers(
            "run cd /home/ubuntu/repo && deploy to instance-20260413-0003",
            &users,
        );
        assert!(hits.iter().any(|h| h.contains("/home/ubuntu")));
        assert!(hits.iter().any(|h| h.contains("instance-")));
        assert!(scan_personal_identifiers("plain methodology text", &users).is_empty());
        let _ = std::fs::remove_dir_all(&users);
    }

    #[test]
    fn hard_gate_finds_pii_families() {
        let users = tmp_dir("pii");
        let hits = scan_personal_identifiers(
            "contact ops@corp.example for AKIAIOSFODNN7EXAMPLE and ghp_AAAA1111BBBB2222CCCC3333 \
             at ssh root@10.2.34.56, mobile 13800138000, mail alice@corp.example",
            &users,
        );
        assert!(hits.iter().any(|h| h.starts_with("email:")));
        assert!(hits.iter().any(|h| h.starts_with("mobile number: 13800138000")));
        assert!(hits.iter().any(|h| h.starts_with("ipv4 address: 10.2.34.56")));
        assert!(hits.iter().any(|h| h.contains("AKIA")));
        assert!(hits.iter().any(|h| h.contains("ghp_")));
        let _ = std::fs::remove_dir_all(&users);
    }

    #[test]
    fn hard_gate_ignores_universal_constants_and_long_digit_runs() {
        let users = tmp_dir("consts");
        let hits = scan_personal_identifiers(
            "curl http://127.0.0.1:8080 and 0.0.0.0, dns 8.8.8.8, ts 1726000000000 ms",
            &users,
        );
        assert!(
            !hits.iter().any(|h| h.contains("ipv4")),
            "loopback/wildcard/DNS IPs must not count: {:?}",
            hits
        );
        assert!(
            !hits.iter().any(|h| h.starts_with("mobile")),
            "13-digit ms timestamps must not count as mobile numbers: {:?}",
            hits
        );
        let _ = std::fs::remove_dir_all(&users);
    }

    #[test]
    fn candidate_collection_is_incremental() {
        let d = tmp_dir("incr");
        // Real layout: users/{user}/skills/{name}/SKILL.md
        std::fs::create_dir_all(d.join("u1/skills/same")).unwrap();
        std::fs::create_dir_all(d.join("u1/skills/changed")).unwrap();
        let same = "---\nname: same\n---\nbody";
        let changed = "---\nname: changed\n---\nbody v1";
        std::fs::write(d.join("u1/skills/same/SKILL.md"), same).unwrap();
        std::fs::write(d.join("u1/skills/changed/SKILL.md"), changed).unwrap();
        let agent = tmp_dir("incr_agent");

        // Index from a previous pass: "same" recorded, "changed" recorded
        // against an older sha.
        let mut index = std::collections::HashMap::new();
        index.insert("same".to_string(), sha256_hex(same));
        index.insert("changed".to_string(), sha256_hex("---\nname: changed\n---\nold"));

        let got = collect_candidates(&d, &agent, &index);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "changed");

        // Empty index (fresh state / lost index): everything is a candidate.
        let all = collect_candidates(&d, &agent, &std::collections::HashMap::new());
        assert_eq!(all.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    fn proposal_file_appends_per_pass() {
        let dir = tmp_dir("proposal");
        let p1 = write_proposal_file(
            &dir,
            &[("skill-a".to_string(), "u1".to_string())],
            &[],
            &[],
        );
        let content1 = std::fs::read_to_string(&p1).unwrap();
        assert_eq!(content1.matches("# 内化提议").count(), 1);
        assert!(content1.contains("skill-a"));
        let _ = write_proposal_file(
            &dir,
            &[],
            &[("skill-b".to_string(), "needs rewrite".to_string())],
            &[],
        );
        let content2 = std::fs::read_to_string(&p1).unwrap();
        assert_eq!(content2.matches("# 内化提议").count(), 1, "header must not duplicate");
        assert_eq!(content2.matches("## pass").count(), 2, "one section per pass");
        assert!(content2.contains("skill-a") && content2.contains("skill-b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classification_output_parses() {
        let out = "github | A | generic gh CLI ops\nmyclaw-x | B | has local paths || identifiers to replace: /home/ubuntu; port 18789\nblog | C | personal asset\nbad line without pipes";
        let parsed = parse_classification(out);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].1, 'A');
        assert_eq!(parsed[1].1, 'B');
        assert!(parsed[1].2.contains("/home/ubuntu"));
        assert_eq!(parsed[2].1, 'C');
    }

    #[test]
    fn pending_detects_new_skills() {
        let users = tmp_dir("pending");
        std::fs::create_dir_all(users.join("u1/skills/s")).unwrap();
        std::fs::write(users.join("u1/skills/s/SKILL.md"), "---\nname: s\n---\n").unwrap();
        assert!(has_pending_user_skills(&users, None));
        assert!(!has_pending_user_skills(
            &users,
            Some("2999-01-01T00:00:00+00:00")
        ));
        let _ = std::fs::remove_dir_all(&users);
    }
}
