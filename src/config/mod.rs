//! config — Central configuration for MyClaw.
//!
//! Defines all configuration types and provides TOML file loading with
//! environment variable expansion (`${ENV_VAR}`).
//!
//! # Configuration file structure
//!
//! ```toml
//! base_dir = "~/.myclaw"
//! workspace_dir = "~/.myclaw/workspace"
//!
//! [providers.openai]
//! api_key = "${OPENAI_API_KEY}"
//!
//! [providers.openai.chat]
//! base_url = "https://api.openai.com/v1"
//!
//! [providers.openai.chat.models.gpt-4o]
//! input = ["text", "image"]
//! output = ["text"]
//! context_window = 128000
//! max_output_tokens = 16384
//!
//! [providers.openai.embedding]
//! base_url = "https://api.openai.com/v1"
//!
//! [providers.openai.embedding.models.text-embedding-3-small]
//! dimensions = 1536
//!
//! [routing.chat]
//! strategy = "fallback"
//! models = ["gpt-4o"]
//!
//! [channels.wechat]
//! api_base = "https://ilink.bot.weixin.qq.com"
//! bot_token = "${WECHAT_BOT_TOKEN}"
//!
//! [channels.telegram]
//! bot_token = "${TELEGRAM_BOT_TOKEN}"
//! allowed_users = ["*"]
//!
//! [agent]
//! permission_mode = "default"
//!
//! [[mcp_servers]]
//! name = "filesystem"
//! command = "npx"
//! args = ["mcp-server-filesystem"]
//! ```
//!
//! # 数据目录与工作区分离（storage-layout redesign §3.2）
//!
//! 系统数据分两个根：
//!
//! - **base dir**（`base_dir`，默认 `~/.myclaw`）：系统数据库 +
//!   系统配置 + 运行时状态。daemon 启动依赖的实体（`sessions/`、`users/`、
//!   `jobs/`、`memory/`、`agents/`、`skills/`、`backups/`、`state/`）全部
//!   在这里。派生路径见 [`AppConfig::sessions_root`] / [`AppConfig::memory_root`] 等方法。
//! - **workspace dir**（`workspace_dir`，默认 `{base_dir}/workspace`）：agent
//!   工作台（cwd、代码仓库、过程产物）。daemon 启动不依赖其中任何东西，
//!   可整体清理重建。

pub mod agent;
pub mod channel;
pub mod filters;
pub mod mcp;
pub mod memory;
pub mod provider;
pub mod routing;
pub mod scheduler;
pub mod sub_agent;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api::loop_breaker::LoopBreakerConfig;
use crate::config::scheduler::SchedulerConfig;
use agent::{AgentConfig, ContextConfig, PromptConfig, ToolExecutorConfig};
use channel::ChannelConfigs;
use mcp::McpServerConfig;
use memory::MemoryConfig;
use provider::ProviderConfig;
use routing::RoutingConfig;

// ── RawConfig (1:1 mapping of TOML file) ──────────────────────────────────────

/// Raw configuration as parsed from TOML — before env var expansion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawConfig {
    /// Data directory path（系统数据库 + 配置 + 运行时状态）。默认 `~/.myclaw`。
    #[serde(default)]
    base_dir: Option<String>,

    /// Workspace directory path（agent 工作台，过程产物）。
    /// 默认 `{base_dir}/workspace`。
    #[serde(default)]
    workspace_dir: Option<String>,

    /// Provider configurations.
    #[serde(default)]
    providers: HashMap<String, ProviderConfig>,

    /// Routing configuration.
    #[serde(default)]
    routing: RoutingConfig,

    /// Channel configurations.
    #[serde(default)]
    channels: ChannelConfigs,

    /// Agent configuration (`[agent]` — permission_mode only).
    #[serde(default)]
    agent: AgentConfig,

    /// Context-engine configuration (`[context_engine]`).
    #[serde(default)]
    context_engine: ContextConfig,

    /// Tool-executor configuration (`[tool_executor]`).
    #[serde(default)]
    tool_executor: ToolExecutorConfig,

    /// Loop-breaker configuration (`[loop_breaker]`).
    #[serde(default)]
    loop_breaker: LoopBreakerConfig,

    /// System prompt configuration (`[prompt]`).
    #[serde(default)]
    prompt: PromptConfig,

    /// Scheduler configuration (`[scheduler]`).
    #[serde(default)]
    scheduler: SchedulerConfig,

    /// Memory configuration (`[memory]` — idle-time distillation).
    #[serde(default)]
    memory: MemoryConfig,

    /// MCP server configurations.
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,

    /// Logging configuration.
    #[serde(default)]
    logging: LoggingConfig,

    /// Safety configuration (`[safety]` — protected paths).
    #[serde(default)]
    safety: SafetyConfig,

    /// System configuration (`[system]` — identity namespace).
    #[serde(default)]
    system: SystemConfig,

    /// Messaging configuration (`[messaging]` — SMTP).
    #[serde(default)]
    messaging: MessagingConfig,

    /// Delegation configuration (`[delegation]` — recursive nesting limit).
    #[serde(default)]
    delegation: DelegationConfig,

    /// Skills configuration (`[skills]` — `~/.agents/skills` inclusion).
    #[serde(default)]
    skills: SkillsConfig,

    /// Tool-shell environment configuration (`[shell]` — PATH fix-ups).
    #[serde(default)]
    shell: ShellConfig,
}

// ── LoggingConfig ─────────────────────────────────────────────────────────────

/// Logging configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Default log level.
    #[serde(default)]
    pub level: Option<String>,

    /// Per-module log levels.
    #[serde(default)]
    pub modules: HashMap<String, String>,
}

// ── SafetyConfig ──────────────────────────────────────────────────────────────

/// Safety configuration for protecting critical paths from agent access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    /// Glob patterns for paths the agent may neither read nor modify
    /// (write/edit/delete). Supports `~` expansion. Default includes
    /// critical system paths.
    #[serde(default = "default_protected_paths")]
    pub protected_paths: Vec<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            protected_paths: default_protected_paths(),
        }
    }
}

/// Default protected paths that the agent cannot modify.
fn default_protected_paths() -> Vec<String> {
    vec![
        "~/.ssh/**".to_string(),
        "**/.env".to_string(),
        "**/.env.*".to_string(),
    ]
}

impl SafetyConfig {
    /// Check if a path matches any protected pattern.
    pub fn is_protected(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        let expanded_path = shellexpand::tilde(&path_str).to_string();
        let expanded_path = Path::new(&expanded_path);

        for pattern in &self.protected_paths {
            let expanded_pattern = shellexpand::tilde(pattern).to_string();
            if glob_match(&expanded_pattern, &expanded_path.to_string_lossy()) {
                return true;
            }
        }
        false
    }
}

// ── SystemConfig（[system] — 实例级 namespace） ──────────────────────────────

/// System configuration (`[system]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    /// Identity namespace for user refs (`<ref id="{namespace}/u/…"/>`).
    /// Defaults to `"myclaw"`. Changing it later requires migrating
    /// persisted `users.json` / resolver bindings (see RFC §2.2).
    #[serde(default = "default_system_namespace")]
    pub namespace: String,
    /// Operator identity (FQID `myclaw/u/{uuid}` or bare uuid) — the human
    /// who runs this daemon. Used for: CLI default identity (`myclaw exec`
    /// without `--user`), draft-backlog agent-layer routing, and (P3)
    /// promote authorization.
    #[serde(default)]
    pub operator: Option<String>,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            // 无 `[system]` 段时 serde 走 Default —— namespace 必须落
            // 默认 "myclaw"（空串会让 `<ref id="/u/…"/>` 无效）。
            namespace: "myclaw".to_string(),
            operator: None,
        }
    }
}

/// Default `[system] namespace` — `"myclaw"`.
fn default_system_namespace() -> String {
    "myclaw".to_string()
}

// ── MessagingConfig（[messaging] — SMTP 配置项） ─────────────────────────────

/// Messaging configuration (`[messaging]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagingConfig {
    /// SMTP settings for verification-code email (RFC §2.2「混合验证」).
    /// Parsed only — the send-verification-code flow is a later phase.
    #[serde(default)]
    pub smtp: SmtpConfig,
}

/// SMTP configuration (`[messaging.smtp]`). All fields optional — without a
/// host, identity binding falls back to the "declaration takes effect
/// immediately" mode (RFC §2.2), which stays unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmtpConfig {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// From address for verification emails (e.g. `noreply@example.com`).
    #[serde(default)]
    pub from: Option<String>,
}

/// Delegation configuration (`[delegation]` — recursive nesting limit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationConfig {
    /// Maximum delegation depth, counting the main agent as depth 1
    /// (RFC §6). Default 3: main → sub → sub-sub allowed; deeper nesting is
    /// rejected at the tool layer (`agent_delegate` returns an error, no
    /// suspension is created).
    #[serde(default = "default_delegation_max_depth")]
    pub max_depth: u32,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self { max_depth: 3 }
    }
}

/// Default `[delegation] max_depth` — `3` (main agent = depth 1).
fn default_delegation_max_depth() -> u32 {
    3
}

// ── SkillsConfig（[skills] — 跨 agent 通用 skill 目录） ──────────────────────

/// Skills configuration (`[skills]`).
///
/// Issue #83: skill CLIs like `npx skills add` distribute skills into a
/// cross-agent shared library (`~/.agents/skills`), symlinking them into
/// the agents they know about. MyClaw isn't in that registry, so without
/// this it never sees skills installed that way. `load_skills_layered`
/// (see `agents::workspace::skill_loader`) merges that directory in, with
/// `~/.myclaw/skills` (local customization) always winning name conflicts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// Also load `~/.agents/skills` alongside the local skills root.
    /// Default on; set `false` to opt a deployment out of the shared
    /// library entirely.
    #[serde(default = "default_true")]
    pub include_agents_dir: bool,
    /// Enable the idle-time skill internalization proposer (RFC #101 §2.4).
    /// Default off — tier-A skills are relocated verbatim into the globally
    /// visible agent layer, which is a behavioral surface change; opt in.
    #[serde(default = "default_proposer_enabled")]
    pub proposer_enabled: bool,
    /// Idle threshold in seconds: no inbound messages for this long before
    /// a proposer pass may run.
    #[serde(default = "default_proposer_idle_secs")]
    pub proposer_idle_secs: u64,
    /// How often the scheduler checks for a pending proposer pass (seconds).
    #[serde(default = "default_proposer_interval_secs")]
    pub proposer_interval_secs: u64,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            include_agents_dir: true,
            proposer_enabled: default_proposer_enabled(),
            proposer_idle_secs: default_proposer_idle_secs(),
            proposer_interval_secs: default_proposer_interval_secs(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_proposer_enabled() -> bool {
    false
}
fn default_proposer_idle_secs() -> u64 {
    1800
}
fn default_proposer_interval_secs() -> u64 {
    3600
}

/// Cross-agent shared skills directory: `~/.agents/skills`. Read-only from
/// MyClaw's perspective — `skill_manage` never writes here, only to the
/// local `skills_root()`.
pub fn agents_skills_dir() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.agents/skills").to_string())
}

// ── ShellConfig（[shell] — 工具 shell 环境修补） ─────────────────────────────

/// Tool-shell environment configuration (`[shell]`).
///
/// Issue #84: the tool shell (`spawn_tracked` in `tools/shell.rs`) inherits
/// the daemon's own process environment, and the daemon is typically
/// started by a systemd user unit with no login/interactive shell in its
/// ancestry — so PATH extensions a user's `.bashrc` appends (npm global
/// prefix, nvm, pyenv, Homebrew, …) are invisible to it. `tools::shell_env`
/// fixes this in three layers: a static system-directory fallback (always
/// on), an optional one-time login-shell probe (`login_env_probe`), and
/// this escape hatch for anything the first two still miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellConfig {
    /// Probe `$SHELL -l -c 'env -0'` once at startup and merge the result
    /// into the tool-shell PATH (gaps only — existing keys are never
    /// clobbered). Default on; disable if the probe is unwanted or
    /// `$SHELL` isn't trustworthy in this deployment.
    #[serde(default = "default_true")]
    pub login_env_probe: bool,

    /// Directories appended to PATH ahead of everything else — the final
    /// escape hatch when the static fallback and login-shell probe both
    /// miss a directory a tool command needs.
    #[serde(default)]
    pub path_extra: Vec<String>,

    /// Extra environment variables injected into every tool-shell command,
    /// applied last (highest priority — including overriding `PATH`
    /// outright if set here).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            login_env_probe: true,
            path_extra: Vec::new(),
            env: HashMap::new(),
        }
    }
}

/// Simple glob matching supporting `*` (any chars except `/`) and `**` (any chars including `/`).
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    glob_match_parts(&pattern_parts, &path_parts)
}

/// Match a list of pattern segments against a list of path segments.
fn glob_match_parts(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() && path.is_empty() {
        return true;
    }
    if pattern.is_empty() {
        return false;
    }

    let pat = pattern[0];
    let rest_pat = &pattern[1..];

    if pat == "**" {
        // ** matches zero or more path segments
        for i in 0..=path.len() {
            if glob_match_parts(rest_pat, &path[i..]) {
                return true;
            }
        }
        return false;
    }

    if path.is_empty() {
        return false;
    }

    let p = path[0];
    let rest_path = &path[1..];

    if glob_match_segment(pat, p) {
        return glob_match_parts(rest_pat, rest_path);
    }

    false
}

fn glob_match_segment(pattern: &str, segment: &str) -> bool {
    // Simple wildcard matching within a single path segment.
    // Supports `*` (any chars) and `?` (single char).
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = segment.chars().collect();
    glob_match_chars(&p, &s)
}

fn glob_match_chars(pat: &[char], seg: &[char]) -> bool {
    match (pat.first(), seg.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some('*'), _) => {
            // * matches zero or more characters
            glob_match_chars(&pat[1..], seg)
                || (!seg.is_empty() && glob_match_chars(pat, &seg[1..]))
        }
        (Some('?'), Some(_)) => glob_match_chars(&pat[1..], &seg[1..]),
        (Some(p), Some(s)) if p == s => glob_match_chars(&pat[1..], &seg[1..]),
        _ => false,
    }
}

// ── AppConfig (public, resolved) ──────────────────────────────────────────────

/// Fully resolved application configuration.
///
/// This is the main config type consumed by all subsystems.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Data directory (absolute path) — system database + config + runtime
    /// state (sessions/users/jobs/memory/agents/skills/…). See the module
    /// docs for the base-dir / workspace split.
    pub base_dir: PathBuf,
    /// Workspace directory (absolute path) — agent working area (process
    /// artifacts, repositories). Defaults to `{base_dir}/workspace`.
    pub workspace_dir: PathBuf,
    /// Path to the config file.
    pub config_path: PathBuf,
    /// Provider configurations.
    pub providers: HashMap<String, ProviderConfig>,
    /// Routing configuration.
    pub routing: RoutingConfig,
    /// Channel configurations.
    pub channels: ChannelConfigs,
    /// Agent configuration (`[agent]` — permission_mode only).
    pub agent: AgentConfig,
    /// Context-engine configuration (`[context_engine]`).
    pub context_engine: ContextConfig,
    /// Tool-executor configuration (`[tool_executor]`).
    pub tool_executor: ToolExecutorConfig,
    /// Loop-breaker configuration (`[loop_breaker]`).
    pub loop_breaker: LoopBreakerConfig,
    /// System prompt configuration (`[prompt]`).
    pub prompt: PromptConfig,
    /// Scheduler configuration (`[scheduler]`).
    pub scheduler: SchedulerConfig,
    /// Memory configuration (`[memory]` — idle-time distillation).
    pub memory: MemoryConfig,
    /// MCP server configurations.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Logging configuration.
    pub logging: LoggingConfig,
    /// Safety configuration (`[safety]` — protected paths).
    pub safety: SafetyConfig,
    /// System configuration (`[system]` — identity namespace).
    pub system: SystemConfig,
    /// Messaging configuration (`[messaging]` — SMTP).
    pub messaging: MessagingConfig,
    /// Delegation configuration (`[delegation]` — recursive nesting limit).
    pub delegation: DelegationConfig,
    /// Skills configuration (`[skills]` — `~/.agents/skills` inclusion).
    pub skills: SkillsConfig,
    /// Tool-shell environment configuration (`[shell]` — PATH fix-ups).
    pub shell: ShellConfig,
}

use std::sync::OnceLock;

static SAFETY_CONFIG: OnceLock<SafetyConfig> = OnceLock::new();

/// Default base dir — `~/.myclaw`. Single source of truth: everything else
/// that needs this default (migration.rs, TelegramChannel's telegram_offset
/// fallback, migrate-layout.py) must derive from this, not recompute its own
/// platform-specific guess — a prior XDG-based implementation here diverged
/// from those other call sites' hardcoded `~/.myclaw` assumption and caused
/// data to split across two untracked directory trees on real deployments.
pub fn default_base_dir() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.myclaw").to_string())
}

/// Runtime-state root: `{base_dir}/state`. Free function (not an `AppConfig`
/// method) because a few callers — `OrchestratorParts`, `TelegramChannel`,
/// `UserRegistry`, `migration.rs` — only carry their own copied `base_dir`,
/// not the full `AppConfig`; this is the single source of truth both they
/// and `AppConfig`'s own methods below delegate to.
pub fn state_root(base_dir: &Path) -> PathBuf {
    base_dir.join("state")
}

/// User entity root: `{base_dir}/users`. Free function for the same reason
/// as `state_root` — `UserRegistry` also only carries a copied `base_dir`.
pub fn users_root(base_dir: &Path) -> PathBuf {
    base_dir.join("users")
}

/// Inbound message spool: `{base_dir}/state/inbound_spool`.
pub fn inbound_spool_dir(base_dir: &Path) -> PathBuf {
    state_root(base_dir).join("inbound_spool")
}

/// Delegation completion queue: `{base_dir}/state/completion_queue`.
pub fn completion_queue_dir(base_dir: &Path) -> PathBuf {
    state_root(base_dir).join("completion_queue")
}

/// Memory-tool audit log dir: `{base_dir}/state/memory/.audit`.
pub fn memory_audit_dir(base_dir: &Path) -> PathBuf {
    state_root(base_dir).join("memory").join(".audit")
}

/// Idle-time memory distillation state dir: `{base_dir}/state/memory`
/// (sibling of the memory pool itself, not inside it — see
/// `agents/orchestrator/scheduled.rs`).
pub fn memory_distill_state_dir(base_dir: &Path) -> PathBuf {
    state_root(base_dir).join("memory")
}

/// Skill proposer state dir: `{base_dir}/state/skill-proposer` (runtime
/// state, mirrors `memory_distill_state_dir`).
pub fn skill_proposer_state_dir(base_dir: &Path) -> PathBuf {
    state_root(base_dir).join("skill-proposer")
}

/// Skill proposal files dir: `{base_dir}/skill-proposals` — operator-facing
/// tier-B proposals awaiting signature.
pub fn skill_proposals_dir(base_dir: &Path) -> PathBuf {
    base_dir.join("skill-proposals")
}

/// Telegram polling offset file: `{base_dir}/telegram_offset`.
pub fn telegram_offset_path(base_dir: &Path) -> PathBuf {
    base_dir.join("telegram_offset")
}

/// `UserResolver` persistence file: `{base_dir}/user_resolver.json`.
pub fn user_resolver_path(base_dir: &Path) -> PathBuf {
    base_dir.join("user_resolver.json")
}

/// `KnownUsersRegistry` persistence file: `{base_dir}/known_users.json`.
pub fn known_users_path(base_dir: &Path) -> PathBuf {
    base_dir.join("known_users.json")
}

impl AppConfig {
    /// Session storage root: `{base_dir}/sessions`.
    pub fn sessions_root(&self) -> PathBuf {
        self.base_dir.join("sessions")
    }

    /// User entity root: `{base_dir}/users`.
    pub fn users_root(&self) -> PathBuf {
        users_root(&self.base_dir)
    }

    /// Job (cron/webhook) entity root: `{base_dir}/jobs`.
    pub fn jobs_root(&self) -> PathBuf {
        self.base_dir.join("jobs")
    }

    /// Memory wiki root: `{base_dir}/memory`.
    pub fn memory_root(&self) -> PathBuf {
        self.base_dir.join("memory")
    }

    /// Skills root: `{base_dir}/skills`.
    pub fn skills_root(&self) -> PathBuf {
        self.base_dir.join("skills")
    }

    /// Cross-agent shared skills dir (`~/.agents/skills`), or `None` when
    /// `[skills] include_agents_dir = false`. See `SkillsConfig` (issue #83).
    pub fn agents_skills_dir_opt(&self) -> Option<PathBuf> {
        self.skills.include_agents_dir.then(agents_skills_dir)
    }

    /// Sub-agent definitions root: `{base_dir}/agents`.
    pub fn agents_root(&self) -> PathBuf {
        self.base_dir.join("agents")
    }

    /// Migration backups root: `{base_dir}/backups`.
    pub fn backups_root(&self) -> PathBuf {
        self.base_dir.join("backups")
    }

    /// Runtime-state root: `{base_dir}/state`.
    pub fn state_root(&self) -> PathBuf {
        state_root(&self.base_dir)
    }

    /// Inbound message spool: `{base_dir}/state/inbound_spool`.
    pub fn inbound_spool_dir(&self) -> PathBuf {
        inbound_spool_dir(&self.base_dir)
    }

    /// Delegation completion queue: `{base_dir}/state/completion_queue`.
    pub fn completion_queue_dir(&self) -> PathBuf {
        completion_queue_dir(&self.base_dir)
    }

    /// Memory-tool audit log dir: `{base_dir}/state/memory/.audit`.
    pub fn memory_audit_dir(&self) -> PathBuf {
        memory_audit_dir(&self.base_dir)
    }

    /// Telegram polling offset file: `{base_dir}/telegram_offset`.
    pub fn telegram_offset_path(&self) -> PathBuf {
        telegram_offset_path(&self.base_dir)
    }

    /// `UserResolver` persistence file: `{base_dir}/user_resolver.json`.
    pub fn user_resolver_path(&self) -> PathBuf {
        user_resolver_path(&self.base_dir)
    }

    /// `KnownUsersRegistry` persistence file: `{base_dir}/known_users.json`.
    pub fn known_users_path(&self) -> PathBuf {
        known_users_path(&self.base_dir)
    }

    /// Proactive-message target memory: `{workspace_dir}/.last_channel`.
    pub fn last_channel_path(&self) -> PathBuf {
        self.workspace_dir.join(".last_channel")
    }

    /// Proactive-message target memory: `{workspace_dir}/.last_recipient`.
    pub fn last_recipient_path(&self) -> PathBuf {
        self.workspace_dir.join(".last_recipient")
    }

    /// Delegation sandbox roots: `{workspace_dir}/worktrees`.
    pub fn worktrees_root(&self) -> PathBuf {
        self.workspace_dir.join("worktrees")
    }
}

/// Initialize the global safety config. Called once at daemon startup.
/// Subsequent calls are no-ops (the first config wins).
pub fn init_safety_config(config: SafetyConfig) {
    let _ = SAFETY_CONFIG.set(config);
}

/// Check if a path is protected: agents may neither read nor modify
/// (write/edit/delete) it, regardless of how the path was reached
/// (workspace-relative, session-media marker, absolute, etc.) — this is the
/// one location-independent backstop, distinct from and in addition to any
/// per-tool path-resolution rules.
/// Returns `true` when the global safety config has not been initialized
/// **and** the path is within the default protected set, or when the path
/// matches any pattern in the configured `protected_paths`.
pub fn is_path_protected(path: &Path) -> bool {
    let config = SAFETY_CONFIG.get();
    match config {
        Some(c) => c.is_protected(path),
        None => SafetyConfig::default().is_protected(path),
    }
}

// ── ConfigLoader ──────────────────────────────────────────────────────────────

/// Loads and resolves configuration from TOML files.
pub struct ConfigLoader;

impl ConfigLoader {
    /// Load configuration from a TOML file.
    ///
    /// Resolves `~` in paths and `${ENV_VAR}` in string values.
    pub fn from_file(path: impl AsRef<Path>) -> Result<AppConfig> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let raw: RawConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Self::resolve(raw, path.to_path_buf())
    }

    /// Load from a TOML string (useful for testing).
    pub fn from_toml(content: &str) -> Result<AppConfig> {
        let raw: RawConfig = toml::from_str(content).context("Failed to parse config string")?;
        Self::resolve(raw, PathBuf::new())
    }

    /// Resolve a raw config into an AppConfig.
    fn resolve(mut raw: RawConfig, config_path: PathBuf) -> Result<AppConfig> {
        // Expand environment variables in all string fields.
        Self::expand_env_vars(&mut raw);

        // Resolve base_dir: explicit path or the default_base_dir() base
        // (~/.myclaw) that everything else (workspace_dir, memory_root(),
        // sessions_root(), users_root(), etc.) nests under.
        let base_dir = raw
            .base_dir
            .map(|dir| Self::expand_path(&dir))
            .unwrap_or_else(default_base_dir);

        // Resolve workspace_dir: explicit path or default `{base_dir}/workspace`.
        let workspace_dir = raw
            .workspace_dir
            .map(|dir| Self::expand_path(&dir))
            .unwrap_or_else(|| base_dir.join("workspace"));

        // Memory root is not configurable — always derived from base_dir
        // (`{base_dir}/memory`, see `AppConfig::memory_root`), like
        // sessions/users/jobs/skills/agents/backups.

        Ok(AppConfig {
            base_dir,
            workspace_dir,
            config_path,
            providers: raw.providers,
            routing: raw.routing,
            channels: raw.channels,
            agent: raw.agent,
            context_engine: raw.context_engine,
            tool_executor: raw.tool_executor,
            loop_breaker: raw.loop_breaker,
            prompt: raw.prompt,
            scheduler: raw.scheduler,
            memory: raw.memory,
            mcp_servers: raw.mcp_servers,
            logging: raw.logging,
            safety: raw.safety,
            system: raw.system,
            messaging: raw.messaging,
            delegation: raw.delegation,
            skills: raw.skills,
            shell: raw.shell,
        })
    }

    /// Expand `${ENV_VAR}` patterns in all string fields.
    fn expand_env_vars(config: &mut RawConfig) {
        // Expand provider-level and capability-level api_keys.
        for provider in config.providers.values_mut() {
            if let Some(ref key) = provider.api_key {
                provider.api_key = Some(Self::expand_string(key));
            }
            // Expand multi-key rotation list
            for key in provider.api_keys.iter_mut() {
                *key = Self::expand_string(key);
            }
            // Expand capability-level api_keys
            if let Some(ref mut chat) = provider.chat {
                if let Some(ref key) = chat.api_key {
                    chat.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut emb) = provider.embedding {
                if let Some(ref key) = emb.api_key {
                    emb.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.image_generation {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.tts {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.stt {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.video {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.search {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
        }

        // Expand channel tokens (iterate over accounts).
        if let Some(ref mut wechat) = config.channels.wechat {
            for (_, account) in wechat.accounts.iter_mut() {
                if let Some(ref token) = account.bot_token {
                    account.bot_token = Some(Self::expand_string(token));
                }
                if let Some(ref key) = account.aes_key {
                    account.aes_key = Some(Self::expand_string(key));
                }
            }
        }
        if let Some(ref mut tg) = config.channels.telegram {
            for (_, account) in tg.accounts.iter_mut() {
                account.bot_token = Self::expand_string(&account.bot_token);
            }
        }

        // Expand messaging SMTP fields (passwords/usernames commonly come from env).
        if let Some(ref mut host) = config.messaging.smtp.host {
            *host = Self::expand_string(host);
        }
        if let Some(ref mut username) = config.messaging.smtp.username {
            *username = Self::expand_string(username);
        }
        if let Some(ref mut password) = config.messaging.smtp.password {
            *password = Self::expand_string(password);
        }
        if let Some(ref mut from) = config.messaging.smtp.from {
            *from = Self::expand_string(from);
        }
    }

    /// Expand `${VAR}` and `$VAR` patterns in a string.
    fn expand_string(s: &str) -> String {
        shellexpand::env(s).unwrap_or_else(|_| s.into()).to_string()
    }

    /// Expand `~` and create absolute path.
    fn expand_path(path: &str) -> PathBuf {
        let expanded = shellexpand::tilde(path).to_string();
        PathBuf::from(expanded)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::capability::Capability;

    #[test]
    fn parse_minimal_config() {
        let toml_str = r#"
[providers.openai]

[providers.openai.chat]
base_url = "https://api.openai.com/v1"

[providers.openai.chat.models.gpt-4o]
input = ["text"]
output = ["text"]

[routing.chat]
strategy = "fixed"
models = ["gpt-4o"]
"#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();
        assert!(config.providers.contains_key("openai"));
        assert_eq!(config.routing.len(), 1);
        assert!(config.routing.get(Capability::Chat).is_some());
    }

    #[test]
    fn parse_empty_config() {
        let config = ConfigLoader::from_toml("").unwrap();
        assert!(config.providers.is_empty());
        assert!(config.channels.enabled_channels().is_empty());
        assert_eq!(config.agent.permission_mode, agent::PermissionMode::Default);
    }

    #[test]
    fn default_base_dir_is_dot_myclaw_under_home() {
        let home = shellexpand::tilde("~").to_string();
        assert_eq!(default_base_dir(), PathBuf::from(home).join(".myclaw"));
    }

    #[test]
    fn unset_base_dir_puts_workspace_and_memory_under_the_default() {
        let config = ConfigLoader::from_toml("").unwrap();
        assert_eq!(config.base_dir, default_base_dir());
        assert_eq!(config.workspace_dir, config.base_dir.join("workspace"));
        assert_eq!(config.memory_root(), config.base_dir.join("memory"));
        assert_eq!(config.sessions_root(), config.base_dir.join("sessions"));
        assert_eq!(config.users_root(), config.base_dir.join("users"));
    }

    #[test]
    fn explicit_base_dir_still_bases_workspace_and_memory_on_it() {
        let toml_str = r#"base_dir = "/tmp/myclaw-explicit-base-dir""#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();
        assert_eq!(
            config.base_dir,
            PathBuf::from("/tmp/myclaw-explicit-base-dir")
        );
        assert_eq!(config.workspace_dir, config.base_dir.join("workspace"));
        assert_eq!(config.memory_root(), config.base_dir.join("memory"));
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
workspace_dir = "/tmp/myclaw"

[providers.openai]
api_key = "test-key"

[providers.openai.chat]
base_url = "https://api.openai.com/v1"

[providers.openai.chat.models.gpt-4o]
input = ["text", "image"]
output = ["text"]
max_output_tokens = 16384
context_window = 128000

[providers.openai.chat.models.gpt-4o.pricing]
input = 2.5
output = 10.0

[providers.openai.embedding]
base_url = "https://api.openai.com/v1"

[providers.openai.embedding.models.text-embedding-3-small]
dimensions = 1536

[routing.chat]
strategy = "fallback"
models = ["gpt-4o"]

[channels.wechat]
enabled = true

[channels.wechat.accounts.default]
api_base = "https://ilink.bot.weixin.qq.com"
bot_token = "test-wechat-token"
allowed_users = ["wxid_abc123"]

[channels.telegram]
enabled = true

[channels.telegram.accounts.default]
bot_token = "test-telegram-token"
allowed_users = ["*"]

[agent]
permission_mode = "full"

[prompt]

[loop_breaker]
max_tool_calls = 50

[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["mcp-server-filesystem"]

[logging]
level = "INFO"
"#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();

        // Workspace
        assert_eq!(config.workspace_dir, PathBuf::from("/tmp/myclaw"));

        // Providers
        assert_eq!(config.providers.len(), 1);
        let openai = &config.providers["openai"];
        assert!(openai.chat.is_some());
        let chat = openai.chat.as_ref().unwrap();
        assert_eq!(chat.base_url, "https://api.openai.com/v1");
        assert!(chat.models.contains_key("gpt-4o"));

        // Routing
        let chat_route = config.routing.get(Capability::Chat).unwrap();
        assert_eq!(chat_route.models, vec!["gpt-4o"]);

        // Channels
        assert_eq!(
            config.channels.enabled_channels(),
            vec!["wechat", "telegram"]
        );

        // Agent / loop breaker
        assert_eq!(config.agent.permission_mode, agent::PermissionMode::Full);
        assert_eq!(config.loop_breaker.max_tool_calls, 50);

        // MCP
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "filesystem");

        // Logging
        assert_eq!(config.logging.level.as_deref(), Some("INFO"));
    }

    #[test]
    fn env_var_expansion() {
        unsafe {
            std::env::set_var("TEST_MYCLAW_KEY", "secret123");
        }

        let toml_str = r#"
[providers.test]

[providers.test.chat]
base_url = "https://api.test.com"
api_key = "${TEST_MYCLAW_KEY}"

[providers.test.chat.models.test-model]
input = ["text"]
output = ["text"]
"#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();
        assert_eq!(
            config.providers["test"]
                .chat
                .as_ref()
                .unwrap()
                .api_key
                .as_deref(),
            Some("secret123")
        );

        unsafe {
            std::env::remove_var("TEST_MYCLAW_KEY");
        }
    }

    #[test]
    fn test_safety_default_protected_paths() {
        let safety = SafetyConfig::default();
        assert!(!safety.protected_paths.is_empty());
        assert!(safety.protected_paths.contains(&"~/.ssh/**".to_string()));
    }

    #[test]
    fn test_safety_protected_check() {
        let safety = SafetyConfig::default();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
        // SSH key path should be protected
        assert!(safety.is_protected(std::path::Path::new(&format!("{}/.ssh/id_rsa", home))));
        // Regular workspace file should not be protected
        assert!(!safety.is_protected(std::path::Path::new(&format!(
            "{}/.myclaw/workspace/test.rs",
            home
        ))));
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match(
            "/home/user/.ssh/id_rsa",
            "/home/user/.ssh/id_rsa"
        ));
        assert!(glob_match("/home/user/.ssh/*", "/home/user/.ssh/id_rsa"));
        assert!(!glob_match(
            "/home/user/.ssh/*",
            "/home/user/.ssh/sub/id_rsa"
        ));
        assert!(glob_match(
            "/home/user/.ssh/**",
            "/home/user/.ssh/sub/id_rsa"
        ));
        assert!(glob_match("**/.env", "/any/path/.env"));
        assert!(glob_match("**/.env", "/home/user/project/.env"));
        assert!(!glob_match("**/.env", "/home/user/project/.env.local"));
        assert!(glob_match("**/.env.*", "/home/user/project/.env.local"));
        assert!(glob_match("~/.myclaw/myclaw.toml", "~/.myclaw/myclaw.toml"));
    }

    #[test]
    fn test_system_namespace_defaults_to_myclaw() {
        // 无 [system] 段 → namespace 默认 "myclaw"
        let config = ConfigLoader::from_toml("").unwrap();
        assert_eq!(config.system.namespace, "myclaw");
        assert!(config.messaging.smtp.host.is_none());
    }

    #[test]
    fn test_delegation_max_depth_defaults_to_3() {
        // 无 [delegation] 段 → max_depth 默认 3（主 agent 层=1）
        let config = ConfigLoader::from_toml("").unwrap();
        assert_eq!(config.delegation.max_depth, 3);
    }

    #[test]
    fn test_delegation_max_depth_parse() {
        // 显式 [delegation] max_depth → 覆盖默认值
        let config = ConfigLoader::from_toml(
            r#"
[delegation]
max_depth = 5
"#,
        )
        .unwrap();
        assert_eq!(config.delegation.max_depth, 5);
    }

    #[test]
    fn test_system_namespace_and_messaging_smtp_parse() {
        unsafe {
            std::env::set_var("TEST_SMTP_PASSWORD", "smtp-secret");
        }
        let toml_str = r#"
[system]
namespace = "brand"

[messaging.smtp]
host = "smtp.example.com"
port = 587
username = "noreply@example.com"
password = "${TEST_SMTP_PASSWORD}"
from = "noreply@example.com"
"#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();
        assert_eq!(config.system.namespace, "brand");
        let smtp = &config.messaging.smtp;
        assert_eq!(smtp.host.as_deref(), Some("smtp.example.com"));
        assert_eq!(smtp.port, Some(587));
        assert_eq!(smtp.username.as_deref(), Some("noreply@example.com"));
        // ${ENV} 展开
        assert_eq!(smtp.password.as_deref(), Some("smtp-secret"));
        assert_eq!(smtp.from.as_deref(), Some("noreply@example.com"));

        unsafe {
            std::env::remove_var("TEST_SMTP_PASSWORD");
        }
    }

    #[test]
    fn test_skills_config_defaults_to_including_agents_dir() {
        let config = ConfigLoader::from_toml("").unwrap();
        assert!(config.skills.include_agents_dir);
        assert_eq!(config.agents_skills_dir_opt(), Some(agents_skills_dir()));
    }

    #[test]
    fn test_skills_config_can_disable_agents_dir() {
        let config = ConfigLoader::from_toml(
            r#"
[skills]
include_agents_dir = false
"#,
        )
        .unwrap();
        assert!(!config.skills.include_agents_dir);
        assert_eq!(config.agents_skills_dir_opt(), None);
    }

    #[test]
    fn test_shell_config_defaults() {
        let config = ConfigLoader::from_toml("").unwrap();
        assert!(config.shell.login_env_probe);
        assert!(config.shell.path_extra.is_empty());
        assert!(config.shell.env.is_empty());
    }

    #[test]
    fn test_shell_config_parse() {
        let config = ConfigLoader::from_toml(
            r#"
[shell]
login_env_probe = false
path_extra = ["/opt/custom/bin"]

[shell.env]
FOO = "bar"
"#,
        )
        .unwrap();
        assert!(!config.shell.login_env_probe);
        assert_eq!(config.shell.path_extra, vec!["/opt/custom/bin".to_string()]);
        assert_eq!(config.shell.env.get("FOO"), Some(&"bar".to_string()));
    }
}
