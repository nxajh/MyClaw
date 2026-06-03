# MyClaw 架构文档

> 自动生成自源码结构提取，覆盖全部 181 个 Rust 源文件。

## 目录

| 模块 | 文件数 | 说明 |
|---|---|---|
| `main.rs / lib.rs / daemon.rs` | — | 入口与工具 |
| `signal / hot_switch / sys_info / str_utils` | — | 入口与工具 |
| `agents/` | 59 | 智能体系统：Agent 运行循环、Turn 管理、Session 管理、上下文引擎、工具执行、调度器、子代理委派、Orchestrator 事件编排(责任链 inbound) |
| `channels/` | 15 | 消息通道：多平台适配（Telegram/QQBot/微信/浏览器 Client）、消息分块、流式推送、安全策略 |
| `cli/` | 14 | 命令行界面：myclaw chat/status/reload/restart/stop/update/config/doctor/tools/tui/completion/exec 子命令 |
| `config/` | 9 | 配置系统：TOML 配置解析、Agent/Channel/Provider/Routing/Scheduler/MCP/SubAgent 配置结构 |
| `mcp/` | 11 | MCP 协议客户端：Model Context Protocol 的 HTTP/SSE/STDIO 传输、工具发现与调用 |
| `memory/` | 1 | 持久化记忆：用户/项目级 key-value 记忆存储 |
| `providers/` | 32 | LLM Provider 系统：多供应商适配（OpenAI/Anthropic/Google/GLM/Kimi/MiniMax/Xiaomi）、能力协商、流式传输、工具调用 |
| `registry/` | 2 | 服务注册表：Provider 和 Channel 的注册与路由 |
| `storage/` | 7 | 存储后端：JSON 文件存储、Session 持久化、共享/私有 KV 存储 |
| `tools/` | 22 | 内置工具集：文件操作、Shell 执行、Web 搜索/请求、记忆管理、任务管理、代理委派、技能系统 |
| `tui/` | 2 | 终端 UI：基于 ratatui 的交互式终端界面 |

---

## 顶层文件

### `daemon.rs`

**用途**: 守护进程主循环：加载配置、初始化所有子系统、启动 HTTP/TUI/Channel 监听

**依赖模块**: `agents`, `channels`

```rust
const DEFAULT_CONFIG_PATHS: &[&str] = &[
```

```rust
pub fn load_config() -> Result<crate::config::AppConfig>
```

```rust
pub fn load_config_from(path: &str) -> Result<crate::config::AppConfig>
```

```rust
fn bind_reusable(port: u16) -> anyhow::Result<std::net::TcpListener>
```

```rust
pub fn init_tracing(config: &crate::config::AppConfig)
```

```rust
fn print_banner(config: &crate::config::AppConfig, mcp_servers: usize, mcp_tools: usize, sub_agent_count: usize, sub_agent_names: &[String])
```

```rust
fn build_registry(config: &crate::config::AppConfig) -> anyhow::Result<crate::registry::Registry>
```

```rust
async fn build_tools(
```

```rust
fn build_skill_manager(workspace_dir: &std::path::Path) -> SkillManager
```

```rust
fn build_sub_agents(workspace_dir: &std::path::Path) -> Vec<crate::config::sub_agent::SubAgentConfig>
```

```rust
fn build_session_backend(config: &crate::config::AppConfig) -> Arc<dyn crate::storage::SessionBackend>
```

```rust
fn build_channel_accounts(config: &crate::config::AppConfig) -> Vec<(String, String, Arc<dyn Channel>)>
```

```rust
fn build_prompt_config(
```

```rust
pub async fn run(config: crate::config::AppConfig) -> Result<()>
```

```rust
async fn wait_for_signal() -> Result<()>
```

```rust
fn reset_telegram_offset()
```

### `hot_switch.rs`

**用途**: 热切换：运行时替换 Provider 注册表、工具注册表等共享状态

```rust
pub const ENV_HOT_SWITCH: &str = "MYCLAW_HOT_SWITCH"
```

```rust
pub const ENV_SOCKET_FD: &str = "MYCLAW_SOCKET_FD"
```

```rust
pub const ENV_CLIENT_SOCKET_FD: &str = "MYCLAW_CLIENT_SOCKET_FD"
```

```rust
pub const ENV_OLD_PID: &str = "MYCLAW_OLD_PID"
```

```rust
pub fn is_hot_switch() -> bool
```

```rust
pub fn inherited_socket_fd() -> Option<i32>
```

```rust
pub fn inherited_client_socket_fd() -> Option<i32>
```

```rust
pub fn old_pid() -> Option<i32>
```

```rust
fn build_child_envp(socket_fd: i32, client_fd: i32, current_pid: u32) -> anyhow::Result<Vec<CString>>
```

```rust
pub fn do_hot_switch(socket_fd: i32, client_fd: i32) -> anyhow::Result<()>
```

### `lib.rs`

**用途**: 库入口，声明所有顶级模块（agents, channels, cli, config, mcp, memory, providers, registry, storage, tools, tui）

```rust
pub fn is_shutting_down() -> bool
```

### `main.rs`

**用途**: 程序入口，解析 CLI 参数并启动 daemon 或 TUI

```rust
async fn main() -> Result<()>
```

```rust
fn resolve_config_or_die(explicit_path: Option<&str>) -> myclaw::config::AppConfig
```

### `signal.rs`

**用途**: Unix 信号处理（SIGTERM, SIGHUP, SIGUSR1）用于优雅关闭和热重载

```rust
pub fn pid_file_path() -> PathBuf
```

```rust
pub fn find_daemon_pid() -> Result<i32>
```

```rust
pub fn send_signal(sig: i32) -> Result<()>
```

```rust
pub fn send_sighup() -> Result<()>
```

```rust
pub fn send_sigterm() -> Result<()>
```

```rust
pub fn send_sigusr1() -> Result<()>
```

### `str_utils.rs`

**用途**: 字符串工具函数（UTF-16 长度计算、Unicode 截断、emoji 感知分块）

```rust
pub fn char_offset(s: &str, max_chars: usize) -> usize
```

```rust
pub fn truncate_chars(s: &str, max_chars: usize) -> &str
```

```rust
pub fn truncate_line(s: &str, max_chars: usize) -> String
```

```rust
pub fn parse_front_matter(content: &str) -> (String, String)
```

```rust
pub fn extract_yaml_string(yaml: &str, key: &str) -> Option<String>
```

```rust
pub fn extract_yaml_list(yaml: &str, key: &str) -> Vec<String>
```

```rust
pub fn extract_yaml_bool(yaml: &str, key: &str) -> Option<bool>
```

```rust
fn test_parse_front_matter()
```

```rust
fn test_parse_front_matter_none()
```

```rust
fn test_extract_yaml_string()
```

```rust
fn test_extract_yaml_list_inline()
```

```rust
fn test_extract_yaml_list_multiline()
```

```rust
fn test_extract_yaml_bool()
```

### `sys_info.rs`

**用途**: 系统信息采集（OS、内存、磁盘、CPU）用于 /status 和遥测

```rust
pub fn os_version() -> String
```

```rust
pub fn shell() -> String
```

```rust
pub fn runtime_info() -> String
```

```rust
fn runtime_info_is_nonempty()
```

---

## `agents/`

**模块说明**: 智能体系统：Agent 运行循环、Turn 管理、Session 管理、上下文引擎、工具执行、调度器、子代理委派

**外部模块依赖**: `AgentRegistry`, `CommandContext`, `apply_and_persist_override`, `backend`, `channels`, `config`, `get_history`, `info`, `providers`, `recovery`, `session`, `session_override`, `skill_loader`, `skills`, `storage`, `str_utils`, `tokens`, `tool_registry`, `tools`, `types`, `workspace`

#### `agents/agent.rs`

**结构体** `Agent`:
```rust
pub struct Agent {
  pub config: SubAgentConfig,
}
```

**Impl** `impl Agent`:
```rust
  pub fn new(config: SubAgentConfig) -> Self
  pub async fn run(
  pub async fn run_recovery(
  fn allowed_tools(&self, runtime: &AgentRuntime) -> Vec<Arc<dyn crate::providers::Tool>>
```

```rust
fn persist_last(session: &mut Session)
```

```rust
// 在 clone 出的 messages 上把媒体规范化为文本（每模态走 adapt_modality(spec)：
// 历史媒体→缓存复用/占位符，当轮媒体→辅助模型转述/转写）。永不修改持久化 history。
//   音频(AUDIO_SPEC)：始终适配（chat 协议无法承载音频，即便音频模型也转写）
//   图片(IMAGE_SPEC)：仅当 !model_supports_images 时适配（视觉模型用原生 ImageB64）
// 在初次构建 messages 后、以及每次 compaction 重建后各调用一次。
async fn adapt_media_for_model(
    messages: &mut [ChatMessage],
    runtime: &AgentRuntime,
    session_id: &str,
    model_supports_images: bool,
)
async fn adapt_modality(messages, runtime, session_id, spec: &ModalitySpec) // 单模态历史+当轮适配
```

> 多模态：当轮图片由 `session_context::process_turn` 经 `add_user_with_media` 持久化进
> history，故 `run()` 的 `messages` clone 已天然携带图片；旧的「从 `last_message` 快照拼回图片」
> 路径已删除（否则视觉模型会重复附图）。非视觉模型的媒体适配见 `agents/modality_adapter.rs`。

```rust
fn last_user_text(session: &Session) -> String
```

**结构体** `CollectedResponse`:
```rust
priv struct CollectedResponse {
  text: String,
  reasoning_content: Option<String>,
  thinking_signature: Option<String>,
  tool_calls: Vec<ToolCall>,
  stop_reason: StopReason,
  usage: Option<crate::providers::ChatUsage>,
}
```

```rust
async fn push_or_drop(
```

```rust
async fn collect_stream(
```

```rust
fn empty_config() -> SubAgentConfig
```

```rust
fn agent_holds_config()
```

```rust
fn session_persist_field_default_none()
```

```rust
fn events_to_stream(events: Vec<crate::providers::StreamEvent>) -> BoxStream<crate::providers::StreamEvent>
```

```rust
async fn collect_stream_accepts_thinking_without_signature()
```

```rust
async fn collect_stream_accepts_thinking_with_signature()
```

```rust
async fn collect_stream_accepts_no_thinking()
```

```rust
async fn collect_stream_rejects_truncated_stream()
```

```rust
async fn collect_stream_propagates_provider_error()
```

#### `agents/agent_registry.rs`

**结构体** `AgentRegistry`:
```rust
pub struct AgentRegistry {
  inner: Arc<RwLock<HashMap<String, Arc<Agent>>>>,
}
```

**Impl** `impl AgentRegistry`:
```rust
  pub fn new() -> Self
  pub fn from_vec(configs: Vec<SubAgentConfig>) -> Self
  pub fn get(&self, name: &str) -> Option<Arc<Agent>>
  pub fn contains(&self, name: &str) -> bool
  pub fn values_cloned(&self) -> Vec<Arc<Agent>>
  pub fn names(&self) -> Vec<String>
  pub fn len(&self) -> usize
  pub fn is_empty(&self) -> bool
  pub fn replace_all(&self, configs: Vec<SubAgentConfig>)
  pub fn reload_from_dir(&self, agents_dir: &Path) -> usize
```

**Impl** `impl Default for AgentRegistry`:
```rust
  fn default() -> Self
```

```rust
fn dummy(name: &str) -> SubAgentConfig
```

```rust
fn from_vec_indexes_by_name()
```

```rust
fn replace_all_overwrites()
```

#### `agents/ask_router.rs`

**结构体** `AskRouter`:
```rust
pub struct AskRouter {
  pending: Arc<DashMap<String, oneshot::Sender<ChannelMessage>>>,
}
```

**Impl** `impl AskRouter`:
```rust
  pub fn new() -> Self
  pub async fn wait_for_reply(
  pub fn fulfill(&self, session_id: &str, reply: ChannelMessage) -> bool
```

```rust
fn msg(content: &str) -> ChannelMessage
```

```rust
async fn fulfill_resolves_future()
```

```rust
async fn fulfill_missing_returns_false()
```

```rust
async fn timeout_clears_pending_slot()
```

#### `agents/attachment.rs`

**枚举** `AttachmentKind`:
```rust
priv enum AttachmentKind {
  SkillListing,
  AgentListing,
  McpInstructions,
  MemoryListing,
  DateInjection,
  AutonomyNotice,
}
```

**结构体** `Delta`:
```rust
priv struct Delta {
  added: Vec<String>,
  removed: Vec<String>,
}
```

**结构体** `AnnouncedState`:
```rust
priv struct AnnouncedState {
  skills: HashSet<String>,
  agents: HashSet<String>,
  mcp: HashSet<String>,
}
```

**结构体** `AttachmentManager`:
```rust
pub struct AttachmentManager {
  /// 本轮待发送增量
  pending: HashMap<AttachmentKind, Delta>,
  /// 当前注入过的 memory 索引文本（用于跨 turn diff）
  memory_index: Option<String>,
  /// 上次注入的日期 "YYYY-MM-DD"（用于区分首次 vs 日期变化）
  last_injected_date: Option<String>,
}
```

**Impl** `impl AttachmentManager`:
```rust
  pub fn new() -> Self
  fn rebuild_from_history(history: &[ChatMessage]) -> AnnouncedState
  pub fn diff_skills(&mut self, skills: &SkillManager, history: &[ChatMessage])
  pub fn diff_agents(&mut self, agents: &[(String, String)], history: &[ChatMessage])
  pub fn diff_mcp(&mut self, servers: &[(String, String)], history: &[ChatMessage])
  pub fn diff_memory(
  pub fn diff_date(&mut self, timezone_offset: i32, history: &[ChatMessage])
  pub fn build_text(&self, skills: &SkillManager) -> Option<String>
  pub fn build_message(&self, skills: &SkillManager) -> Option<ChatMessage>
  pub fn diff_autonomy(&mut self, new_level: &crate::config::agent::PermissionMode)
  pub fn clear_pending(&mut self)
  pub fn pending_keys(&self) -> Vec<&'static str>
  fn render_skills(delta: &Delta, skills: &SkillManager) -> String
  fn render_agents(delta: &Delta) -> String
  fn render_memory(delta: &Delta) -> String
  fn render_mcp(delta: &Delta) -> String
  fn render_date(delta: &Delta) -> String
  fn render_autonomy(delta: &Delta) -> String
```

```rust
fn make_skills(names: &[&str]) -> SkillManager
```

```rust
fn empty_history() -> Vec<ChatMessage>
```

```rust
fn empty_history_sends_all_skills()
```

```rust
fn no_change_no_message()
```

```rust
fn added_skill_appears_in_delta()
```

```rust
fn removed_skill_appears_in_delta()
```

```rust
fn compaction_naturally_resets()
```

```rust
fn agents_diff_works()
```

```rust
fn merged_sections_in_single_message()
```

```rust
fn rebuild_parses_removed_items()
```

```rust
fn date_injection_on_empty_history()
```

```rust
fn date_injection_skips_if_already_in_history()
```

```rust
fn date_reinjects_after_compaction()
```

#### `agents/context_engine.rs`

**结构体** `CompactionResult`:
```rust
pub(crate) struct CompactionResult {
  pub compact_start: usize,
  pub compact_end: usize,
  pub summary: String,
  pub summary_tokens: u64,
  pub removed_tokens: u64,
  #[allow(dead_code)]
  pub compacted_count: usize,
}
```

**结构体** `ContextEngine`:
```rust
pub struct ContextEngine {
  compact_threshold: f64,
  retain_work_units: usize,
  registry: Arc<dyn ProviderRegistry>,
  resources: Arc<ResourceProvider>,
  memory_executor: MemoryToolExecutor,
  max_rounds: usize,
}
```

**Impl** `impl ContextEngine`:
```rust
  pub fn new(
  pub fn compact_threshold(&self) -> f64
  pub fn should_compact(&self, total_tokens: u64, context_window: u64) -> bool
  pub fn compaction_boundary(
  pub(crate) async fn execute_compaction(
  async fn summarize(
  async fn do_summarize(
  async fn collect_summary_stream(&self, mut stream: BoxStream<StreamEvent>) -> anyhow::Result<SummaryResponse>
```

**结构体** `SummaryResponse`:
```rust
priv struct SummaryResponse {
  text: String,
  reasoning_content: Option<String>,
  thinking_signature: Option<String>,
  tool_calls: Vec<ToolCall>,
  #[allow(dead_code)]
  usage: Option<ChatUsage>,
}
```

```rust
fn find_incremental_range(history: &[ChatMessage], boundary: usize) -> (usize, usize, Option<String>)
```

```rust
fn strip_images(msg: &ChatMessage) -> ChatMessage
```

```rust
fn build_memory_prompt(knowledge_dir: &str) -> String
```

```rust
fn build_summarizer_prompt(msg_count: usize, existing_summary: Option<&str>, memory_prompt: &str) -> String
```

```rust
fn audit_summary_quality(to_compact: &[ChatMessage], summary: &str) -> (bool, Vec<String>)
```

```rust
fn extract_file_paths(messages: &[ChatMessage]) -> Vec<String>
```

#### `agents/delegation.rs`

**枚举** `DelegationEvent`:
```rust
pub enum DelegationEvent {
  /// Sub-agent completed successfully.
  Completed {
  task_id: String,
  parent_session_id: String,
  reply_target: String,
  summary: String,
  /// How long the sub-agent ran (in seconds).
  duration_secs: u64,
  },
  /// Sub-agent failed.
  Failed {
  task_id: String,
  parent_session_id: String,
  reply_target: String,
  error: String,
  },
}
```

#### `agents/delegation_coordinator.rs`

**结构体** `DelegationCoordinator`:
```rust
pub struct DelegationCoordinator {
  /// Sub-agent configurations, indexed by name. Same Arc as
  /// `AgentRuntime.agents` so name → Agent lookups stay consistent.
  configs: Arc<super::AgentRegistry>,
  /// Shared SessionManager. Sub-sessions are flat peers of regular
  /// sessions (`meta.parent_session_id` is the link).
  session_manager: Arc<SessionManager>,
  /// Root directory for git worktrees (when isolation = worktree).
  worktrees_root: PathBuf,
  /// Parent AgentRuntime, installed by the daemon after both this
  /// coordinator and the runtime have been built. `delegate` reads
  /// the runtime from here and passes it (with workspace_dir
  /// overlaid when worktree-isolated) to `SessionContext::process_turn`.
  runtime_cell: Arc<std::sync::OnceLock<crate::agents::AgentRuntime>>,
  /// In-flight background delegations (task_id → JoinHandle). Powers
  /// `/agent_list` (read snapshot) and `/agent_kill` (abort by id).
  running: Arc<DashMap<String, JoinHandle<()>>>,
  /// Sender for `DelegationEvent`s emitted when background
  /// delegations complete. Installed by daemon via `set_event_sender`.
  event_tx_cell: Arc<std::sync::OnceLock<mpsc::Sender<DelegationEvent>>>,
}
```

**Impl** `impl DelegationCoordinator`:
```rust
  pub fn new(
  pub fn set_runtime(&self, runtime: crate::agents::AgentRuntime)
  pub fn set_event_sender(&self, tx: mpsc::Sender<DelegationEvent>)
  pub fn event_sender(&self) -> Option<mpsc::Sender<DelegationEvent>>
  pub fn running_snapshot(&self) -> Vec<String>
  pub fn cancel(&self, task_id: &str) -> bool
  fn runtime(&self) -> anyhow::Result<crate::agents::AgentRuntime>
  fn find_agent(&self, name: &str) -> Option<Arc<crate::agents::Agent>>
  fn open_sub_session(
  pub fn delegate_with_parent<'a>(
  pub fn delegate_async(
```

**Impl** `impl crate::agents::AgentDelegator for DelegationCoordinator`:
```rust
  async fn delegate(
  fn list_available(&self) -> Vec<(String, Option<String>)>
```

#### `agents/delegator.rs`

**Trait** `AgentDelegator`:
```rust
pub trait AgentDelegator {
  async fn delegate(
  fn list_available(&self) -> Vec<(String, Option<String>)>
}
```

#### `agents/error.rs`

**枚举** `AgentError`:
```rust
pub enum AgentError {
  /// The loop-breaker aborted the turn due to a repetitive tool pattern.
  #[error("loop breaker triggered: {reason}")]
  LoopBreak { reason: String },
  /// The per-turn tool call hard limit was exceeded.
  #[error("tool call limit reached ({limit})")]
  ToolLimitReached { limit: usize },
  /// The LLM stream produced no data within the configured timeout.
  #[error("stream chunk timeout after {secs}s")]
  StreamTimeout { secs: u64 },
  /// The streaming client disconnected before the turn completed.
  #[error("client disconnected during stream")]
  ClientDisconnected,
  /// A tool execution failure that the model cannot recover from.
  #[error("tool '{name}' failed: {source}")]
  ToolFailed {
  name: String,
  #[source]
  source: anyhow::Error,
  },
  /// An LLM provider error (network, auth, rate-limit, etc.).
  // ... 6 more variants
}
```

#### `agents/llm_stream.rs`

```rust
pub const STREAM_FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(600)
```

```rust
pub const STREAM_CHUNK_INTERVAL_TIMEOUT: Duration = Duration::from_secs(120)
```

```rust
pub async fn read_next(
```

```rust
pub async fn read_to_string(stream: BoxStream<StreamEvent>) -> Result<String>
```

#### `agents/loop_breaker.rs`

**枚举** `LoopBreak`:
```rust
pub enum LoopBreak {
  /// No loop detected, continue.
  None,
  /// Loop detected. Contains the reason.
  Detected(LoopBreakReason),
}
```

**枚举** `LoopBreakReason`:
```rust
pub enum LoopBreakReason {
  /// Same tool + same args repeated too many times.
  ExactRepeat {
  tool: String,
  count: usize,
  threshold: usize,
  },
  /// Two tools ping-ponging.
  PingPong {
  tool_a: String,
  tool_b: String,
  rounds: usize,
  },
  /// Same tool, different args, same results.
  NoProgress {
  tool: String,
  count: usize,
  },
  /// Hard limit exceeded.
  MaxCalls {
  count: usize,
  limit: usize,
  },
}
```

**结构体** `LoopBreakerConfig`:
```rust
pub struct LoopBreakerConfig {
  /// Hard cap on total tool calls. 0 = unlimited (but still checks patterns).
  #[serde(default = "default_max_tool_calls")]
  pub max_tool_calls: usize,
  /// Sliding window size for pattern detection.
  #[serde(default = "default_window_size")]
  pub window_size: usize,
  /// Exact repeat threshold: same tool + same args N times → break.
  #[serde(default = "default_exact_repeat_threshold")]
  pub exact_repeat_threshold: usize,
  /// Ping-pong threshold: alternating rounds before breaking.
  #[serde(default = "default_ping_pong_rounds")]
  pub ping_pong_rounds: usize,
  /// No-progress threshold: same tool + same result hash N consecutive times → break.
  #[serde(default = "default_no_progress_threshold")]
  pub no_progress_threshold: usize,
  /// Tools that are inherently exploratory (e.g. "shell") and need a higher threshold
  /// before NoProgress is triggered. These tools naturally produce similar results
  /// (empty grep, exit code 0) across different args without actually looping.
  #[serde(default = "default_relaxed_tools")]
  pub relaxed_tools: Vec<String>,
}
```

```rust
fn default_max_tool_calls() -> usize { 100 }
```

```rust
fn default_window_size() -> usize { 20 }
```

```rust
fn default_exact_repeat_threshold() -> usize { 3 }
```

```rust
fn default_ping_pong_rounds() -> usize { 6 }
```

```rust
fn default_no_progress_threshold() -> usize { 5 }
```

```rust
fn default_relaxed_tools() -> Vec<String> { vec!["shell".to_string()] }
```

**Impl** `impl Default for LoopBreakerConfig`:
```rust
  fn default() -> Self
```

**结构体** `ToolInvocation`:
```rust
priv struct ToolInvocation {
  tool_name: String,
  args_hash: u64,
  result_hash: u64,
}
```

**结构体** `LoopBreaker`:
```rust
pub struct LoopBreaker {
  config: LoopBreakerConfig,
}
```

**Impl** `impl LoopBreaker`:
```rust
  pub fn new(config: LoopBreakerConfig) -> Self
  pub fn new_counter(&self) -> LoopBreakerCounter
  pub fn new_counter_with_max(&self, max_tool_calls: usize) -> LoopBreakerCounter
```

**结构体** `LoopBreakerCounter`:
```rust
pub struct LoopBreakerCounter {
  config: LoopBreakerConfig,
  /// Total tool calls in this turn.
  total_calls: usize,
  /// Sliding window of recent invocations.
  window: VecDeque<ToolInvocation>,
}
```

**Impl** `impl LoopBreakerCounter`:
```rust
  pub fn new(config: LoopBreakerConfig) -> Self
  pub fn record_and_check(&mut self, tool_name: &str, args: &str, result: &str) -> LoopBreak
  pub fn reset(&mut self)
  pub fn total_calls(&self) -> usize
  pub fn max_tool_calls(&self) -> usize
  fn check_exact_repeat(&self) -> Option<LoopBreakReason>
  fn check_ping_pong(&self) -> Option<LoopBreakReason>
  fn check_no_progress(&self) -> Option<LoopBreakReason>
```

```rust
fn simple_hash(s: &str) -> u64
```

```rust
fn default_breaker() -> LoopBreakerCounter
```

```rust
fn exact_repeat_triggers_after_threshold()
```

```rust
fn exact_repeat_resets_on_different_call()
```

```rust
fn exact_repeat_different_tool_resets()
```

```rust
fn exact_repeat_allows_polling_different_results()
```

```rust
fn exact_repeat_polling_then_stall_triggers()
```

```rust
fn ping_pong_triggers_after_four_rounds()
```

```rust
fn ping_pong_not_triggered_for_same_tool()
```

```rust
fn ping_pong_broken_by_third_tool()
```

```rust
fn no_progress_triggers_when_same_result_different_args()
```

```rust
fn no_progress_broken_by_different_tool()
```

```rust
fn no_progress_relaxed_tool_uses_higher_threshold()
```

```rust
fn no_progress_broken_by_different_result()
```

```rust
fn no_progress_not_triggered_when_results_differ()
```

```rust
fn max_calls_triggers_hard_limit()
```

```rust
fn max_calls_zero_means_unlimited()
```

```rust
fn reset_clears_state()
```

```rust
fn sliding_window_respects_max_size()
```

```rust
fn simple_hash_is_deterministic()
```

```rust
fn simple_hash_differs_for_different_input()
```

```rust
fn simple_hash_handles_empty_string()
```

```rust
fn simple_hash_is_full_content()
```

#### `agents/mcp_manager.rs`

**结构体** `McpManager`:
```rust
pub struct McpManager {
  /// Connected registry. None until `connect()` is called.
  registry: Arc<tokio::sync::RwLock<Option<Arc<crate::mcp::McpRegistry>>>>,
  /// Cached MCP tool wrappers (rebuilt after each connect).
  tools: Arc<tokio::sync::RwLock<Vec<Arc<dyn crate::providers::Tool>>>>,
  /// Number of servers connected on the last successful `connect()`.
  server_count: Arc<std::sync::atomic::AtomicUsize>,
}
```

**Impl** `impl McpManager`:
```rust
  pub fn new() -> Self
  pub async fn connect(&self, configs: &[crate::config::mcp::McpServerConfig]) -> anyhow::Result<()>
  async fn build_wrappers(
  pub async fn tools(&self) -> Vec<Arc<dyn crate::providers::Tool>>
  pub async fn server_count(&self) -> usize
  pub async fn tool_count(&self) -> usize
  pub async fn is_connected(&self) -> bool
  pub async fn server_instructions(&self) -> Vec<(String, String)>
```

**Impl** `impl Default for McpManager`:
```rust
  fn default() -> Self
```

**Impl** `impl From<&crate::config::mcp::McpServerConfig> for crate::mcp::config_types::McpServerConfig`:
```rust
  fn from(cfg: &crate::config::mcp::McpServerConfig) -> Self
```

```rust
async fn new_is_not_connected()
```

```rust
async fn connect_empty_is_connected_but_empty()
```

#### `agents/mod.rs`

#### `agents/orchestrator/mod.rs`

**用途**: Orchestrator 运行时与装配。`run(self)` 把三事件源(入站 / 调度 / 委派)合流为单一 `Stream<OrchestratorEvent>` 并分派;`new()` 从 `OrchestratorParts` 组装出 `OrchestratorCtx` 依赖包。模块组的入口,re-export `OrchestratorCtx` / `ChannelRegistry` / `OrchestratorEvent`。

```rust
const CHANNEL_QUEUE_SIZE: usize = 100
pub(crate) use scheduled::run_scheduled_turn;
```

**结构体** `CronTrigger`(替代旧 `SchedulerEvent::Cron` 的 11 字段;只保留 cron turn 实际消费的 6 个):
```rust
pub struct CronTrigger {
  pub session_key: String,
  pub prompt: String,
  pub target_channel: Option<String>,
  pub target_account: Option<String>,
  pub job_id: String,
  pub model: Option<String>,
}
```

**枚举** `SchedulerEvent`:
```rust
pub enum SchedulerEvent {
  Heartbeat { target_channel: Option<String>, target_account: Option<String> },
  Cron(CronTrigger),
}
```

```rust
pub type ChannelMsgSender = mpsc::Sender<((String, String), ChannelMessage)>
```

**结构体** `Orchestrator`(只独占"消费一次"的 receiver / handle;依赖在 `ctx`):
```rust
pub struct Orchestrator {
  ctx: Arc<OrchestratorCtx>,
  msg_rx: Option<mpsc::Receiver<((String, String), ChannelMessage)>>,
  listener_handles: Vec<JoinHandle<()>>,        // run() 返回前 abort
  delegation_rx: Option<mpsc::Receiver<DelegationEvent>>,
  scheduler_rx: Option<mpsc::Receiver<SchedulerEvent>>,
}
```

**结构体** `OrchestratorParts`(组装根 daemon.rs 的入参,装配缝;保留长字段名):
```rust
pub struct OrchestratorParts {
  pub session_manager: Arc<SessionManager>,
  pub channels: Vec<(String, String, Arc<dyn Channel>)>,
  pub delegator: Option<Arc<DelegationCoordinator>>,
  pub delegation_rx: Option<mpsc::Receiver<DelegationEvent>>,
  pub scheduler_rx: Option<mpsc::Receiver<SchedulerEvent>>,
  pub ask_router: Arc<crate::agents::AskRouter>,
  pub agent_runtime: crate::agents::AgentRuntime,
  pub workspace_dir: std::path::PathBuf,
  pub scheduler: Option<crate::agents::SharedScheduler>,
}
```

**Impl** `impl Orchestrator`:
```rust
  pub fn new(parts: OrchestratorParts) -> (Self, ChannelMsgSender)
  pub fn ctx(&self) -> &Arc<OrchestratorCtx>          // webhook / daemon 取依赖包
  fn spawn_listener(channel_type: String, account_id: String, channel: Arc<dyn Channel>, msg_tx: Arc<ChannelMsgSender>) -> JoinHandle<()>
  pub async fn run(self, shutdown_rx: watch::Receiver<bool>, unfinished: Vec<UnfinishedSubAgent>) -> anyhow::Result<()>
  async fn handle_scheduler_event(&self, event: SchedulerEvent)
```

```rust
pub(super) fn history_has_incomplete_turn(history: &[ChatMessage]) -> bool
pub(crate) fn is_silent_ok(response: &str, prefix: &str) -> bool
```

#### `agents/orchestrator/ctx.rs`

**用途**: `OrchestratorCtx` —— 共享依赖包(全 `Arc`,随便 clone),与"运行时"`Orchestrator` 分离;长寿命 spawn 任务持 `Arc<OrchestratorCtx>` 而非 `Arc<Orchestrator>`。`ChannelRegistry` 封装裸 `Arc<DashMap>`,统一查表。

**结构体** `ChannelRegistry`:
```rust
pub struct ChannelRegistry { inner: Arc<DashMap<(String, String), Arc<dyn Channel>>> }
impl ChannelRegistry {
  pub fn new() -> Self
  pub fn insert(&self, account: (String, String), channel: Arc<dyn Channel>)
  pub fn get(&self, account: &(String, String)) -> Option<Arc<dyn Channel>>
  pub fn get_by_key(&self, key: &SessionKey) -> Option<Arc<dyn Channel>>
  pub fn len(&self) -> usize
  pub fn is_empty(&self) -> bool
}
```

**结构体** `OrchestratorCtx`:
```rust
pub struct OrchestratorCtx {
  pub channels: ChannelRegistry,
  pub sessions: Arc<SessionManager>,
  pub ask: Arc<AskRouter>,
  pub runtime: AgentRuntime,
  pub delegator: Option<Arc<DelegationCoordinator>>,
  pub scheduler: Option<SharedScheduler>,
}
impl OrchestratorCtx {
  pub fn session_context_for(&self, sk: &str) -> Arc<SessionContext>
  pub fn channel(&self, account: &(String, String)) -> Option<Arc<dyn Channel>>
}
```

#### `agents/orchestrator/key.rs`

**用途**: 会话键值类型,取代散落的 `splitn(3, ':')` / `format!("{}:{}:{}")`。`splitn` 仅存在于 `SessionKey::parse`。

```rust
pub struct SessionKey { pub channel: String, pub account: String, pub sender: String }
impl SessionKey {
  pub fn new(channel: impl Into<String>, account: impl Into<String>, sender: impl Into<String>) -> Self
  pub fn parse(s: &str) -> Option<Self>
  pub fn account_key(&self) -> (String, String)
}
impl fmt::Display for SessionKey   // "ct:ac:sender"

/// 子代理键是 2 段,单独建模
pub struct SubAgentKey { pub agent: String, pub sub_session: String }
impl SubAgentKey { pub fn new(agent: impl Into<String>, sub_session: impl Into<String>) -> Self }
impl fmt::Display for SubAgentKey  // "agent:sub_session"
```

#### `agents/orchestrator/event.rs`

**枚举** `OrchestratorEvent`(`run()` 的统一事件类型):
```rust
pub enum OrchestratorEvent {
  Inbound { channel_type: String, account_id: String, message: ChannelMessage },
  Scheduled(super::SchedulerEvent),
  Delegation(DelegationEvent),
  AskReply { session_id: String, reply: ChannelMessage },
  Shutdown,
}
```

#### `agents/orchestrator/turn.rs`

**用途**: per-turn 参数解析的单一归属——把 `session_override` 叠加到运行时默认,产出系统提示 / thinking / model。消除 recovery 等处的重复组装。

```rust
pub struct ResolvedTurn {
  pub system_prompt: String,
  pub thinking: Option<ThinkingConfig>,
  pub model: Option<String>,
  pub permission_mode: PermissionMode,
  pub run_mode: RunMode,
}
impl ResolvedTurn {
  pub fn resolve(session: &Session, runtime: &AgentRuntime) -> Self
  pub fn turn_context(&self) -> TurnContext<'_>
}
```

#### `agents/orchestrator/inbound.rs`

**用途**: 入站用户消息的责任链。每个拦截器返回 `Flow::Stop`(已消费)或 `Flow::Next(msg)`(透传,可改写);终端 `DispatchTurn` 经 `dispatch_turn` spawn `process_turn`。链顺序:ask-reply → callback → crash-recovery → slash-command → dispatch-turn。

```rust
enum Flow { Stop, Next(ChannelMessage) }

trait Interceptor: Send + Sync {
  fn name(&self) -> &'static str;
  async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) -> Flow;
}
// 拦截器单元结构:AskReply / Callback / CrashRecovery / SlashCommand / DispatchTurn
fn chain() -> [&'static dyn Interceptor; 5]

pub(super) async fn dispatch(ctx: &OrchestratorCtx, account: (String, String), msg: ChannelMessage)
pub(super) async fn dispatch_turn(ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage)
pub(super) fn retry_abort_prompt(content: impl Into<String>, sk: &str, reply_target: impl Into<String>, thread_ts: Option<String>) -> (SendTarget, MessagePayload)
```

#### `agents/orchestrator/delegation.rs`

**用途**: 子代理完成 / 失败的"系统唤醒"——合成系统通知消息,经 `inbound::dispatch_turn` 直接驱动父会话一次 turn,**不**伪造 inbound 跑完整拦截链。

```rust
pub(super) async fn wake(ctx: &OrchestratorCtx, event: DelegationEvent)
```

#### `agents/orchestrator/recovery.rs`

**用途**: 启动恢复——崩溃 / SIGKILL 中断的 turn 重放。普通会话与子代理统一为 `spawn_recovery` + `CompletionSink`(投递去向的唯一差异)。

```rust
enum CompletionSink {
  Channel { key: String, backend: Arc<dyn SessionBackend>, channels: ChannelRegistry },
  Delegate { task_id: String, parent_session_id: String, reply_target: String, delegator: Option<Arc<DelegationCoordinator>> },
}
fn spawn_recovery(session_ctx: Arc<SessionContext>, runtime: AgentRuntime, backend: Arc<dyn SessionBackend>, label: &'static str, id: String, sink: CompletionSink)
pub(super) fn run_startup(sessions: &Arc<SessionManager>, runtime: &AgentRuntime, channels: &ChannelRegistry, unfinished: &[UnfinishedSubAgent], delegator: &Option<Arc<DelegationCoordinator>>)
```

#### `agents/orchestrator/scheduled.rs`

**用途**: 调度 turn 派发。心跳 / Cron 作为独立 spawn 任务,经 `run_scheduled_turn` 驱动一次合成 turn(与用户入站 turn 同一 `process_turn` 路径),输出投递到目标 channel。`run_scheduled_turn` 亦被 webhook(`scheduler.rs`)复用,消除重复。

```rust
pub(crate) async fn run_scheduled_turn(orch: &OrchestratorCtx, session_key: &str, prompt: &str, model_override: Option<String>) -> anyhow::Result<String>
pub(crate) async fn run_heartbeat_task(orch: Arc<OrchestratorCtx>, target_channel: Option<String>, target_account: Option<String>, prompt: String, due: Vec<HeartbeatTask>, state: HeartbeatState, state_path: PathBuf)
pub(crate) async fn run_cron_task(orch: Arc<OrchestratorCtx>, trigger: super::CronTrigger)
async fn send_to_target_internal(orch: &OrchestratorCtx, target_channel: Option<String>, target_account: Option<String>, content: &str)
```

> `agents/orchestrator/test_support.rs` 为 `#[cfg(test)]` 测试夹具:可注入 mock 的 `OrchestratorCtx`(内存 SessionManager + no-op ProviderRegistry/AgentRuntime + 记录式 `MockChannel`),供 inbound 拦截器单测使用。

#### `agents/prompt.rs`

**结构体** `SystemPromptConfig`:
```rust
pub struct SystemPromptConfig {
  /// Workspace directory (for AGENT.md lookup at the caller).
  /// Not read by the builder itself — kept here as runtime info that
  /// `build_runtime` exposes to the LLM as part of the working environment.
  pub workspace_dir: String,
  /// Knowledge directory (contains memory/*.md files).
  pub knowledge_dir: String,
  /// Permission mode — controls tool access level.
  pub permission_mode: PermissionMode,
  /// Run mode — controls execution context rules.
  pub run_mode: RunMode,
  /// Optional identity header prepended before all sections.
  /// Used by sub-agents to inject their name and role without
  /// exposing SECTION_* constants outside this module.
  pub identity_header: Option<String>,
  /// Total character limit for the system prompt (0 = unlimited).
  pub max_chars: usize,
  /// Whether the provider supports native tool calling.
  pub native_tools: bool,
}
```

**Impl** `impl Default for SystemPromptConfig`:
```rust
  fn default() -> Self
```

**结构体** `SystemPromptBuilder`:
```rust
pub struct SystemPromptBuilder {
  config: SystemPromptConfig,
}
```

**Impl** `impl SystemPromptBuilder`:
```rust
  pub fn new(config: SystemPromptConfig) -> Self
  pub fn build(&self, skills: &SkillManager) -> String
  pub fn build_with_profile(
  fn build_action_instruction(&self) -> String
  fn build_safety(&self) -> String
  fn build_run_mode_rules(&self) -> String
  fn behavioral_rules() -> Vec<String>
  fn build_runtime(&self) -> String
  fn truncate(&self, mut text: String) -> String
```

```rust
const SECTION_ANTI_NARRATION: &str = r#"## CRITICAL: No Tool Narration
```

```rust
const SECTION_TOOL_HONESTY: &str = r#"## CRITICAL: Tool Honesty
```

```rust
const SECTION_SAFETY_FULL: &str = "## Safety\n\nYou have full autonomy. Execute actions directly without asking for confirmation unless the action is potentially destructive or irreversible."
```

```rust
const SECTION_INTERACTIVE_RULES: &str = r#"## Running Mode: Interactive Session
```

```rust
const SECTION_AUTONOMOUS_RULES: &str = r#"## Running Mode: Autonomous Background
```

```rust
const SECTION_TASK_PERSISTENCE: &str = r#"## Task Persistence
```

```rust
const SECTION_NO_OVER_ENGINEERING: &str = r#"## Don't Over-Engineer
```

```rust
const SECTION_MANDATORY_TOOL_USE: &str = r#"## Mandatory Tool Use
```

```rust
const SECTION_TOOL_PRIORITY: &str = r#"## Tool Priority
```

```rust
const SECTION_MEMORY_GUIDE: &str = r#"## Memory Writing Guide
```

```rust
const SECTION_READ_BEFORE_EDIT: &str = "## Read Before Edit\n\nDo not propose changes to code you haven't read. If asked about or modifying a file, read it first."
```

```rust
const SECTION_SYSTEM_REMINDERS: &str = r#"## System Reminders
```

```rust
fn build(config: SystemPromptConfig) -> String
```

```rust
fn test_anti_narration_present()
```

```rust
fn test_truncation()
```

```rust
fn test_readonly_safety()
```

```rust
fn test_action_instruction_readonly()
```

```rust
fn test_interactive_rules_present()
```

```rust
fn test_background_rules_present()
```

```rust
fn test_behavioral_rules_present()
```

```rust
fn test_no_channel_caps()
```

```rust
fn test_runtime_no_model_name()
```

```rust
fn test_identity_header_prepended()
```

```rust
fn test_no_tool_list_in_prompt()
```

```rust
fn test_profile_section_appended()
```

```rust
fn test_no_profile_section_when_empty()
```

#### `agents/recovery.rs`

**结构体** `UnfinishedSubAgent`:
```rust
pub struct UnfinishedSubAgent {
  pub agent_name: String,
  pub task_id: String,
  pub task_preview: String,
  pub parent_session_id: String,
  pub sub_session_id: String,
  /// The parent session key (e.g. "telegram:default:12345") used to look up
  /// the main agent's session and emit a `DelegationEvent` when recovery
  /// completes. Resolved from the parent session's `owner` field.
  pub session_key: String,
  /// The reply_target stored on the parent session's last_message.
  pub reply_target: String,
}
```

```rust
pub fn scan_unfinished_subagents(session_manager: &SessionManager) -> Vec<UnfinishedSubAgent>
```

#### `agents/resource_provider.rs`

**结构体** `ResourceProvider`:
```rust
pub struct ResourceProvider {
  pub(crate) skills: Arc<RwLock<SkillManager>>,
  pub(crate) sub_agents: Arc<AgentRegistry>,
  pub(crate) mcp_instructions: Vec<(String, String)>,
  pub(crate) skills_dir: PathBuf,
  pub(crate) agents_dir: PathBuf,
  /// Absolute path to the memory/ directory (for diff_memory scanning).
  pub(crate) knowledge_dir: String,
  /// Timezone offset in hours (for date injection).
  pub(crate) timezone_offset: i32,
}
```

**Impl** `impl ResourceProvider`:
```rust
  pub fn new(
  pub fn timezone_offset(&self) -> i32
```

#### `agents/runtime.rs`

**结构体** `RuntimeDefaults`:
```rust
pub struct RuntimeDefaults {
  /// Default permission mode (overridden per turn by SessionOverride).
  pub permission_mode: PermissionMode,
  /// Base prompt config — workspace/knowledge dirs (as strings, used
  /// by SystemPromptBuilder + file-tool path resolution), identity
  /// header, native_tools, …
  pub prompt: SystemPromptConfig,
}
```

**Impl** `impl Default for RuntimeDefaults`:
```rust
  fn default() -> Self
```

**结构体** `AgentRuntime`:
```rust
pub struct AgentRuntime {
  /// LLM provider registry — used to resolve `model_id` to a ChatProvider.
  pub providers: Arc<dyn ProviderRegistry>,
  /// All registered tools (built-in + MCP wrappers + skill tools).
  /// `Agent.run` filters this through `AgentConfig.tools` per turn.
  pub tools: Arc<ToolRegistry>,
  /// Skill metadata for system-prompt injection and the `skill_view` tool.
  pub skills: Arc<RwLock<SkillManager>>,
  /// Sub-agents available for `agent_delegate`. Shared `Arc` so the
  /// AgentRegistry held by SessionManager and AgentRuntime is the
  /// same live table.
  pub agents: Arc<AgentRegistry>,
  /// Compaction policy + summarizer. Shared singleton.
  pub context_engine: Arc<ContextEngine>,
  /// Tool executor (timeout + dispatch). Shared singleton.
  pub tool_executor: Arc<ToolExecutor>,
  /// Loop-breaker policy. Hands out per-turn `LoopBreakerCounter`s
  /// via `new_counter()`.
  pub loop_breaker: Arc<LoopBreaker>,
  /// Defaults — exactly `{ permission_mode, prompt }` per the target
  /// shape; see RFC v2 §三.A.
  pub defaults: RuntimeDefaults,
  /// MCP server manager (Option because MCP servers are opt-in).
  /// Read by the `/mcp` slash command to report connection state.
  pub mcp_manager: Option<Arc<McpManager>>,
  /// Search-provider rate-limit tracker shared with `WebSearchTool`
  /// (the tool writes timestamps on rate-limit; `/status` reads them
  /// to render ⏱️ markers next to cooled-down providers).
  pub search_cooldown: Option<Arc<SearchProviderCooldown>>,
  /// Content-addressed cache of media-fingerprint → text description,
  /// shared with the modality adapter (`agents/modality_adapter.rs`).
  /// Survives across turns/sessions so historical media can reuse a
  /// description instead of degrading to a placeholder. `new()` installs
  /// an in-memory `LruDescriptionCache`; override via
  /// `with_description_cache`.
  pub description_cache: Arc<dyn DescriptionCache>,
}
```

**Impl** `impl AgentRuntime`:
```rust
  pub fn new(
  pub fn with_defaults(mut self, defaults: RuntimeDefaults) -> Self
  pub fn with_description_cache(mut self, cache: Arc<dyn DescriptionCache>) -> Self
  pub fn with_mcp_manager(mut self, mcp: Arc<McpManager>) -> Self
  pub fn with_search_cooldown(mut self, cooldown: Arc<SearchProviderCooldown>) -> Self
  pub fn build_system_prompt(&self, prompt_config: &SystemPromptConfig) -> String
```

#### `agents/modality_adapter.rs`

Modality adaptation layer — translates non-text modalities (image, and in
later phases audio/video) to text when the primary chat model does not
support them natively. Operates on cloned messages only (never mutates
persistent history) and is **registry-free**: the auxiliary `ChatProvider`
is passed in by the caller. Translation results are cached by content
fingerprint (sha256 of URL / base64 payload) so historical media can reuse a
description rather than degrading to a placeholder. See
`docs/multimodal-auxiliary-translation.md` §3.3, §4.3, §4.7.

**结构体** `ModalitySpec` — static description of how to adapt one modality:
```rust
pub struct ModalitySpec {
  pub modality: Modality,        // input modality this spec adapts
  pub prompt: &'static str,      // prompt sent to the auxiliary model
  pub label: &'static str,       // label for the injected text, e.g. "图片"
  pub placeholder: &'static str, // text used when no description is available
}
pub const IMAGE_SPEC: ModalitySpec; // image, label "图片", placeholder "[image]"
pub const AUDIO_SPEC: ModalitySpec; // audio transcription, label "音频", placeholder "[audio]"
```

**自由函数**:
```rust
pub fn part_matches(part: &ContentPart, modality: &Modality) -> bool // Image: ImageUrl/ImageB64; Audio: AudioB64
pub fn fingerprint(part: &ContentPart) -> Option<String> // → ContentPart::content_fingerprint (decoded-bytes sha256 == blob hash)
pub async fn translate_part(
  provider: &dyn ChatProvider,
  model_id: &str,
  part: &ContentPart,
  spec: &ModalitySpec,
  cache: &dyn DescriptionCache,
  session_id: &str,
) -> anyhow::Result<String>
pub fn adapt_history_media(
  messages: &mut [ChatMessage],
  spec: &ModalitySpec,
  cache: &dyn DescriptionCache,
  session_id: &str,
  skip_idx: Option<usize>,
) // historical media → cached description or placeholder; never calls aux model
pub async fn adapt_last_turn_media(
  msg: &mut ChatMessage,
  spec: &ModalitySpec,
  aux: Option<(&Arc<dyn ChatProvider>, &str)>,
  cache: &dyn DescriptionCache,
  session_id: &str,
) // current-turn media → one combined (numbered) description; degrades gracefully
```

`translate_part` builds a single self-contained user message (media part +
`spec.prompt`), issues a streaming `provider.chat(req)` (`stream = true`),
aggregates via `ChatResponse::from_stream(stream).await?`, takes `.text`, and
caches it under the fingerprint. `adapt_last_turn_media` translates a
message's media parts in parallel via `futures_util::future::join_all`.

**trait** `DescriptionCache` — `(session_id, fingerprint)` → description cache:
```rust
pub trait DescriptionCache: Send + Sync {
  fn get(&self, session_id: &str, key: &str) -> Option<String>;
  fn put(&self, session_id: &str, key: String, value: String);
}
```

**结构体** `LruDescriptionCache` — `Mutex<lru::LruCache<String, String>>`-backed
in-memory impl, keyed by a `(session_id, key)` composite (`new(capacity)` /
`Default` = 512 entries). Used by CLI one-shot commands and tests.

**结构体** `PersistentDescriptionCache` — two-tier impl: bounded LRU hot tier +
**per-session** content-addressed on-disk cold tier at
`{sessions_root}/{session_id}/descriptions/{key}.txt` (a sibling of that
session's `blobs/`; write-through + read-through, atomic temp+rename).
`open(sessions_root, capacity)`. The daemon installs it over `workspace/sessions`
so image descriptions survive restarts and LRU eviction (a non-vision model
recovers historical-image descriptions without re-invoking the auxiliary model —
parallel to how blobs persist image bytes) and are reclaimed with the session on
delete. Keyed by `ContentPart::content_fingerprint` (decoded-bytes sha256 == the
blob hash), so descriptions share the blob mark-and-sweep: `rotate_history` and
`truncate_messages` sweep `descriptions/*.txt` against the content fingerprints of
all live media (a superset of the live blob hashes — inline `ImageB64`, `ImageRef`,
and `ImageUrl`), reclaiming descriptions of compaction-dropped images in-session.

#### `agents/session_context.rs`

**结构体** `SessionContext`:
```rust
pub struct SessionContext {
  /// Mutable session state. Wrapped in Mutex so the turn lock and the
  /// Session itself share the same critical section.
  pub session: Arc<Mutex<Session>>,
  /// Agent bound to this session at creation time. Built from
  /// `Session.agent_name` via `SessionManager.build_agent_for_session`.
  pub agent: Arc<Agent>,
  /// Attachments awaiting injection on the next user turn. Mutex
  /// because `AttachmentManager.diff_*` mutate pending state; the
  /// outer Arc lives on the SessionContext itself, so a plain
  /// `Mutex<AttachmentManager>` here is sufficient.
  pub attachments: Mutex<AttachmentManager>,
  /// User message saved when the previous turn ended with an empty LLM
  /// response or interrupted streaming. Cleared once retried.
  pub pending_retry: Arc<Mutex<Option<String>>>,
  /// Serializes `process_turn` per session. Distinct from `session`'s
  /// Mutex because some readers want to peek at session state without
  /// blocking on an in-flight turn.
  pub turn_lock: Arc<Mutex<()>>,
  /// Loaded UserProfile snapshot taken at SessionContext creation.
  /// Immutable for the lifetime of the context — per RFC §三.A reload
  /// semantics drop the SessionContext and let `SessionManager`
  /// rematerialize it from a fresh profile read.
  pub user_profile: Arc<UserProfile>,
}
```

**Impl** `impl SessionContext`:
```rust
  pub fn new(session: Session, agent: Arc<Agent>) -> Self
  pub fn with_profile(session: Session, agent: Arc<Agent>, profile: Arc<UserProfile>) -> Self
  pub async fn session_snapshot(&self) -> Session
  pub async fn process_turn(  // 从 last_message.image_urls/image_base64 构建媒体
                             // parts，非空时调 add_user_with_media（否则 add_user）；
                             // persist_hook 随后持久化携带图片的消息（externalize 生效）
```

#### `agents/tokens.rs`

```rust
pub fn estimate_tokens(text: &str) -> u64
```

```rust
pub fn estimate_message_tokens(msg: &ChatMessage) -> u64
```

**结构体** `TokenTracker`:
```rust
pub struct TokenTracker {
  last_input_tokens: u64,
  last_cached_tokens: u64,
  last_output_tokens: u64,
  pending_estimated_tokens: u64,
}
```

**Impl** `impl TokenTracker`:
```rust
  pub fn new() -> Self
  pub fn update_from_usage(&mut self, input_tokens: u64, output_tokens: u64, cached_tokens: u64)
  pub fn record_pending(&mut self, tokens: u64)
  pub fn seed_from_history(&mut self, system_prompt: &str, history: &[ChatMessage])
  pub fn total_tokens(&self) -> u64
  pub fn is_fresh(&self) -> bool
  pub fn last_input(&self) -> u64 { self.last_input_tokens }
  pub fn last_cached(&self) -> u64 { self.last_cached_tokens }
  pub fn last_output(&self) -> u64 { self.last_output_tokens }
  pub fn adjust_for_compaction(&mut self, removed_tokens: u64, added_tokens: u64)
```

```rust
pub fn is_write_tool(name: &str) -> bool
```

#### `agents/tool_executor.rs`

**结构体** `ToolExecutor`:
```rust
pub struct ToolExecutor {
  pub timeout_secs: u64,
}
```

**Impl** `impl ToolExecutor`:
```rust
  pub fn new(timeout_secs: u64) -> Self
  pub(crate) async fn execute(
  async fn run_tool(
```

```rust
pub(crate) fn parse_tool_args(arguments: &str) -> serde_json::Value
```

**结构体** `MemoryToolExecutor`:
```rust
pub struct MemoryToolExecutor {
  tools: Arc<ToolRegistry>,
}
```

**Impl** `impl MemoryToolExecutor`:
```rust
  pub(crate) fn new(tools: Arc<ToolRegistry>) -> Self
  pub(crate) async fn execute(&self, call: &ToolCall, session: &Session) -> anyhow::Result<ToolResult>
```

#### `agents/tool_registry.rs`

**结构体** `ToolRegistry`:
```rust
pub struct ToolRegistry {
  tools: HashMap<String, Arc<dyn Tool>>,
}
```

**Impl** `impl Default for ToolRegistry`:
```rust
  fn default() -> Self
```

**Impl** `impl ToolRegistry`:
```rust
  pub fn new() -> Self
  pub fn register(&mut self, tool: Arc<dyn Tool>)
  pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>>
  pub fn all_tools(&self) -> Vec<Arc<dyn Tool>>
  pub fn tool_count(&self) -> usize
  pub fn tool_names_sorted(&self) -> Vec<String>
```

#### `agents/turn.rs`

**结构体** `TurnContext`:
```rust
pub struct TurnContext {
  /// Fully assembled system prompt: builtin sections + AGENT.md body
  /// + user profile + runtime info + skill instructions.
  pub system_prompt: &'a str,
  /// LLM model to call. `None` → Agent.run falls back to
  /// `ProviderRegistry.get_chat_provider(Capability::Chat)` (the first
  /// chat-capable provider per registration order).
  pub model_id: Option<&'a str>,
  /// Thinking / reasoning config override for this turn.
  pub thinking: Option<&'a ThinkingConfig>,
  /// Permission mode resolved as `SessionOverride > [agent].permission_mode > default`.
  pub permission_mode: PermissionMode,
  /// Run mode resolved as `SessionOverride > Interactive`.
  pub run_mode: RunMode,
}
```

**结构体** `TurnResult`:
```rust
pub struct TurnResult {
  pub text: String,
  pub stop_reason: StopReason,
  /// When the LLM returns empty output and the turn ends abnormally, the
  /// user message is saved here so the user can retry without retyping.
  /// `SessionContext` stores this into `pending_retry` for the next turn.
  pub pending_retry: Option<String>,
}
```

#### `agents/turn_event.rs`

**枚举** `TurnEvent`:
```rust
pub enum TurnEvent {
  /// LLM 文本片段
  #[serde(rename = "chunk")]
  Chunk { delta: String },
  /// 思考过程片段（thinking model）
  #[serde(rename = "thinking")]
  Thinking { delta: String },
  /// Agent 正在调用工具
  #[serde(rename = "tool_call")]
  ToolCall {
  id: String,
  name: String,
  args: serde_json::Value,
  },
  /// 工具返回结果
  #[serde(rename = "tool_result")]
  ToolResult { id: String, name: String, output: String },
  /// Turn 被用户取消
  #[serde(rename = "cancelled")]
  Cancelled { partial: String },
  /// Turn 完成（最终事件，包含完整文本）
  // ... 3 more variants
}
```

```rust
fn serialize_chunk()
```

```rust
fn serialize_tool_call()
```

```rust
fn serialize_cancelled()
```

#### `agents/user_messages.rs`

**用途**: Orchestrator 发出的面向用户的文案字符串（重试 / 放弃提示、超时通知），集中于此以便与编排逻辑分离，并作为未来 i18n 的单一接缝。

```rust
pub const MSG_NO_PENDING_RETRY: &str = "没有待重试的消息，请重新发送。"
```

```rust
pub const MSG_ABORT_ACK: &str = "已取消"
```

```rust
pub const MSG_TURN_FAILED: &str = "⚠️ 处理超时，未收到模型回复。"
```

```rust
pub const MSG_INCOMPLETE_TURN: &str = "⚠️ 检测到上次请求未处理完成（可能是服务重启）。\n\n请选择重试或放弃。"
```

```rust
pub const BTN_RETRY: &str = "🔄 重试"
```

```rust
pub const BTN_ABORT: &str = "✖ 放弃"
```

#### `agents/user_profile.rs`

**结构体** `UserResolver`:
```rust
pub struct UserResolver {
  overrides: RwLock<std::collections::HashMap<String, String>>,
}
```

**Impl** `impl UserResolver`:
```rust
  pub fn new() -> Self
  pub fn resolve(&self, routing_key: &str) -> String
  pub fn set(&self, routing_key: impl Into<String>, user_id: impl Into<String>)
  pub fn routing_keys_for(&self, user_id: &str) -> Vec<String>
```

**结构体** `UserProfile`:
```rust
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
```

**Impl** `impl UserProfile`:
```rust
  pub fn load(workspace_dir: &Path, user_id: &str) -> Self
  pub fn path(workspace_dir: &Path, user_id: &str) -> PathBuf
  pub fn save(&self, workspace_dir: &Path, user_id: &str) -> std::io::Result<()>
  pub fn is_empty(&self) -> bool
  pub fn to_prompt_section(&self) -> Option<String>
```

```rust
fn resolver_defaults_to_identity()
```

```rust
fn resolver_override_collapses_keys()
```

```rust
fn empty_profile_renders_none()
```

```rust
fn profile_renders_known_fields()
```

```rust
fn profile_load_returns_empty_for_missing()
```

```rust
fn profile_save_then_load_roundtrip()
```

### `agents/commands/`

**子模块说明**: 斜杠命令处理：/help /status /model /new /compact /config /reload /export /mcp 等

#### `agents/commands/config.rs`

```rust
pub fn cmd_config(args: &str, ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_settings(ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_autonomy(args: &str, ctx: CommandContext<'_>) -> String
```

#### `agents/commands/info.rs`

```rust
pub fn cmd_help() -> String
```

```rust
pub async fn cmd_status(ctx: CommandContext<'_>) -> String
```

```rust
pub fn cmd_tools(ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_context(ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_btw(args: &str, ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_export(ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_mcp(ctx: CommandContext<'_>) -> String
```

```rust
pub fn cmd_skill(ctx: CommandContext<'_>) -> String
```

#### `agents/commands/mod.rs`

**结构体** `CommandContext`:
```rust
pub struct CommandContext {
  pub user_id: &'a str,
  pub registry: &'a Arc<dyn crate::providers::ProviderRegistry>,
  pub session_manager: &'a SessionManager,
  pub runtime: &'a AgentRuntime,
  /// Active SessionContext for this user (the canonical Arc<Mutex<Session>>
  /// the inbound Agent dispatch is using). Commands acquire
  /// `session_ctx.session.lock().await` for read/write access to live
  /// state — same Mutex used by Agent.run, so reads see the latest
  /// turn's state without stale-cache surprises.
  pub session_ctx: Option<&'a Arc<crate::agents::SessionContext>>,
}
```

```rust
pub fn parse_command(content: &str) -> Option<(&str, &str)>
```

```rust
pub fn command_catalog() -> Vec<(&'static str, &'static str)>
```

```rust
pub fn is_known_command(cmd: &str) -> bool
```

```rust
pub async fn dispatch(cmd: &str, args: &str, ctx: CommandContext<'_>) -> Option<String>
```

```rust
pub(super) async fn apply_and_persist_override(ov: SessionOverride, ctx: &CommandContext<'_>)
```

```rust
pub(super) async fn get_history(ctx: &CommandContext<'_>) -> Option<Vec<crate::providers::ChatMessage>>
```

#### `agents/commands/model.rs`

```rust
pub async fn cmd_model(args: &str, ctx: CommandContext<'_>) -> String
```

```rust
pub fn cmd_models(ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_think(args: &str, ctx: CommandContext<'_>) -> String
```

#### `agents/commands/reload.rs`

```rust
pub fn cmd_stop() -> String
```

```rust
pub async fn cmd_reload(ctx: CommandContext<'_>) -> String
```

#### `agents/commands/session.rs`

```rust
pub async fn cmd_new(args: &str, ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_compact(ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_history(ctx: CommandContext<'_>) -> String
```

```rust
pub fn cmd_sessions(ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_switch(args: &str, ctx: CommandContext<'_>) -> String
```

```rust
pub fn cmd_rename(args: &str, ctx: CommandContext<'_>) -> String
```

```rust
pub async fn cmd_delete(args: &str, ctx: CommandContext<'_>) -> String
```

### `agents/scheduling/`

**子模块说明**: 调度系统：Cron 定时任务、心跳任务、Webhook、WorkUnit 调度

#### `agents/scheduling/cron_loader.rs`

```rust
pub fn parse_cron_file(path: &Path) -> anyhow::Result<CronJob>
```

```rust
pub fn load_cron_jobs(cron_dir: &Path) -> Vec<CronJob>
```

```rust
fn parse_cron_file_valid()
```

```rust
fn parse_cron_file_default_target()
```

```rust
fn parse_cron_file_missing_schedule()
```

```rust
fn parse_cron_file_empty_body()
```

```rust
fn load_cron_jobs_from_dir()
```

```rust
fn load_cron_jobs_missing_dir()
```

#### `agents/scheduling/cron_types.rs`

**结构体** `DeliveryConfig`:
```rust
pub struct DeliveryConfig {
  /// Target channel name (e.g. "telegram", "discord").
  pub channel: String,
  /// Target account ID (for multi-instance channels).
  #[serde(default)]
  pub account_id: Option<String>,
  /// Target user/group ID (channel-specific format).
  #[serde(default)]
  pub to: Option<String>,
  /// Thread/topic ID for threaded channels (Discord, Telegram topics).
  #[serde(default)]
  pub thread_id: Option<String>,
}
```

**枚举** `RetryableError`:
```rust
pub enum RetryableError {
  RateLimit,
  Timeout,
  ServerError,
  Network,
  Overloaded,
}
```

**结构体** `RetryConfig`:
```rust
pub struct RetryConfig {
  /// Max retries before marking as permanently failed (default: 3).
  #[serde(default = "default_max_attempts")]
  pub max_attempts: u32,
  /// Backoff delays in milliseconds for each retry (default: [30000, 60000, 300000]).
  #[serde(default = "default_backoff_ms")]
  pub backoff_ms: Vec<u64>,
}
```

```rust
fn default_max_attempts() -> u32 { 3 }
```

```rust
fn default_backoff_ms() -> Vec<u64> { vec![30_000, 60_000, 300_000] }
```

**Impl** `impl Default for RetryConfig`:
```rust
  fn default() -> Self
```

**结构体** `FailureAlertConfig`:
```rust
pub struct FailureAlertConfig {
  /// Alert after N consecutive failures (default: 3).
  #[serde(default = "default_after")]
  pub after: u32,
  /// Minimum seconds between repeated alerts (default: 3600).
  #[serde(default = "default_cooldown_secs")]
  pub cooldown_secs: u64,
  /// Whether to include skipped runs in the failure count (default: false).
  #[serde(default)]
  pub include_skipped: bool,
}
```

```rust
fn default_after() -> u32 { 3 }
```

```rust
fn default_cooldown_secs() -> u64 { 3600 }
```

**Impl** `impl Default for FailureAlertConfig`:
```rust
  fn default() -> Self
```

**枚举** `RunStatus`:
```rust
pub enum RunStatus {
  #[default]
  Ok,
  Error,
  Timeout,
  Skipped,
}
```

**Impl** `impl RunStatus`:
```rust
  pub fn as_str(&self) -> &'static str
```

**结构体** `RunRecord`:
```rust
pub struct RunRecord {
  /// ISO 8601 timestamp of execution.
  pub run_at: String,
  /// Execution status.
  pub status: RunStatus,
  /// Execution duration in milliseconds.
  #[serde(default)]
  pub duration_ms: u64,
  /// First 200 chars of output (for quick preview).
  #[serde(default)]
  pub output_preview: String,
  /// Error message if status != Ok.
  #[serde(default)]
  pub error: Option<String>,
  /// Input tokens consumed.
  #[serde(default)]
  pub input_tokens: u64,
  /// Output tokens produced.
  #[serde(default)]
  pub output_tokens: u64,
}
```

**Impl** `impl RunRecord`:
```rust
  pub fn now(status: RunStatus) -> Self
  pub fn with_duration(mut self, ms: u64) -> Self
  pub fn with_error(mut self, err: String) -> Self
  pub fn with_output_preview(mut self, output: &str) -> Self
```

**枚举** `ScheduleKind`:
```rust
pub enum ScheduleKind {
  /// Standard cron expression.
  Cron { expr: String },
  /// Fixed interval (e.g. every 30 minutes).
  Every { interval_ms: u64 },
  /// One-shot: run once at a specific time, then auto-disable.
  At { at: String },
}
```

#### `agents/scheduling/heartbeat_tasks.rs`

**结构体** `HeartbeatTask`:
```rust
pub struct HeartbeatTask {
  pub name: String,
  pub interval: Duration,
  pub description: String,
  pub is_paused: bool,
}
```

**结构体** `HeartbeatState`:
```rust
pub struct HeartbeatState {
  /// Map of task name -> last run timestamp (Unix millis).
  pub last_run: HashMap<String, u64>,
}
```

**Impl** `impl HeartbeatState`:
```rust
  pub fn load(path: &Path) -> Self
  pub fn save(&self, path: &Path)
```

```rust
fn parse_interval_str(s: &str) -> Option<Duration>
```

```rust
fn text_to_name(text: &str) -> String
```

```rust
pub fn parse_heartbeat(content: &str) -> (String, Vec<HeartbeatTask>)
```

```rust
fn parse_task_line(text: &str) -> Option<HeartbeatTask>
```

```rust
pub fn due_tasks<'a>(
```

```rust
pub fn build_heartbeat_prompt(context: &str, due: &[&HeartbeatTask]) -> String
```

```rust
fn parse_structured_tasks()
```

```rust
fn parse_backward_compat()
```

```rust
fn parse_no_tasks_section()
```

```rust
fn parse_interval_str_variants()
```

```rust
fn due_tasks_skips_paused_only()
```

```rust
fn due_tasks_skips_paused()
```

```rust
fn build_prompt_includes_context_and_tasks()
```

#### `agents/scheduling/mod.rs`

#### `agents/scheduling/scheduler.rs`

```rust
pub type SharedScheduler = Arc<Scheduler>
```

**结构体** `JobEntry`:
```rust
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
  /// Per-job IANA timezone override (e.g. "Asia/Shanghai").
  #[serde(default)]
  pub tz: Option<String>,
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
  // ... 17 more fields
}
```

```rust
fn default_target() -> String { "last".to_string() }
```

```rust
fn default_true() -> bool { true }
```

**结构体** `JobUpdate`:
```rust
pub struct JobUpdate {
  pub name: Option<String>,
  pub schedule: Option<String>,
  pub prompt: Option<String>,
  pub target: Option<String>,
  pub tz: Option<String>,
  pub active_hours: Option<String>,
  pub enabled: Option<bool>,
  pub delivery: Option<DeliveryConfig>,
  pub enabled_tools: Option<Vec<String>>,
  pub disabled_tools: Option<Vec<String>>,
  pub retry: Option<crate::agents::scheduling::cron_types::RetryConfig>,
  pub failure_alert: Option<crate::agents::scheduling::cron_types::FailureAlertConfig>,
  pub max_runs: Option<Option<u32>>,
  pub delete_after_run: Option<bool>,
  pub model: Option<Option<String>>,
  pub provider: Option<Option<String>>,
}
```

**结构体** `JobsFile`:
```rust
pub struct JobsFile {
  pub jobs: Vec<JobEntry>,
}
```

**结构体** `Scheduler`:
```rust
pub struct Scheduler {
  /// Jobs data protected by RwLock for concurrent access.
  jobs: RwLock<JobsFile>,
  /// Path to jobs.json on disk.
  path: PathBuf,
  /// Last known mtime (for hot-reload detection).
  last_mtime: ParkMutex<Option<SystemTime>>,
  /// Global IANA timezone.
  timezone: String,
  /// Heartbeat config.
  heartbeat_config: Option<HeartbeatConfig>,
  /// Event channel to orchestrator.
  event_tx: tokio::sync::mpsc::Sender<SchedulerEvent>,
  /// Last channel that received a user message (format
  /// `channel_type:account_id`). Read by heartbeat / cron output
  /// dispatch when the job's target is "last".
  pub last_channel: Arc<tokio::sync::Mutex<Option<String>>>,
  /// Path to persist `last_channel` across restarts.
  last_channel_file: PathBuf,
  /// Last recipient (reply_target) that received a user message.
  pub last_recipient: Arc<tokio::sync::Mutex<Option<String>>>,
  /// Path to persist `last_recipient` across restarts.
  last_recipient_file: PathBuf,
}
```

**Impl** `impl Scheduler`:
```rust
  pub fn new(
  pub async fn record_user_message(&self, channel_key: &str, reply_target: &str)
  pub fn should_run(&self) -> bool
  pub async fn run(&self)
```

**Impl** `impl Scheduler`:
```rust
  pub fn jobs(&self) -> Vec<JobEntry>
  pub fn job_count(&self) -> usize
  pub fn add_job(&self, mut entry: JobEntry) -> anyhow::Result<String>
  pub fn update_job(&self, id: &str, update: JobUpdate) -> anyhow::Result<bool>
  pub fn remove_job(&self, id: &str) -> anyhow::Result<bool>
  pub fn set_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<bool>
  pub fn mark_run_result(&self, id: &str, record: RunRecord) -> Option<String>
  pub fn drain_auto_delete(&self) -> Vec<String>
  pub fn read_run_log(&self, job_id: &str, limit: usize) -> Vec<RunRecord>
  fn run_log_path(&self, job_id: &str) -> PathBuf
  fn append_run_log_inner(&self, job_id: &str, record: &RunRecord)
```

**Impl** `impl Scheduler`:
```rust
  fn save_to_disk_inner(&self, data: &JobsFile) -> anyhow::Result<()>
  pub fn maybe_reload(&self)
  pub fn migrate_from_markdown(&self, cron_dir: &Path) -> usize
```

```rust
pub fn scan_prompt_injection(prompt: &str) -> Result<(), String>
```

```rust
pub fn validate_schedule(schedule: &str) -> Result<(), String>
```

```rust
pub fn validate_at_timestamp(at: &str) -> Result<(), String>
```

```rust
pub fn validate_tz(tz: &str) -> Result<(), String>
```

```rust
pub fn validate_active_hours(hours: &str) -> Result<(), String>
```

```rust
pub fn resolve_tz(name: &str) -> chrono_tz::Tz
```

```rust
pub fn compute_next_run(schedule: &str, last_run: Option<&str>, tz_name: &str) -> Option<String>
```

```rust
pub fn compute_next_run_full(
```

```rust
fn compute_next_run_inner(
```

```rust
fn generate_id() -> String
```

**结构体** `WebhookContext`:
```rust
pub struct WebhookContext {
  /// Shared orchestrator dependency bundle (sessions / runtime / channels /
  /// scheduler). Webhook handlers reach for `ctx.sessions`, `ctx.runtime`,
  /// `ctx.channels`, `ctx.scheduler` directly.
  pub ctx: Arc<crate::agents::OrchestratorCtx>,
  /// Timezone string used for cron evaluation in the webhook server.
  pub timezone: String,
}
```

```rust
pub fn parse_interval(s: &str) -> Option<Duration>
```

```rust
fn parse_target_channel(target: &str) -> Option<String>
```

```rust
fn parse_target_account(target: &str) -> Option<String>
```

```rust
pub fn is_active_hours(active_hours: &Option<String>, tz_name: &str) -> bool
```

```rust
fn parse_hours(s: &str) -> Option<(u32, u32)>
```

```rust
fn parse_hhmm(s: &str) -> Option<u32>
```

```rust
pub async fn run_scheduled_task(
```

```rust
pub async fn send_to_target(ctx: &WebhookContext, target: &str, content: &str)
```

```rust
pub async fn run_webhook_server(
```

```rust
async fn handle_request(
```

```rust
async fn handle_hooks_agent(
```

```rust
async fn handle_hooks_wake(
```

```rust
fn verify_hmac_signature(body: &[u8], secret: &str, header_value: &str) -> bool
```

```rust
async fn collect_body<B>(body: B) -> anyhow::Result<Bytes>
```

```rust
fn ok_response(status: StatusCode, body: &str) -> anyhow::Result<Response<Full<Bytes>>>
```

```rust
fn parse_interval_minutes()
```

```rust
fn parse_interval_hours()
```

```rust
fn parse_interval_seconds()
```

```rust
fn parse_interval_zero_disables()
```

```rust
fn parse_interval_invalid()
```

```rust
fn parse_hours_valid()
```

```rust
fn is_active_hours_no_restriction()
```

```rust
fn is_active_hours_invalid_format_always_active()
```

```rust
fn silent_heartbeat_ok()
```

```rust
fn verify_hmac_signature_valid()
```

```rust
fn verify_hmac_signature_invalid()
```

```rust
fn verify_hmac_signature_wrong_length()
```

```rust
fn validate_schedule_valid()
```

```rust
fn validate_schedule_invalid()
```

```rust
fn validate_tz_valid()
```

```rust
fn validate_tz_invalid()
```

```rust
fn validate_active_hours_valid()
```

```rust
fn validate_active_hours_invalid_format()
```

```rust
fn validate_active_hours_start_ge_end()
```

#### `agents/scheduling/webhook_loader.rs`

**结构体** `WebhookJobDef`:
```rust
pub struct WebhookJobDef {
  /// URL path，e.g. "/github/issues".
  pub path: String,
  /// HMAC secret 或 Bearer token.
  pub secret: Option<String>,
  /// 认证方式：hmac（默认）或 bearer.
  pub auth: WebhookAuth,
  /// 输出投递目标：last | none | channel name.
  pub target: String,
  /// Prompt 模板（body），可含 {{path.to.field}} 占位符.
  pub prompt_template: String,
  /// 源文件路径.
  pub source_path: std::path::PathBuf,
}
```

**枚举** `WebhookAuth`:
```rust
pub enum WebhookAuth {
  /// HMAC-SHA256，验证 X-Hub-Signature-256 header.
  Hmac,
  /// Bearer token，验证 Authorization header.
  Bearer,
}
```

```rust
pub fn parse_webhook_file(path: &Path) -> anyhow::Result<WebhookJobDef>
```

```rust
pub fn load_webhook_jobs(webhooks_dir: &Path) -> Vec<WebhookJobDef>
```

```rust
pub fn render_template(template: &str, payload: &serde_json::Value) -> String
```

```rust
fn navigate_json_value<'a>(val: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value>
```

```rust
fn parse_webhook_file_valid()
```

```rust
fn parse_webhook_file_bearer_auth()
```

```rust
fn parse_webhook_file_no_secret()
```

```rust
fn parse_webhook_file_path_normalization()
```

```rust
fn parse_webhook_file_missing_path()
```

```rust
fn parse_webhook_file_empty_body()
```

```rust
fn load_webhook_jobs_from_dir()
```

```rust
fn load_webhook_jobs_missing_dir()
```

```rust
fn render_template_simple()
```

```rust
fn render_template_nested()
```

```rust
fn render_template_array_index()
```

```rust
fn render_template_missing_field()
```

```rust
fn render_template_multiple_same_field()
```

```rust
fn render_template_no_placeholders()
```

```rust
fn render_template_number_and_bool()
```

```rust
fn render_template_unclosed_braces_ignored()
```

#### `agents/scheduling/work_unit.rs`

**结构体** `WorkUnit`:
```rust
pub struct WorkUnit {
  /// 触发该 work unit 的 user 消息索引（assistant 前面最近的 user）
  pub user_start: usize,
  /// assistant 消息在历史中的索引
  pub start: usize,
  /// 最后一个匹配的 tool result 索引（若无 tool calls 则等于 start）
  pub end: usize,
}
```

```rust
pub fn extract_work_units(history: &[ChatMessage]) -> Vec<WorkUnit>
```

```rust
pub fn find_compaction_boundary(history: &[ChatMessage], retain_count: usize) -> usize
```

```rust
fn estimate_msg_tokens(msg: &ChatMessage) -> u64
```

```rust
pub fn find_compaction_boundary_for_budget(
```

```rust
fn make_tool_call(id: &str, name: &str) -> ToolCall
```

```rust
fn test_extract_work_units()
```

```rust
fn test_boundary_with_user_preserved()
```

### `agents/session/`

**子模块说明**: Session 管理：会话创建/恢复/持久化/覆盖/回收

#### `agents/session/backend.rs`

**结构体** `InMemorySessionMeta`:
```rust
priv struct InMemorySessionMeta {
  owner: String,
  display_name: Option<String>,
  created_at: chrono::DateTime<chrono::Utc>,
  last_activity: chrono::DateTime<chrono::Utc>,
}
```

**结构体** `InMemoryBackend`:
```rust
pub struct InMemoryBackend {
  sessions: RwLock<HashMap<String, InMemorySessionMeta>>,
  messages: RwLock<HashMap<String, Vec<ChatMessage>>>,
  summaries: RwLock<HashMap<String, Vec<SummaryRecord>>>,
  active: RwLock<HashMap<String, String>>,
  counter: std::sync::atomic::AtomicU32,
}
```

**Impl** `impl InMemoryBackend`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Default for InMemoryBackend`:
```rust
  fn default() -> Self
```

**Impl** `impl SessionBackend for InMemoryBackend`:
```rust
  fn create_session(&self, owner: &str, display_name: Option<&str>) -> std::io::Result<SessionInfo>
  fn delete_session(&self, session_id: &str) -> std::io::Result<()>
  fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()>
  fn get_session(&self, session_id: &str) -> Option<SessionInfo>
  fn list_sessions(&self, owner: &str) -> Vec<SessionInfo>
  fn list_all_sessions(&self) -> Vec<SessionInfo>
  fn get_active_session(&self, user_id: &str) -> Option<String>
  fn set_active_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()>
  fn load_messages(&self, session_id: &str) -> Vec<ChatMessage>
  fn append_message(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<i64>
  fn truncate_messages(&self, session_id: &str, keep_count: usize) -> std::io::Result<()>
  fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool>
  fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()>
  fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord>
  fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)>
  fn clear_summary(&self, session_id: &str) -> std::io::Result<()>
  fn cleanup_stale(&self, _ttl_hours: u32) -> std::io::Result<usize>
```

**Trait** `PersistHook`:
```rust
pub trait PersistHook {
  fn persist_message(&self, session_id: &str, message: &ChatMessage) -> Option<i64>
  fn save_compaction(&self, session_id: &str, summary: &SummaryRecord)
  fn rotate_history(&self, session_id: &str, surviving: &[(i64, ChatMessage)])
  fn save_token_count(&self, session_id: &str, total: u64)
  fn save_session_override(&self, session_id: &str, override_json: &str)
  fn save_last_message(&self, session_id: &str, msg: &crate::channels::ChannelMessage)
  fn truncate_messages(&self, session_id: &str, keep_count: usize)
}
```

**结构体** `BackendPersistHook`:
```rust
pub struct BackendPersistHook {
  backend: Arc<dyn SessionBackend>,
}
```

**Impl** `impl BackendPersistHook`:
```rust
  pub fn new(backend: Arc<dyn SessionBackend>) -> Self
```

**Impl** `impl PersistHook for BackendPersistHook`:
```rust
  fn persist_message(&self, session_id: &str, message: &ChatMessage) -> Option<i64>
  fn save_compaction(&self, session_id: &str, summary: &SummaryRecord)
  fn rotate_history(&self, session_id: &str, surviving: &[(i64, ChatMessage)])
  fn save_token_count(&self, session_id: &str, total: u64)
  fn save_session_override(&self, session_id: &str, override_json: &str)
  fn save_last_message(&self, session_id: &str, msg: &crate::channels::ChannelMessage)
  fn truncate_messages(&self, session_id: &str, keep_count: usize)
```

#### `agents/session/manager.rs`

**结构体** `SessionNotOwned`:
```rust
pub struct SessionNotOwned {
  pub session_id: String,
  pub routing_key: String,
}
```

**Impl** `impl fmt::Display for SessionNotOwned`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

**结构体** `SessionManager`:
```rust
pub struct SessionManager {
  backend: Arc<dyn SessionBackend>,
  /// User's active session: user_id → session_id.
  /// Per-routing-key SessionContext. At most one SessionContext per
  /// routing_key (the 1:1 invariant): every active routing_key has a
  /// SessionContext that wraps its active Session.
  contexts: RwLock<HashMap<String, Arc<SessionContext>>>,
  /// AgentRegistry used to resolve `Session.agent_name` to an
  /// `Arc<Agent>` when building SessionContexts. Defaults to an
  /// empty registry for test-only managers; production daemons
  /// install the workspace-loaded registry via `with_agents`.
  /// Stored as `Arc` so it stays in sync with AgentRuntime's view.
  agents: Arc<AgentRegistry>,
  /// User resolver — maps routing_key → user_id for per-user paths
  /// (profile, memory). Held here so `list_sessions_for_user` and
  /// future per-user lookups don't need to take it as a parameter.
  resolver: Arc<UserResolver>,
}
```

**Impl** `impl SessionManager`:
```rust
  pub fn new(backend: Arc<dyn SessionBackend>) -> Self
  pub fn with_agents(mut self, agents: Arc<AgentRegistry>) -> Self
  pub fn with_resolver(mut self, resolver: Arc<UserResolver>) -> Self
  pub fn resolver(&self) -> &Arc<UserResolver>
  pub fn in_memory() -> Self
  pub fn backend(&self) -> &Arc<dyn SessionBackend>
  pub fn get_or_create(&self, user_id: &str) -> Session
  fn resolve_active(&self, user_id: &str) -> String
  pub fn new_session(&self, user_id: &str, name: Option<&str>) -> std::io::Result<SessionInfo>
  pub fn switch_session(&self, user_id: &str, session_id: &str) -> std::io::Result<SessionInfo>
  pub fn delete_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()>
  pub fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()>
  pub fn list_sessions(&self, user_id: &str) -> Vec<SessionInfo>
  pub fn list_sessions_for_user(
  pub fn list_sub_sessions(&self, parent_session_id: &str) -> Vec<SessionInfo>
  pub fn session_id_for_routing_key(&self, routing_key: &str) -> Option<String>
  pub fn get_by_id(&self, session_id: &str) -> Option<Session>
  pub fn create_sub_session_context(
  pub fn create_sub_session(
  pub fn list_all_sessions(&self) -> Vec<SessionInfo>
  pub fn active_session_id(&self, user_id: &str) -> Option<String>
  pub fn save_session_override(&self, user_id: &str, session_override: SessionOverride)
  pub fn get_session_override(&self, user_id: &str) -> SessionOverride
  pub fn get_context(&self, routing_key: &str) -> Option<Arc<SessionContext>>
  pub fn get_or_create_context(&self, routing_key: &str) -> Arc<SessionContext>
  pub fn get_or_create_context_with<F>(
  pub fn build_persist_hook(&self) -> Arc<dyn super::PersistHook>
  fn build_agent_for_session(&self, session: &Session) -> Arc<Agent>
  pub fn register_context(&self, routing_key: &str, ctx: Arc<SessionContext>)
  pub fn drop_context(&self, routing_key: &str)
  pub fn append_message(&self, session_id: &str, message: ChatMessage)
```

**Impl** `impl Default for SessionManager`:
```rust
  fn default() -> Self
```

```rust
fn permissive_main_default(agent_name: &str) -> SubAgentConfig
```

#### `agents/session/mod.rs`

#### `agents/session/recovery.rs`

**结构体** `BreakpointItem`:
```rust
pub struct BreakpointItem {
  pub tool_call_id: String,
  pub tool_name: String,
  /// JSON-encoded arguments string.
  pub arguments: String,
}
```

```rust
pub fn identify_breakpoint(messages: &[ChatMessage]) -> Vec<BreakpointItem>
```

```rust
pub fn detect_incomplete_turn(messages: &[ChatMessage]) -> bool
```

#### `agents/session/session_override.rs`

**结构体** `SessionOverride`:
```rust
pub struct SessionOverride {
  /// Force a specific model ID instead of the routing default.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub model: Option<String>,
  /// Override thinking/reasoning mode. None = use model's `reasoning` field.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub thinking: Option<bool>,
  /// Thinking effort level when thinking is enabled ("low"/"medium"/"high").
  #[serde(skip_serializing_if = "Option::is_none")]
  pub effort: Option<String>,
  /// Override permission mode for this session.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub permission_mode: Option<crate::config::agent::PermissionMode>,
  /// Override run mode for this session (Interactive vs Background).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub run_mode: Option<crate::config::agent::RunMode>,
  /// Override max tool calls per turn (0 = unlimited).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub max_tool_calls: Option<usize>,
  /// Override compaction trigger threshold (0.0..1.0).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub compact_threshold: Option<f64>,
  /// Override number of recent work units to retain during compaction.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub retain_work_units: Option<usize>,
  /// Replace the assembled system prompt with this string. Used by
  /// `DelegationCoordinator` so sub-agent turns can run through
  /// `SessionContext::process_turn` while still seeing their
  /// minimal AGENT.md identity prompt instead of the full builder
  /// output. Not persisted — set per-context-lifecycle.
  #[serde(skip)]
  // ... 1 more fields
}
```

**Impl** `impl SessionOverride`:
```rust
  pub fn is_empty(&self) -> bool
  pub fn to_thinking_config(&self) -> Option<crate::providers::ThinkingConfig>
```

```rust
pub fn sanitize_history(history: &mut Vec<ChatMessage>)
```

```rust
pub(super) fn sanitize_paired(pairs: Vec<(i64, ChatMessage)>) -> Vec<(i64, ChatMessage)>
```

#### `agents/session/types.rs`

**结构体** `SummaryMetadata`:
```rust
pub struct SummaryMetadata {
  pub version: u32,
  pub token_estimate: u64,
  pub up_to_message: i64,
}
```

**结构体** `Session`:
```rust
pub struct Session {
  /// Session ID (e.g. "k3jr9px2").
  pub id: String,
  /// Owner routing key (e.g. "telegram:default:12345").
  /// RFC v2 renames "owner" semantically to `routing_key`; the field stays
  /// named `owner` for source-diff churn reasons.
  pub owner: String,
  /// Agent name that owns this session. References `workspace/agents/{name}/AGENT.md`.
  /// Defaults to "main"; sub-sessions inherit their delegating agent's name.
  pub agent_name: String,
  /// Parent session ID for sub-sessions spawned by `agent_delegate`.
  /// `None` for top-level user sessions.
  pub parent_session_id: Option<String>,
  /// Current conversation history (in-memory).
  pub history: Vec<ChatMessage>,
  /// Parallel to `history`: database message IDs, 0 for summary or unpersisted messages.
  pub message_ids: Vec<i64>,
  /// Monotonic compaction version counter.
  pub compact_version: u32,
  /// In-memory summary metadata (restored from backend on load).
  pub summary_metadata: Option<SummaryMetadata>,
  /// Per-session runtime overrides set by slash commands.
  pub session_override: SessionOverride,
  /// Set when the last persisted turn ended with a user message but no
  /// corresponding assistant response (e.g. daemon crash/SIGKILL). The
  /// orchestrator will prompt the user to retry or abort on the next
  /// interaction. Not persisted — rebuilt on every session load.
  pub incomplete_turn: bool,
  /// Last incoming ChannelMessage. Carries sender, reply_target, attachments,
  /// images. Persisted so startup recovery can reconstruct the routing
  /// context and resume an interrupted turn. RFC v2 §三.A replaces the old
  // ... 5 more fields
}
```

**Impl** `impl Clone for Session`:
```rust
  fn clone(&self) -> Self
```

**Impl** `impl std::fmt::Debug for Session`:
```rust
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
```

**Impl** `impl Session`:
```rust
  pub fn new(id: String) -> Self
  pub fn with_persist(mut self, persist: Arc<dyn PersistHook>) -> Self
  pub fn with_channel(mut self, channel: Arc<dyn Channel>) -> Self
  pub fn save_to_disk(&self)
  pub fn reply_target(&self) -> Option<&str>
  pub fn record_inbound(&mut self, msg: ChannelMessage)
  pub fn add_user(&mut self, text: String)
  pub fn add_user_with_media(&mut self, text: String, media: Vec<ContentPart>)  // 媒体在前 + Text 在后
  pub fn add_user_text(&mut self, text: String)
  pub fn add_assistant(&mut self, text: String)
  pub fn add_assistant_text(&mut self, text: String)
  pub fn add_assistant_with_tools(
  pub fn add_tool_result(&mut self, tool_call_id: String, content: String, is_error: bool)
  pub fn add_system_text(&mut self, text: String)
  pub fn pop_last_assistant(&mut self)
  pub fn rollback_to(&mut self, len: usize)
  pub(crate) fn apply_compaction(
  pub(crate) fn drop_pre_boundary(&mut self, boundary: usize, version: u32)
```

### `agents/workspace/`

**子模块说明**: 工作空间：Agent 配置加载、Skill 加载与发现、文件监视

#### `agents/workspace/agent_loader.rs`

```rust
pub fn parse_agent_file(path: &Path) -> Result<SubAgentConfig>
```

```rust
pub fn load_agents_from_dir(agents_dir: &Path) -> Vec<SubAgentConfig>
```

```rust
pub fn validate_agents(agents: &[SubAgentConfig], known_tools: &[&str]) -> Vec<String>
```

```rust
fn test_parse_agent_file()
```

```rust
fn test_parse_agent_file_minimal()
```

```rust
fn test_parse_agent_file_with_model()
```

```rust
fn test_parse_agent_file_model_null()
```

```rust
fn test_parse_agent_fallback_name()
```

```rust
fn test_load_agents_from_dir()
```

```rust
fn test_load_agents_missing_dir()
```

```rust
fn test_validate_agents_duplicate()
```

```rust
fn test_validate_agents_unknown_tool()
```

#### `agents/workspace/mod.rs`

#### `agents/workspace/skill_loader.rs`

**结构体** `SkillDefinition`:
```rust
pub struct SkillDefinition {
  pub name: String,
  pub description: String,
  pub keywords: Vec<String>,
  pub prompt_body: String,
  pub source_path: PathBuf,
  pub version: Option<String>,
  pub when_to_use: Option<String>,
  pub argument_hint: Option<String>,
  pub arguments: Vec<String>,
  pub user_invocable: bool,
  pub agent_invocable: bool,
}
```

```rust
pub fn parse_skill_file(path: &Path) -> Result<SkillDefinition>
```

```rust
pub fn load_skills_from_dir(skills_dir: &Path) -> Vec<SkillDefinition>
```

```rust
fn test_parse_skill_file()
```

```rust
fn test_parse_skill_file_new_fields()
```

```rust
fn test_parse_skill_file_defaults()
```

```rust
fn test_load_skills_from_dir()
```

```rust
fn test_load_skills_missing_dir()
```

#### `agents/workspace/skills.rs`

**结构体** `Skill`:
```rust
pub struct Skill {
  pub name: String,
  pub description: String,
  pub keywords: Vec<String>,
  pub prompt_body: String,
  pub version: Option<String>,
  pub when_to_use: Option<String>,
  pub argument_hint: Option<String>,
  pub arguments: Vec<String>,
  pub user_invocable: bool,
  pub agent_invocable: bool,
  pub skill_dir: Option<PathBuf>,
}
```

**Impl** `impl Skill`:
```rust
  pub fn from_definition(def: &SkillDefinition) -> Self
```

**结构体** `SkillManager`:
```rust
pub struct SkillManager {
  skills: HashMap<String, Skill>,
}
```

**Impl** `impl Default for SkillManager`:
```rust
  fn default() -> Self
```

**Impl** `impl SkillManager`:
```rust
  pub fn new() -> Self
  pub fn register(&mut self, skill: Skill)
  pub fn skill_count(&self) -> usize
  pub fn skills_iter(&self) -> impl Iterator<Item = (&str, &Skill)>
  pub fn agent_skills_iter(&self) -> impl Iterator<Item = (&str, &Skill)>
  pub fn skill_dir(&self, name: &str) -> Option<&Path>
  pub fn get(&self, name: &str) -> Option<&Skill>
  pub fn reload(&mut self, new_skills: Vec<Skill>)
  pub fn skill_prompts(&self) -> Vec<(&str, &str)>
```

#### `agents/workspace/watcher.rs`

**结构体** `ChangeSet`:
```rust
pub struct ChangeSet {
  pub skills_changed: bool,
  pub agents_changed: bool,
  pub memory_changed: bool,
}
```

**结构体** `WorkspaceWatcher`:
```rust
pub struct WorkspaceWatcher {
  /// 变化信号接收端（AgentLoop 持有）
  pub rx: watch::Receiver<ChangeSet>,
  // watcher 必须存活才能持续监听
  _watcher: RecommendedWatcher,
}
```

**Impl** `impl WorkspaceWatcher`:
```rust
  pub fn new(workspace_dir: &Path, knowledge_dir: &Path) -> Result<Self>
  pub fn spawn_managed(
```

**结构体** `ManagedWatcherGuard`:
```rust
pub struct ManagedWatcherGuard {
  _watcher: WorkspaceWatcher,
  _handle: tokio::task::JoinHandle<()>,
  cancel: tokio_util::sync::CancellationToken,
  /// Directories the watcher reloads on change — exposed so callers can
  /// trigger a manual reload (e.g. `/reload` slash command).
  pub agents_dir: PathBuf,
  pub skills_dir: PathBuf,
}
```

**Impl** `impl Drop for ManagedWatcherGuard`:
```rust
  fn drop(&mut self)
```

---

## `channels/`

**模块说明**: 消息通道：多平台适配（Telegram/QQBot/微信/浏览器 Client）、消息分块、流式推送、安全策略

**外部模块依赖**: `agents`, `channel`, `config`, `keyboard`, `markdown`, `message`, `token`, `types`

#### `channels/client.rs`

**结构体** `StreamContext`:
```rust
priv struct StreamContext {
  event_tx: mpsc::Sender<TurnEvent>,
  cancel: CancellationToken,
}
```

**结构体** `ClientConnection`:
```rust
priv struct ClientConnection {
  /// WebSocket sender (clone of the split sink, wrapped as mpsc for simplicity).
  ws_sender: mpsc::Sender<String>,
  /// Current active session key for this connection.
  #[allow(dead_code)]
  active_session: String,
  /// Set of session keys owned by this connection.
  sessions: std::collections::HashSet<String>,
}
```

**结构体** `ClientChannel`:
```rust
pub struct ClientChannel {
  config: ClientConfig,
  /// Outgoing messages for Orchestrator (filled by WS handlers).
  message_tx: mpsc::Sender<ChannelMessage>,
  /// One-time take for listen().
  message_rx: Mutex<Option<mpsc::Receiver<ChannelMessage>>>,
  /// Pre-bound listener passed from the old process during hot switch.
  /// When set, start() reuses it instead of calling bind().
  pre_bound: SyncMutex<Option<std::net::TcpListener>>,
  /// Per-session streaming context.
  stream_contexts: Arc<RwLock<HashMap<String, StreamContext>>>,
  /// Active connections: connection_id → ClientConnection.
  connections: Arc<RwLock<HashMap<String, ClientConnection>>>,
  /// Reverse map: session_key → connection_id.
  session_owners: Arc<RwLock<HashMap<String, String>>>,
  /// Session manager for management API (set after construction).
  session_manager: Arc<OnceLock<Arc<crate::agents::SessionManager>>>,
  /// Tool specs for management API (set after construction).
  tool_specs: Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
  /// Workspace directory for memory API (set after construction).
  workspace_dir: Arc<OnceLock<std::path::PathBuf>>,
  /// Config file path for config read/write API (set after construction).
  config_path: Arc<OnceLock<std::path::PathBuf>>,
  /// Skill manager for skills API (set after construction).
  skill_manager: Arc<OnceLock<Arc<RwLock<crate::agents::SkillManager>>>>,
  /// Service registry for models API (set after construction).
  provider_registry: Arc<OnceLock<Arc<dyn crate::providers::ProviderRegistry>>>,
}
```

**Impl** `impl ClientChannel`:
```rust
  pub fn new(config: ClientConfig) -> Self
  pub fn set_pre_bound(&self, listener: std::net::TcpListener)
  pub fn set_session_manager(&self, sm: Arc<crate::agents::SessionManager>)
  pub fn set_tool_specs(&self, specs: Vec<crate::providers::capability_tool::ToolSpec>)
  pub fn set_workspace_dir(&self, dir: std::path::PathBuf)
  pub fn set_config_path(&self, path: std::path::PathBuf)
  pub fn set_skill_manager(&self, sm: Arc<RwLock<crate::agents::SkillManager>>)
  pub fn set_provider_registry(&self, sr: Arc<dyn crate::providers::ProviderRegistry>)
  async fn start(&self) -> anyhow::Result<()>
```

**Impl** `impl Channel for ClientChannel`:
```rust
  fn name(&self) -> &str
  fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities
  async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>
  async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>
  async fn health_check(&self) -> bool
  fn create_stream(
```

**结构体** `ClientTurnStream`:
```rust
pub struct ClientTurnStream {
  reply_target: String,
  event_tx: mpsc::Sender<TurnEvent>,
  cancel: CancellationToken,
  status: crate::channels::StreamDelivery,
  finished: bool,
}
```

**Impl** `impl crate::channels::TurnStream for ClientTurnStream`:
```rust
  async fn push(
  fn status(&self) -> crate::channels::StreamDelivery
  async fn finish(mut self: Box<Self>) -> crate::channels::StreamDelivery
  async fn abort(mut self: Box<Self>)
  fn cancel_token(&self) -> Option<CancellationToken>
```

**Impl** `impl Drop for ClientTurnStream`:
```rust
  fn drop(&mut self)
```

**结构体** `ApiContext`:
```rust
priv struct ApiContext<'a> {
  /// Session-manager scope key (channel:account:sender), stable across reconnects.
  user_id: &'a str,
  session_manager: &'a Arc<OnceLock<Arc<crate::agents::SessionManager>>>,
  tool_specs: &'a Arc<RwLock<Vec<crate::providers::capability_tool::ToolSpec>>>,
  workspace_dir: &'a Arc<OnceLock<std::path::PathBuf>>,
  config_path: &'a Arc<OnceLock<std::path::PathBuf>>,
  skill_manager: &'a Arc<OnceLock<Arc<RwLock<crate::agents::SkillManager>>>>,
  provider_registry: &'a Arc<OnceLock<Arc<dyn crate::providers::ProviderRegistry>>>,
}
```

```rust
fn handle_api_request(
```

```rust
fn reconstruct_history(
```

#### `channels/message.rs`

**枚举** `LenUnit`:
```rust
pub enum LenUnit {
  /// Unicode code points (Rust `chars().count()`).
  Codepoints,
  /// UTF-16 code units (Telegram's measure — emoji counts as 2).
  Utf16Units,
  /// Raw UTF-8 bytes.
  Bytes,
}
```

**结构体** `ChannelCapabilities`:
```rust
pub struct ChannelCapabilities {
  pub supports_streaming: bool,
  pub supports_edit: bool,
  pub supports_delete: bool,
  pub supports_inline_buttons: bool,
  pub supports_media: bool,
  pub supports_threads: bool,
  /// Maximum length of a single message; messages longer than this must be split.
  pub message_chunk_limit: usize,
  pub message_len_unit: LenUnit,
}
```

**结构体** `MessageId`:
```rust
pub struct MessageId {
}
```

**Impl** `impl MessageId`:
```rust
  pub fn new(id: impl Into<String>) -> Self
  pub fn as_str(&self) -> &str
```

```rust
pub type SendResult = Option<MessageId>
```

**结构体** `SendTarget`:
```rust
pub struct SendTarget {
  pub recipient: String,
  pub thread_id: Option<String>,
  pub cancellation_token: Option<CancellationToken>,
}
```

**Impl** `impl SendTarget`:
```rust
  pub fn new(recipient: impl Into<String>) -> Self
  pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self
  pub fn with_cancel(mut self, token: CancellationToken) -> Self
```

**枚举** `MediaSource`:
```rust
pub enum MediaSource {
  /// Remote URL (HTTP/HTTPS).
  Url(String),
  /// In-memory bytes (e.g. from Telegram file API decryption).
  Inline {
  data: Vec<u8>,
  mime_type: Option<String>,
  file_name: Option<String>,
  },
}
```

**枚举** `MessagePayload`:
```rust
pub enum MessagePayload {
  /// Plain text.
  Text { text: String },
  /// Text + inline action buttons. Channels without button support
  /// downgrade by sending the text alone.
  Interactive {
  text: String,
  buttons: Vec<InlineButton>,
  },
  /// Media (image/file) with optional caption.
  Media {
  source: MediaSource,
  caption: Option<String>,
  },
}
```

**Impl** `impl MessagePayload`:
```rust
  pub fn text(text: impl Into<String>) -> Self
  pub fn to_fallback_text(&self) -> String
```

**结构体** `ChannelMessage`:
```rust
pub struct ChannelMessage {
  pub id: String,
  pub sender: String,
  pub reply_target: String,
  pub content: String,
  pub timestamp: u64,
  pub thread_ts: Option<String>,
  pub interruption_scope_id: Option<String>,
  pub attachments: Vec<MediaAttachment>,
  /// URLs of images attached to this message (e.g. from Telegram photo messages).
  pub image_urls: Option<Vec<String>>,
  /// Base64-encoded image data (used when the source URL is not directly
  /// accessible by the LLM provider, e.g. Telegram file API).
  pub image_base64: Option<Vec<String>>,
}
```

**结构体** `InlineButton`:
```rust
pub struct InlineButton {
  /// Button label displayed to the user.
  pub label: String,
  /// Callback data sent back when the button is clicked.
  /// For Telegram: max 64 bytes.
  pub callback_data: String,
}
```

**枚举** `CallbackAction`:
```rust
pub enum CallbackAction {
  /// User asked to retry the last failed turn.
  Retry { session_key_prefix: String },
  /// User asked to abort the pending retry prompt.
  Abort { session_key_prefix: String },
  /// Future-extension hook for app-defined callbacks.
  Custom { tag: String, data: String },
}
```

**Impl** `impl CallbackAction`:
```rust
  pub fn serialize(&self) -> String
  pub fn parse(s: &str) -> Option<Self>
```

**结构体** `SendMessage`:
```rust
pub struct SendMessage {
  pub content: String,
  pub recipient: String,
  pub subject: Option<String>,
  pub thread_ts: Option<String>,
  pub cancellation_token: Option<CancellationToken>,
  pub attachments: Vec<MediaAttachment>,
  pub image_urls: Option<Vec<String>>,
  /// Optional inline buttons (Telegram inline_keyboard, etc.)
  /// Channels that don't support buttons silently ignore this field.
  pub inline_buttons: Option<Vec<InlineButton>>,
}
```

**Impl** `impl SendMessage`:
```rust
  pub fn new(content: impl Into<String>, recipient: impl Into<String>) -> Self
  pub fn is_verbose(&self, chunk_limit: usize) -> bool
```

**结构体** `MediaAttachment`:
```rust
pub struct MediaAttachment {
  pub file_name: String,
  pub data: Vec<u8>,
  pub mime_type: Option<String>,
}
```

**枚举** `ProcessingStatus`:
```rust
pub enum ProcessingStatus {
  /// LLM call started — the bot is "thinking".
  Thinking,
  /// Response sent successfully (status cleanup already handled in send()).
  Done,
  /// An error occurred during processing.
  Error,
}
```

**Trait** `Channel`:
```rust
pub trait Channel {
  fn name(&self) -> &str
  async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>
  async fn send_payload(
  async fn edit_message(
  async fn delete_message(
  async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>
  async fn health_check(&self) -> bool
  async fn on_status(&self, _recipient: &str, _status: ProcessingStatus) {}
  fn capabilities(&self) -> &ChannelCapabilities
  fn message_len(&self, text: &str) -> usize
  fn supports_streaming(&self) -> bool
  fn create_stream(
  fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy
  fn check_authorization(
}
```

**结构体** `DedupState`:
```rust
pub struct DedupState {
  inner: Arc<Mutex<DedupInner>>,
  capacity: usize,
}
```

**结构体** `DedupInner`:
```rust
priv struct DedupInner {
  /// Set membership for O(1) lookup.
  seen: std::collections::HashSet<String>,
  /// Insertion order for FIFO eviction.
  order: std::collections::VecDeque<String>,
}
```

```rust
const DEFAULT_DEDUP_CAPACITY: usize = 50_000
```

**Impl** `impl Default for DedupState`:
```rust
  fn default() -> Self
```

**Impl** `impl DedupState`:
```rust
  pub fn new() -> Self
  pub fn with_capacity(capacity: usize) -> Self
  pub fn check_and_record(&self, id: &str) -> bool
  pub(crate) fn len(&self) -> usize
```

```rust
pub fn split_message_chunk(message: &str, limit: usize, unit: LenUnit) -> Vec<String>
```

```rust
pub fn split_message_chunk_chars(message: &str, limit: usize) -> Vec<String>
```

```rust
fn measure(s: &str, unit: LenUnit) -> usize
```

```rust
fn char_cost(c: char, unit: LenUnit) -> usize
```

```rust
fn find_split_point(text: &str, limit: usize, unit: LenUnit) -> usize
```

```rust
fn find_last_pattern(chars: &[char], pattern: &[char]) -> Option<usize>
```

```rust
fn find_last_char(chars: &[char], target: char) -> Option<usize>
```

```rust
fn callback_action_roundtrip_retry_abort()
```

```rust
fn callback_action_custom_passthrough()
```

```rust
fn callback_action_rejects_non_callback()
```

```rust
fn message_payload_fallback_text()
```

```rust
fn dedup_state_basic_dedup()
```

```rust
fn dedup_state_bounded_eviction()
```

```rust
fn dedup_state_recent_ids_still_dedup()
```

#### `channels/mod.rs`

#### `channels/security.rs`

**枚举** `AllowList`:
```rust
pub enum AllowList {
  All,
  Whitelist(Vec<String>),
}
```

**Impl** `impl AllowList`:
```rust
  pub fn from_config(opt: Option<Vec<String>>) -> Self
  pub fn allows(&self, sender: &str) -> bool
  pub fn is_empty_whitelist(&self) -> bool
```

**枚举** `GroupAuthMode`:
```rust
pub enum GroupAuthMode {
  /// Reject all group messages (Phase 4 default — "统一关").
  Reject,
  /// Accept all group messages that pass `group_allowlist`.
  Open,
  /// Only accept group messages that @mention the bot (Telegram
  /// `mention_only` carryover).
  MentionOnly,
}
```

**结构体** `ChannelSecurityPolicy`:
```rust
pub struct ChannelSecurityPolicy {
  pub allowed_users: AllowList,
  pub group_mode: GroupAuthMode,
  pub group_allowlist: AllowList,
}
```

**Impl** `impl ChannelSecurityPolicy`:
```rust
  pub fn open() -> Self
  pub fn dm_only() -> Self
```

**枚举** `MessageScope`:
```rust
pub enum MessageScope {
  Direct,
  Group { id: &'a str, has_mention: bool },
}
```

**枚举** `AuthDecision`:
```rust
pub enum AuthDecision {
  Allow,
  Ignore,
  Reject { reason: &'static str },
}
```

**Impl** `impl AuthDecision`:
```rust
  pub fn allowed(self) -> bool
```

```rust
pub fn warn_if_locked_down(channel: &dyn super::Channel)
```

```rust
pub fn evaluate(policy: &ChannelSecurityPolicy, sender: &str, scope: MessageScope<'_>) -> AuthDecision
```

```rust
fn allow_list_from_config_canonical()
```

```rust
fn allow_list_allows()
```

```rust
fn allow_list_is_empty_whitelist()
```

```rust
fn evaluate_direct_allowed()
```

```rust
fn evaluate_group_default_reject_ignores_silently()
```

```rust
fn evaluate_group_mention_only()
```

```rust
fn evaluate_group_allowlist_filters()
```

```rust
fn auth_decision_allowed()
```

#### `channels/turn_stream.rs`

**枚举** `StreamDelivery`:
```rust
pub enum StreamDelivery {
  /// Buffered locally; not yet observed by the consumer.
  #[default]
  Pending,
  /// Delivered to the transport layer (e.g. WS send returned, HTTP 200).
  /// The consumer has the bytes but hasn't acknowledged completion.
  Visible,
  /// Consumer has acknowledged final delivery (e.g. client ack frame,
  /// Telegram's final editMessageText success).
  FinalDelivered,
}
```

**Trait** `TurnStream`:
```rust
pub trait TurnStream {
  async fn push(&mut self, event: TurnEvent) -> anyhow::Result<StreamDelivery>
  fn status(&self) -> StreamDelivery
  async fn finish(self: Box<Self>) -> StreamDelivery
  async fn abort(self: Box<Self>)
  fn cancel_token(&self) -> Option<CancellationToken>
}
```

#### `channels/wechat.rs`

```rust
const CHANNEL_VERSION: &str = "2.1.7"
```

```rust
const ILINK_APP_ID: &str = "bot"
```

```rust
const QR_POLL_INTERVAL_SECS: u64 = 3
```

```rust
const QR_MAX_ATTEMPTS: u64 = 60
```

```rust
const RATE_LIMIT_PAUSE_SECS: u64 = 3600
```

```rust
const MAX_CONSECUTIVE_ERRORS: u32 = 10
```

```rust
const MESSAGE_TYPE_BOT: i64 = 2
```

```rust
const MESSAGE_STATE_FINISH: i64 = 2
```

```rust
const ITEM_TYPE_TEXT: i64 = 1
```

```rust
const TYPING_STATUS_TYPING: i64 = 1
```

```rust
const TYPING_STATUS_CANCEL: i64 = 2
```

```rust
fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8>
```

```rust
fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, String>
```

```rust
fn encrypt_ecb(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8>
```

```rust
fn decrypt_ecb(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String>
```

```rust
fn build_base_info() -> BaseInfo
```

```rust
fn build_client_version() -> u32
```

**结构体** `BaseInfo`:
```rust
priv struct BaseInfo {
  channel_version: String,
}
```

**结构体** `IlinkMessage`:
```rust
priv struct IlinkMessage {
  #[serde(default)]
  from_user_id: String,
  #[serde(default)]
  to_user_id: String,
  #[serde(default)]
  client_id: String,
  #[serde(default)]
  create_time_ms: i64,
  #[serde(default)]
  group_id: String,
  #[serde(rename = "type", default)]
  message_type: i64,
  #[serde(rename = "state", default)]
  message_state: i64,
  #[serde(default)]
  list: Vec<MessageItem>,
  #[serde(default)]
  context_token: String,
}
```

**Impl** `impl IlinkMessage`:
```rust
  fn chat_id(&self) -> &str
  fn is_group(&self) -> bool { !self.group_id.is_empty() }
```

**结构体** `MessageItem`:
```rust
priv struct MessageItem {
  #[serde(rename = "type", default)]
  item_type: i64,
  #[serde(default)]
  text_item: Option<TextItem>,
}
```

**结构体** `TextItem`:
```rust
priv struct TextItem {
  #[serde(default)]
  text: String,
}
```

**结构体** `GetUpdatesResponse`:
```rust
priv struct GetUpdatesResponse {
  #[serde(default)]
  ret: i64,
  #[serde(default)]
  errmsg: String,
  #[serde(rename = "get_updates_buf", default)]
  get_updates_buf: String,
  #[serde(default)]
  msgs: Vec<IlinkMessage>,
}
```

**结构体** `GetConfigResponse`:
```rust
priv struct GetConfigResponse {
  #[serde(default)]
  ret: i64,
  #[serde(default)]
  errmsg: String,
  #[serde(default)]
  wxid: String,
  #[serde(default)]
  nickname: String,
  #[serde(default)]
  typing_ticket: String,
  #[serde(default)]
  aeskey: String,
}
```

**结构体** `QrCodeResponse`:
```rust
priv struct QrCodeResponse {
  #[serde(default)]
  qrcode: String,
  #[serde(default)]
  qrcode_img_content: String,
}
```

**结构体** `QrStatus`:
```rust
priv struct QrStatus {
  #[serde(default)]
  status: String,
  #[serde(default)]
  bot_token: String,
  #[serde(default)]
  ilink_bot_id: String,
  #[serde(default)]
  baseurl: String,
  #[serde(default)]
  ilink_user_id: String,
  #[serde(default)]
  nickname: String,
}
```

**结构体** `GetUpdatesRequest`:
```rust
priv struct GetUpdatesRequest {
  #[serde(rename = "get_updates_buf")]
  get_updates_buf: String,
  #[serde(rename = "base_info")]
  base_info: BaseInfo,
}
```

**结构体** `SendMessageRequest`:
```rust
priv struct SendMessageRequest {
  #[serde(rename = "snake_case")]
  msg: SendMessageMsg,
  #[serde(rename = "base_info")]
  base_info: BaseInfo,
}
```

**结构体** `SendMessageMsg`:
```rust
priv struct SendMessageMsg {
  #[serde(default)]
  from_user_id: String,
  to_user_id: String,
  client_id: String,
  #[serde(rename = "type")]
  message_type: i64,
  #[serde(rename = "state")]
  message_state: i64,
  #[serde(rename = "list")]
  item_list: Vec<SendMessageItem>,
  #[serde(rename = "context_token", skip_serializing_if = "Option::is_none")]
  context_token: Option<String>,
}
```

**结构体** `SendMessageItem`:
```rust
priv struct SendMessageItem {
  #[serde(rename = "type")]
  item_type: i64,
  #[serde(rename = "text_item")]
  text_item: SendTextItem,
}
```

**结构体** `SendTextItem`:
```rust
priv struct SendTextItem {
}
```

**结构体** `SendTypingRequest`:
```rust
priv struct SendTypingRequest {
  #[serde(rename = "ilink_user_id")]
  ilink_user_id: String,
  #[serde(rename = "typing_ticket")]
  typing_ticket: String,
  status: i64,
  #[serde(rename = "base_info")]
  base_info: BaseInfo,
}
```

**结构体** `GetConfigRequest`:
```rust
priv struct GetConfigRequest {
  #[serde(rename = "ilink_user_id")]
  ilink_user_id: String,
  #[serde(rename = "base_info")]
  base_info: BaseInfo,
}
```

**结构体** `GetBotQrCodeRequest`:
```rust
priv struct GetBotQrCodeRequest {
}
```

**结构体** `GetQrCodeStatusRequest`:
```rust
priv struct GetQrCodeStatusRequest {
  #[serde(rename = "qrcode")]
  qrcode: String,
  #[serde(rename = "base_info")]
  base_info: BaseInfo,
}
```

**枚举** `ApiError`:
```rust
priv enum ApiError {
  #[error("Network error: {0}")]
  Network(String),
  #[error("HTTP {0}: {1}")]
  Http(u16, String),
  #[error("Parse error: {0}")]
  Parse(String),
  #[error("API error {0}: {1}")]
  Api(i64, String),
  #[error("Not authenticated")]
  NotAuthenticated,
}
```

**结构体** `SharedState`:
```rust
priv struct SharedState {
  bot_token: Option<String>,
  bot_wxid: Option<String>,
  bot_nickname: Option<String>,
  get_updates_buf: String,
  typing_ticket: Option<String>,
  aes_key: Option<String>,
  context_tokens: HashMap<String, String>,
  api_base: Option<String>,
}
```

**结构体** `ApiClient`:
```rust
priv struct ApiClient {
  api_base: String,
  http: Client,
  state: Arc<RwLock<SharedState>>,
  client_version: String,
}
```

**Impl** `impl ApiClient`:
```rust
  fn new(config: &WechatAccountConfig) -> Self
  fn url(&self, endpoint: &str) -> String
  fn random_uin_header() -> String
  async fn api_post(&self, endpoint: &str, body: &serde_json::Value) -> Result<serde_json::Value, ApiError>
  async fn api_get(&self, endpoint: &str) -> Result<serde_json::Value, ApiError>
  fn check_ret(&self, raw: &serde_json::Value) -> Result<(), ApiError>
  async fn get_updates(&self) -> Result<GetUpdatesResponse, ApiError>
  async fn send_text(&self, to_user_id: &str, text: &str, context_token: Option<&str>) -> Result<(), ApiError>
  async fn send_typing(&self, to_user_id: &str, typing: bool) -> Result<(), ApiError>
  async fn get_config(&self, ilink_user_id: &str) -> Result<GetConfigResponse, ApiError>
  async fn get_bot_qrcode(&self) -> Result<QrCodeResponse, ApiError>
  async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrStatus, ApiError>
```

**枚举** `InboundContent`:
```rust
priv enum InboundContent {
}
```

**结构体** `InboundEvent`:
```rust
priv struct InboundEvent {
  msg_id: String,
  sender_wxid: String,
  chat_id: String,
  is_group: bool,
  content: InboundContent,
  context_token: String,
  raw_timestamp: i64,
}
```

```rust
fn parse_inbound(msg: &IlinkMessage) -> InboundEvent
```

**枚举** `ErrorClass`:
```rust
priv enum ErrorClass {
}
```

```rust
fn error_class(err: &ApiError) -> ErrorClass
```

```rust
fn classify_backoff(err: &ApiError, count: u32) -> u64
```

**结构体** `WechatChannel`:
```rust
pub struct WechatChannel {
  api: ApiClient,
  config: WechatAccountConfig,
  dedup: DedupState,
}
```

**Impl** `impl WechatChannel`:
```rust
  pub fn new(config: WechatAccountConfig) -> Self
  fn build_security_policy(&self) -> crate::channels::ChannelSecurityPolicy
  async fn login(&self) -> anyhow::Result<()>
```

**Impl** `impl Channel for WechatChannel`:
```rust
  fn name(&self) -> &str { "wechat" }
  fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities
  fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy
  async fn send(&self, message: &SendMessage) -> anyhow::Result<()>
  async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>
  async fn health_check(&self) -> bool
```

```rust
fn test_client_version_encoding()
```

```rust
fn test_pkcs7_roundtrip()
```

```rust
fn test_encrypt_decrypt_roundtrip()
```

```rust
fn test_parse_text_message()
```

```rust
fn test_dedup()
```

### `channels/qqbot/`

**子模块说明**: QQ 频道适配：消息发送/接收、Markdown 渲染、Keyboard 交互、Token 管理

#### `channels/qqbot/channel.rs`

```rust
pub const GATEWAY_URL: &str = "https://api.sgroup.qq.com/gateway/bot"
```

```rust
pub const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken"
```

```rust
pub const API_BASE: &str = "https://api.sgroup.qq.com"
```

```rust
pub const INTENTS: u32 = (1 << 30) | (1 << 25) | (1 << 12) | (1 << 26)
```

```rust
pub const OP_RESUME: u32 = 6
```

```rust
pub const OP_HELLO: u32 = 10
```

```rust
pub const OP_IDENTIFY: u32 = 2
```

```rust
pub const OP_HEARTBEAT: u32 = 1
```

```rust
pub const OP_HEARTBEAT_ACK: u32 = 11
```

```rust
pub const OP_DISPATCH: u32 = 0
```

```rust
pub const OP_RECONNECT: u32 = 7
```

```rust
pub const OP_INVALID_SESSION: u32 = 9
```

```rust
pub const RECONNECT_DELAYS: &[u64] = &[1, 2, 5, 10, 30, 60]
```

```rust
pub const RAPID_RECONNECT_LIMIT: usize = 3
```

```rust
pub const RAPID_RECONNECT_WINDOW_SECS: u64 = 5
```

```rust
pub fn user_agent() -> String
```

**结构体** `QQBotChannel`:
```rust
pub struct QQBotChannel {
  pub(super) config: QQBotAccountConfig,
  pub(super) token_manager: Arc<TokenManager>,
  pub(super) dedup: DedupState,
  /// Last sequence number for heartbeat.
  pub(super) last_seq: Arc<Mutex<Option<u64>>>,
  pub(super) http_client: reqwest::Client,
  /// Active typing keep-alive tasks, keyed by recipient (e.g. "c2c:xxx").
  pub(super) typing_tasks: Arc<Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>>,
  /// WebSocket session for Resume support.
  pub(super) session: Arc<Mutex<Option<SessionState>>>,
  /// Monotonic counter for proactive message msg_seq to avoid collisions.
  pub(super) msg_seq_counter: Arc<AtomicU32>,
}
```

**Impl** `impl QQBotChannel`:
```rust
  pub fn new(config: QQBotAccountConfig) -> Self
  fn next_msg_seq(&self) -> u32
  fn build_security_policy(&self) -> crate::channels::ChannelSecurityPolicy
  async fn fetch_gateway_url(&self) -> anyhow::Result<String>
  fn build_identify(&self, token: &str) -> String
  fn handle_dispatch(
  fn apply_auth(
  fn parse_c2c_message(&self, data: &serde_json::Value) -> Option<ChannelMessage>
  fn parse_group_message(&self, data: &serde_json::Value) -> Option<ChannelMessage>
  fn build_text_body(&self, content: &str, msg_id: &str, msg_seq: u32) -> serde_json::Value
  fn build_markdown_body(&self, content: &str, msg_id: &str, msg_seq: u32) -> serde_json::Value
  async fn send_rest_with_retry(&self, url: &str, body: &serde_json::Value) -> anyhow::Result<()>
  async fn send_c2c_text(
  async fn send_group_text(
  async fn send_c2c_message(
  async fn send_group_message(
  fn start_internal_typing(&self, recipient: &str)
  fn stop_internal_typing(&self, recipient: &str)
  async fn send_c2c_keyboard(
  async fn send_group_keyboard(
  fn ack_interaction(&self, event_id: &str)
  async fn try_bot_command(
```

**Impl** `impl Channel for QQBotChannel`:
```rust
  fn name(&self) -> &str
  fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities
  fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy
  async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>
  async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>
  async fn health_check(&self) -> bool
```

**Impl** `impl QQBotChannel`:
```rust
  async fn ws_loop(&self, tx: mpsc::Sender<ChannelMessage>)
  async fn ws_connect(&self, tx: &mpsc::Sender<ChannelMessage>) -> anyhow::Result<WsDisconnect>
  async fn handle_ws_message(
```

#### `channels/qqbot/keyboard.rs`

**结构体** `ButtonPermission`:
```rust
pub struct ButtonPermission {
  pub r#type: u32,
}
```

**结构体** `ButtonAction`:
```rust
pub struct ButtonAction {
  /// 1 = Callback (INTERACTION_CREATE), 2 = Link (opens URL).
  pub r#type: u32,
  /// Payload delivered in data.resolved.button_data when type=1.
  pub data: String,
  pub permission: ButtonPermission,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub click_limit: Option<u32>,
}
```

**结构体** `ButtonRenderData`:
```rust
pub struct ButtonRenderData {
  pub label: String,
  pub visited_label: String,
  pub style: u32,
}
```

**结构体** `Button`:
```rust
pub struct Button {
  pub id: String,
  pub render_data: ButtonRenderData,
  pub action: ButtonAction,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub group_id: Option<String>,
}
```

**结构体** `ButtonRow`:
```rust
pub struct ButtonRow {
  pub buttons: Vec<Button>,
}
```

**结构体** `KeyboardContent`:
```rust
pub struct KeyboardContent {
  pub rows: Vec<ButtonRow>,
}
```

**结构体** `Keyboard`:
```rust
pub struct Keyboard {
  pub content: KeyboardContent,
}
```

**Impl** `impl Keyboard`:
```rust
  pub fn from_pairs(pairs: &[(impl AsRef<str>, impl AsRef<str>)]) -> Self
```

#### `channels/qqbot/mod.rs`

#### `channels/qqbot/token.rs`

**结构体** `TokenState`:
```rust
pub struct TokenState {
  pub access_token: String,
  /// Wall-clock expiry time. Uses `SystemTime` instead of `Instant` so that
  /// token expiry is correctly detected after system suspend (e.g. laptop
  /// sleep). NTP adjustments of a few seconds are negligible compared to the
  /// typical ~2-hour token lifetime.
  pub expires_at: std::time::SystemTime,
}
```

**结构体** `TokenManager`:
```rust
pub struct TokenManager {
  pub state: tokio::sync::RwLock<Option<TokenState>>,
  pub app_id: String,
  pub client_secret: String,
  pub http_client: reqwest::Client,
  /// Background refresh task handle.
  pub bg_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
```

**Impl** `impl TokenManager`:
```rust
  pub fn new(app_id: String, client_secret: String) -> Self
  pub async fn start_background_refresh(self: &Arc<Self>)
  async fn background_refresh_loop(&self)
  pub async fn get_token(&self) -> anyhow::Result<String>
  pub async fn refresh(&self) -> anyhow::Result<String>
  pub async fn do_refresh(&self) -> anyhow::Result<String>
  async fn fetch_new_token(&self) -> anyhow::Result<TokenState>
```

#### `channels/qqbot/types.rs`

**结构体** `SessionState`:
```rust
pub struct SessionState {
  pub session_id: String,
  pub last_seq: u64,
}
```

**枚举** `WsDisconnect`:
```rust
pub enum WsDisconnect {
  /// Normal disconnect or unknown close code — reconnect with fresh Identify.
  Clean,
  /// Should try Resume (e.g. server-initiated Reconnect opcode).
  TryResume,
  /// Fatal — do not reconnect (e.g. close codes 4914/4915).
  Fatal,
  /// Token-related — refresh token before reconnecting.
  TokenExpired,
}
```

**结构体** `GatewayPayload`:
```rust
pub struct GatewayPayload {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub op: u32,
  #[serde(default)]
  pub s: Option<u64>,
  #[serde(default)]
  pub t: Option<String>,
  #[serde(default)]
  pub d: serde_json::Value,
}
```

### `channels/telegram/`

**子模块说明**: Telegram 适配：消息发送/接收/编辑/删除、Markdown 转换

#### `channels/telegram/channel.rs`

```rust
const MAX_MESSAGE_LENGTH: usize = 4096
```

```rust
const CONTINUATION_OVERHEAD: usize = 30
```

**结构体** `DebounceEntry`:
```rust
priv struct DebounceEntry {
  sender: String,
  reply_target: String,
  contents: Vec<String>,
  images: Option<Vec<String>>,
  first_ts: u64,
  timer: tokio::task::JoinHandle<()>,
}
```

```rust
type ReactionTracker = Arc<Mutex<std::collections::HashMap<String, Vec<(i64, i64)>>>>
```

**结构体** `TelegramChannel`:
```rust
pub struct TelegramChannel {
  bot_token: String,
  /// Normalized DM whitelist. Plain `Vec<String>` (not Arc<RwLock>)
  /// because MyClaw applies config changes via `myclaw reload` → SIGUSR1
  /// → hot_switch full-process restart, not in-process mutation. New
  /// process = fresh struct = no writer ever exists in this process.
  allowed_users: Vec<String>,
  /// Phase 4: allowed group chat IDs (RFC §14.5). `None` = reject all
  /// groups (Phase 4 default); `Some(vec ["*"])` = allow all groups.
  allowed_groups: Option<Vec<String>>,
  mention_only: bool,
  api_base: String,
  dedup: DedupState,
  /// Username of this bot (fetched lazily). Wrapped in Arc for Clone.
  bot_username: Arc<Mutex<Option<String>>>,
  /// Workspace directory for saving attachments.
  workspace_dir: Option<std::path::PathBuf>,
  /// Active typing keep-alive tasks, keyed by recipient (chat_id).
  typing_tasks: Arc<Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>>,
  /// Whether to send acknowledgement reactions on received messages.
  ack_reactions: bool,
  /// Track ack reactions: reply_target → (chat_id, message_id) for removal after reply.
  pending_acks: ReactionTracker,
  /// Status reactions: reply_target → Vec<(chat_id, msg_id)>.
  status_reactions: ReactionTracker,
  /// Debounce window in milliseconds (0 = disabled).
  debounce_ms: u64,
  /// Debounce buffer: "sender|reply_target" → pending entry.
  debounce_buffer: Arc<Mutex<std::collections::HashMap<String, DebounceEntry>>>,
  /// Stall watchdog timeout in seconds (0 = disabled).
  stall_timeout_secs: u64,
  // ... 8 more fields
}
```

**Impl** `impl TelegramChannel`:
```rust
  pub fn new(config: TelegramAccountConfig) -> Self
  fn api_url(&self, method: &str) -> String
  fn http_client(&self) -> &reqwest::Client
  fn offset_path(&self) -> std::path::PathBuf
  fn load_offset(&self) -> i64
  fn persist_offset(&self, offset: i64)
  fn normalize_identity(value: &str) -> String
  fn normalize_allowed_users(users: Vec<String>) -> Vec<String>
  fn try_authorize(
  async fn fetch_bot_username(&self) -> Option<String>
  fn get_bot_username(&self) -> Option<String>
  fn set_bot_username(&self, username: String)
  fn find_bot_mention_spans(&self, text: &str) -> Vec<(usize, usize)>
  fn strip_bot_mentions(&self, text: &str) -> String
  fn contains_bot_mention(&self, text: &str) -> bool
  fn is_reply_to_bot(&self, msg: &Message) -> bool
  fn is_group_message(chat: &Chat) -> bool
  fn format_forward_attribution(msg: &Message) -> Option<String>
  fn parse_reply_target(reply_target: &str) -> (String, Option<String>)
  async fn send_raw(
  async fn delete_message_raw(&self, chat_id: i64, message_id: i64) -> anyhow::Result<()>
  async fn edit_message_text_raw(&self, chat_id: i64, message_id: i64, text: &str) -> anyhow::Result<bool>
  async fn send_chat_action(
  fn parse_message_content(&self, msg: &Message) -> String
  async fn download_file_base64(&self, file_id: &str) -> anyhow::Result<String>
  async fn ack_message(&self, chat_id: i64, message_id: i64)
  async fn remove_ack(&self, chat_id: i64, message_id: i64)
  async fn set_reaction(&self, chat_id: i64, message_id: i64, emoji: &str) -> anyhow::Result<()>
  async fn remove_reaction(&self, chat_id: i64, message_id: i64, _emoji: &str) -> anyhow::Result<()>
  async fn answer_callback_query(&self, callback_query_id: &str)
  fn start_internal_typing(&self, recipient: &str)
  fn stop_internal_typing(&self, recipient: &str)
```

**Impl** `impl TelegramChannel`:
```rust
  async fn debounce_send(&self, msg: ChannelMessage, tx: mpsc::Sender<ChannelMessage>)
  async fn stall_watchdog(&self)
  async fn poll_loop(&self, tx: mpsc::Sender<ChannelMessage>)
  fn chunk_for_telegram(content: &str) -> Vec<String>
```

**Impl** `impl Channel for TelegramChannel`:
```rust
  fn name(&self) -> &str
  fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities
  fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy
  async fn send(&self, message: &SendMessage) -> anyhow::Result<()>
  async fn edit_message(
  async fn delete_message(
  async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>
  async fn health_check(&self) -> bool
  async fn on_status(&self, recipient: &str, status: ProcessingStatus)
```

```rust
fn make_config() -> TelegramAccountConfig
```

```rust
fn test_normalize_identity()
```

```rust
fn test_normalize_allowed_users()
```

```rust
fn phase4_security_policy_default_rejects_groups()
```

```rust
fn phase4_check_authorization_dm_allows_listed_user()
```

```rust
fn phase4_group_mention_only_with_allowlist()
```

```rust
fn test_parse_reply_target()
```

```rust
fn test_message_thread_id_in_reply_target()
```

```rust
fn test_forward_attribution_user()
```

```rust
fn test_forward_attribution_channel()
```

```rust
fn test_forward_attribution_hidden_sender()
```

```rust
fn test_forward_attribution_none()
```

```rust
fn test_bot_mention_spans()
```

```rust
fn test_dedup()
```

```rust
fn test_message_chunking()
```

```rust
fn test_utf16_chunking_emoji()
```

```rust
fn test_md_bold()
```

```rust
fn test_md_italic_asterisk()
```

```rust
fn test_md_italic_underscore()
```

```rust
fn test_md_strikethrough()
```

```rust
fn test_md_inline_code()
```

```rust
fn test_md_code_block_plain()
```

```rust
fn test_md_code_block_with_lang()
```

```rust
fn test_md_link()
```

```rust
fn test_md_heading()
```

```rust
fn test_md_blockquote()
```

```rust
fn test_md_horizontal_rule()
```

```rust
fn test_md_html_escape_in_plain_text()
```

```rust
fn test_md_no_formatting()
```

```rust
fn test_md_mixed_formatting()
```

```rust
fn test_md_formatting_not_inside_code_block()
```

```rust
fn test_md_formatting_not_inside_inline_code()
```

```rust
fn test_md_unclosed_bold_closed_at_end()
```

```rust
fn test_md_multiline_heading()
```

```rust
fn test_md_complex_message()
```

#### `channels/telegram/markdown.rs`

```rust
pub fn escape_html(text: &str) -> String
```

```rust
pub fn markdown_to_telegram_html(markdown: &str) -> String
```

#### `channels/telegram/mod.rs`

#### `channels/telegram/types.rs`

**结构体** `Update`:
```rust
pub struct Update {
  #[serde(default)]
  pub update_id: i64,
  #[serde(default)]
  pub message: Option<Message>,
  #[serde(default)]
  pub edited_message: Option<Message>,
  #[serde(default)]
  pub callback_query: Option<CallbackQuery>,
}
```

**结构体** `PhotoSize`:
```rust
pub struct PhotoSize {
  #[serde(default)]
  pub file_id: String,
  #[serde(default)]
  pub file_unique_id: String,
  #[serde(default)]
  pub width: i32,
  #[serde(default)]
  pub height: i32,
  #[serde(default)]
  pub file_size: Option<i64>,
}
```

**结构体** `Message`:
```rust
pub struct Message {
  #[serde(default)]
  pub message_id: i64,
  #[serde(default)]
  pub message_thread_id: Option<i64>,
  #[serde(default)]
  pub from: Option<User>,
  #[serde(default)]
  pub chat: Chat,
  #[serde(default)]
  pub text: Option<String>,
  #[serde(default)]
  pub caption: Option<String>,
  #[serde(default)]
  pub photo: Option<Vec<PhotoSize>>,
  #[serde(default)]
  pub forward_from: Option<User>,
  #[serde(default)]
  pub forward_from_chat: Option<Chat>,
  #[serde(default)]
  pub forward_sender_name: Option<String>,
  #[serde(default)]
  pub forward_date: Option<i64>,
  #[serde(default)]
  pub reply_to_message: Option<Box<Message>>,
}
```

**结构体** `User`:
```rust
pub struct User {
  #[serde(default)]
  pub id: i64,
  #[serde(default)]
  pub username: Option<String>,
  #[serde(default)]
  pub first_name: Option<String>,
}
```

**结构体** `Chat`:
```rust
pub struct Chat {
  #[serde(default)]
  pub id: i64,
  #[serde(rename = "type", default)]
  pub kind: String,
  #[serde(default)]
  pub username: Option<String>,
  #[serde(default)]
  pub title: Option<String>,
}
```

**结构体** `CallbackQuery`:
```rust
pub struct CallbackQuery {
  #[serde(default)]
  pub id: String,
  #[serde(default)]
  pub from: Option<User>,
  #[serde(default)]
  pub message: Option<Message>,
  #[serde(default)]
  pub data: Option<String>,
}
```

**结构体** `SendMessageRequest`:
```rust
pub struct SendMessageRequest {
  #[serde(rename = "chat_id")]
  pub chat_id: String,
  #[serde(rename = "message_thread_id", skip_serializing_if = "Option::is_none")]
  pub message_thread_id: Option<String>,
  #[serde(default)]
  pub text: String,
  #[serde(rename = "parse_mode", skip_serializing_if = "Option::is_none")]
  pub parse_mode: Option<String>,
  #[serde(rename = "reply_markup", skip_serializing_if = "Option::is_none")]
  pub reply_markup: Option<serde_json::Value>,
}
```

**结构体** `SendChatActionRequest`:
```rust
pub struct SendChatActionRequest {
  #[serde(rename = "chat_id")]
  pub chat_id: String,
  #[serde(rename = "message_thread_id", skip_serializing_if = "Option::is_none")]
  pub message_thread_id: Option<String>,
  #[serde(rename = "action")]
  pub action: String,
}
```

**结构体** `GetUpdatesResponse`:
```rust
pub struct GetUpdatesResponse {
  #[serde(default)]
  pub ok: bool,
  #[serde(default)]
  pub result: Vec<Update>,
}
```

---

## `cli/`

**模块说明**: 命令行界面：myclaw chat/status/reload/restart/stop/update/config/doctor/tools/tui/completion/exec 子命令

#### `cli/cmd_chat.rs`

```rust
pub async fn run(cli: &Cli, prompt: Option<&str>, agent: Option<&str>, model: Option<&str>, print: bool) -> Result<()>
```

#### `cli/cmd_completion.rs`

```rust
pub fn run(shell: Shell) -> Result<()>
```

#### `cli/cmd_config.rs`

**枚举** `ConfigAction`:
```rust
pub enum ConfigAction {
  /// Show the full resolved configuration.
  Show,
  /// Get a specific config value by dotted path (e.g. "routing.chat.models").
  Get {
  /// Dotted config path.
  path: String,
  },
  /// Set a config value (writes to the config file).
  Set {
  /// Dotted config path.
  path: String,
  /// Value to set.
  value: String,
  },
  /// Initialize a new config file with defaults.
  Init {
  /// Output path (default: ~/.myclaw/myclaw.toml).
  #[arg(short, long)]
  output: Option<String>,
  },
}
```

```rust
pub async fn run(cli: &Cli, action: ConfigAction) -> Result<()>
```

```rust
fn generate_default_config() -> String
```

#### `cli/cmd_doctor.rs`

```rust
pub async fn run(_cli: &Cli, fix: bool) -> Result<()>
```

```rust
fn find_config() -> Option<PathBuf>
```

```rust
fn try_load_config() -> Option<myclaw::config::AppConfig>
```

#### `cli/cmd_exec.rs`

```rust
pub async fn run(cli: &Cli, prompt: &str, agent: Option<&str>, model: Option<&str>, format: &str) -> Result<()>
```

#### `cli/cmd_reload.rs`

```rust
pub async fn run(_cli: &Cli) -> Result<()>
```

#### `cli/cmd_restart.rs`

```rust
pub async fn run(_cli: &Cli) -> Result<()>
```

#### `cli/cmd_status.rs`

```rust
fn default_model(cfg: &myclaw::config::AppConfig) -> Option<&str>
```

```rust
pub async fn run(cli: &Cli, format: &str) -> Result<()>
```

```rust
fn print_text_status(cfg: &Option<myclaw::config::AppConfig>)
```

```rust
fn print_json_status(cfg: &Option<myclaw::config::AppConfig>) -> Result<()>
```

```rust
fn read_uptime(pid: i32) -> Option<(u64, u64)>
```

#### `cli/cmd_stop.rs`

```rust
pub async fn run(_cli: &Cli) -> Result<()>
```

#### `cli/cmd_tools.rs`

**枚举** `ToolsCommand`:
```rust
pub enum ToolsCommand {
  /// List all built-in tools.
  List {
  /// Output format: text (default) or json.
  #[arg(long, default_value = "text")]
  format: String,
  },
  /// List configured sub-agents.
  Agents {
  /// Output format: text (default) or json.
  #[arg(long, default_value = "text")]
  format: String,
  },
  /// List configured MCP servers.
  Mcp {
  /// Output format: text (default) or json.
  #[arg(long, default_value = "text")]
  format: String,
  },
}
```

```rust
pub async fn run(cli: &Cli, cmd: ToolsCommand) -> Result<()>
```

```rust
fn list_tools(format: &str) -> Result<()>
```

```rust
fn list_agents(cli: &Cli, format: &str) -> Result<()>
```

```rust
fn list_mcp(cli: &Cli, format: &str) -> Result<()>
```

#### `cli/cmd_tui.rs`

```rust
const DEFAULT_WS_URL: &str = "ws://127.0.0.1:18789/myclaw"
```

```rust
pub async fn run(url: Option<&str>) -> Result<()>
```

#### `cli/cmd_update.rs`

```rust
pub fn run_update() -> Result<()>
```

```rust
fn get_latest_run_id() -> Result<String>
```

```rust
fn download_artifact(run_id: &str, dest: &Path) -> Result<()>
```

```rust
fn find_project_dir() -> Result<PathBuf>
```

```rust
fn send_sigusr1() -> Result<()>
```

#### `cli/mod.rs`

**结构体** `Cli`:
```rust
pub struct Cli {
  /// Path to config file.
  #[arg(short, long, global = true)]
  pub config: Option<String>,
  /// Log level (trace, debug, info, warn, error).
  #[arg(long, global = true)]
  pub log_level: Option<String>,
  #[command(subcommand)]
  pub command: Option<Commands>,
}
```

**枚举** `Commands`:
```rust
pub enum Commands {
  /// Start the MyClaw daemon (starts all configured channels and agents).
  Run {
  /// Path to config file (default: search in ~/.myclaw/, /etc/myclaw/).
  #[arg(short, long)]
  config: Option<String>,
  },
  /// Start an interactive chat session (TUI / REPL).
  Chat {
  /// Initial prompt to send immediately.
  prompt: Option<String>,
  /// Agent name to use (from config [[agents]]).
  #[arg(short, long)]
  agent: Option<String>,
  /// Model override.
  #[arg(short, long)]
  model: Option<String>,
  /// Non-interactive mode: print response and exit.
  #[arg(short, long)]
  print: bool,
  },
  // ... 12 more variants
}
```

```rust
const DEFAULT_CONFIG_PATHS: &[&str] = &[
```

```rust
pub fn load_config(cli: &Cli) -> Result<myclaw::config::AppConfig>
```

```rust
pub fn load_config_opt(cli: &Cli) -> Option<myclaw::config::AppConfig>
```

```rust
fn resolve_config_path(cli: &Cli) -> Option<std::path::PathBuf>
```

```rust
pub fn init_tracing(cfg: &myclaw::config::AppConfig)
```

#### `cli/signal.rs`

---

## `config/`

**模块说明**: 配置系统：TOML 配置解析、Agent/Channel/Provider/Routing/Scheduler/MCP/SubAgent 配置结构

**外部模块依赖**: `agents`, `providers`

#### `config/agent.rs`

**枚举** `PermissionMode`:
```rust
pub enum PermissionMode {
  /// All tools allowed, no approval needed.
  Full,
  /// Default: safe tools auto-approved, dangerous tools need approval.
  #[default]
  Default,
  /// Only read-only tools allowed.
  ReadOnly,
}
```

**枚举** `RunMode`:
```rust
pub enum RunMode {
  /// Interactive session — a user or supervisor is present.
  #[default]
  Interactive,
  /// Autonomous background task (Cron, Webhook) — no active user.
  Background,
}
```

**结构体** `ContextConfig`:
```rust
pub struct ContextConfig {
  /// Compact threshold: trigger compaction when token usage exceeds
  /// this fraction of context_window. Default: 0.7
  #[serde(default = "default_compact_threshold")]
  pub compact_threshold: f64,
  /// Number of recent complete work units to retain during compaction.
  #[serde(default = "default_retain_work_units")]
  pub retain_work_units: usize,
}
```

```rust
fn default_compact_threshold() -> f64 { 0.7 }
```

```rust
fn default_retain_work_units() -> usize { 2 }
```

**Impl** `impl Default for ContextConfig`:
```rust
  fn default() -> Self
```

**结构体** `ToolExecutorConfig`:
```rust
pub struct ToolExecutorConfig {
  /// Tool call timeout in seconds.
  #[serde(default = "default_tool_timeout")]
  pub timeout_secs: u64,
}
```

```rust
fn default_tool_timeout() -> u64 { 180 }
```

**Impl** `impl Default for ToolExecutorConfig`:
```rust
  fn default() -> Self
```

**结构体** `AgentConfig`:
```rust
pub struct AgentConfig {
  /// Permission mode — controls tool approval requirements.
  #[serde(default)]
  pub permission_mode: PermissionMode,
}
```

**结构体** `PromptConfig`:
```rust
pub struct PromptConfig {
  /// Maximum system prompt length in characters. 0 = unlimited.
  #[serde(default)]
  pub max_chars: usize,
  /// Use native tool calling (vs XML protocol).
  #[serde(default = "default_true")]
  pub native_tools: bool,
  /// IANA timezone name (e.g. "Asia/Shanghai").
  /// Takes precedence over `timezone_offset` when set.
  #[serde(default)]
  pub timezone: Option<String>,
  /// Timezone offset in hours (e.g. 8 for UTC+8).
  /// Legacy fallback — prefer `timezone` for DST-aware scheduling.
  #[serde(default = "default_timezone_offset")]
  pub timezone_offset: i32,
}
```

```rust
fn default_timezone_offset() -> i32 { 8 }
```

```rust
fn default_true() -> bool { true }
```

**Impl** `impl Default for PromptConfig`:
```rust
  fn default() -> Self
```

```rust
fn default_agent_config()
```

```rust
fn default_subsystem_configs()
```

#### `config/channel.rs`

**结构体** `WechatAccountConfig`:
```rust
pub struct WechatAccountConfig {
  /// iLink Bot API base URL.
  pub api_base: String,
  /// Bot token (if pre-authenticated; supports `${ENV_VAR}` expansion).
  pub bot_token: Option<String>,
  /// AES key for message encryption (supports `${ENV_VAR}` expansion).
  pub aes_key: Option<String>,
  /// Long-poll timeout in seconds.
  #[serde(default = "default_poll_timeout")]
  pub poll_timeout: u64,
  /// Allowed WeChat user IDs (`["*"]` = all).
  #[serde(default)]
  pub allowed_users: Vec<String>,
  /// Whether this account is enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
}
```

```rust
fn default_poll_timeout() -> u64
```

```rust
fn default_true() -> bool
```

**Impl** `impl Default for WechatAccountConfig`:
```rust
  fn default() -> Self
```

**结构体** `WechatChannelConfig`:
```rust
pub struct WechatChannelConfig {
  /// Whether this channel type is enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
  /// Account instances keyed by account ID.
  #[serde(default)]
  pub accounts: HashMap<String, WechatAccountConfig>,
}
```

**结构体** `TelegramAccountConfig`:
```rust
pub struct TelegramAccountConfig {
  /// Telegram Bot API token (supports `${ENV_VAR}` expansion).
  pub bot_token: String,
  /// Allowed Telegram usernames or user IDs for DM (`["*"]` = all).
  #[serde(default)]
  pub allowed_users: Vec<String>,
  /// Allowed Telegram group IDs (RFC §14).
  /// `None` = reject all groups (Phase 4 default); `["*"]` = accept all
  /// groups; `["g1", "g2"]` = whitelist by chat ID.
  #[serde(default)]
  pub allowed_groups: Option<Vec<String>>,
  /// Only respond when @mentioned in (allowed) groups.
  #[serde(default)]
  pub mention_only: bool,
  /// Override the Telegram Bot API base URL (for local Bot API servers).
  pub api_base: Option<String>,
  /// Per-account proxy URL.
  pub proxy_url: Option<String>,
  /// Whether this account is enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
  /// Approval prompt timeout in seconds.
  #[serde(default = "default_approval_timeout")]
  pub approval_timeout_secs: u64,
  /// Send acknowledgement reactions on received messages.
  #[serde(default = "default_true")]
  pub ack_reactions: bool,
  /// Workspace directory for saving downloaded attachments.
  pub workspace_dir: Option<String>,
  /// Stall watchdog threshold in seconds. Send "still thinking" message if typing exceeds this.
  /// Set to 0 to disable.
  // ... 2 more fields
}
```

```rust
fn default_debounce_ms() -> u64
```

```rust
fn default_stall_timeout_secs() -> u64
```

```rust
fn default_approval_timeout() -> u64
```

**结构体** `TelegramChannelConfig`:
```rust
pub struct TelegramChannelConfig {
  /// Whether this channel type is enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
  /// Account instances keyed by account ID.
  #[serde(default)]
  pub accounts: HashMap<String, TelegramAccountConfig>,
}
```

**结构体** `QQBotAccountConfig`:
```rust
pub struct QQBotAccountConfig {
  /// Whether this account is enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
  /// QQ Bot App ID.
  pub app_id: String,
  /// QQ Bot Client Secret.
  pub client_secret: String,
  /// Allowed user OpenIDs for private chat (RFC §14).
  /// `None` = allow all; `[]` = reject all; `["*"]` = allow all.
  /// Replaces the legacy `allow_from` field (Phase 4 rename).
  #[serde(default, alias = "allow_from")]
  pub allowed_users: Option<Vec<String>>,
  /// Allowed group OpenIDs (RFC §14).
  /// `None` = reject all groups (Phase 4 default); `["*"]` = accept all
  /// groups; `["g1", "g2"]` = whitelist.
  /// Replaces the legacy `group_allow_from` field (Phase 4 rename).
  #[serde(default, alias = "group_allow_from")]
  pub allowed_groups: Option<Vec<String>>,
}
```

**结构体** `QQBotChannelConfig`:
```rust
pub struct QQBotChannelConfig {
  /// Whether this channel type is enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
  /// Account instances keyed by account ID.
  #[serde(default)]
  pub accounts: HashMap<String, QQBotAccountConfig>,
}
```

**结构体** `ChannelConfigs`:
```rust
pub struct ChannelConfigs {
  pub wechat: Option<WechatChannelConfig>,
  pub telegram: Option<TelegramChannelConfig>,
  pub qqbot: Option<QQBotChannelConfig>,
  pub client: Option<ClientConfig>,
}
```

**Impl** `impl ChannelConfigs`:
```rust
  pub fn enabled_channels(&self) -> Vec<&str>
```

**结构体** `ClientConfig`:
```rust
pub struct ClientConfig {
  /// Whether this channel is enabled.
  #[serde(default)]
  pub enabled: bool,
  /// Bind address for the WebSocket server.
  #[serde(default = "default_client_bind")]
  pub bind: String,
  /// Maximum concurrent WebSocket connections.
  #[serde(default = "default_max_connections")]
  pub max_connections: u32,
  /// Authentication token (Bearer). None = no auth required.
  pub auth_token: Option<String>,
}
```

```rust
fn default_client_bind() -> String
```

```rust
fn default_max_connections() -> u32
```

**Impl** `impl Default for ClientConfig`:
```rust
  fn default() -> Self
```

```rust
fn deserialize_wechat_account_config()
```

```rust
fn deserialize_telegram_account_config()
```

```rust
fn deserialize_multi_account_config()
```

```rust
fn channel_configs_enabled_list()
```

#### `config/filters.rs`

**枚举** `NameFilter`:
```rust
pub enum NameFilter {
  /// `tools: [all]` or omitted entirely → all allowed.
  AllKeyword(AllKeyword),
  /// `tools: [shell, file_read]` → allow-list.
  Allow(Vec<String>),
  /// `tools: { except: [destructive_op] }` → deny-list.
  Deny(DenyList),
}
```

**结构体** `AllKeyword`:
```rust
pub struct AllKeyword {
}
```

**结构体** `DenyList`:
```rust
pub struct DenyList {
  pub except: Vec<String>,
}
```

**Impl** `impl NameFilter`:
```rust
  pub fn all() -> Self
  pub fn allows(&self, name: &str) -> bool
  pub fn is_all(&self) -> bool
```

**Impl** `impl Default for NameFilter`:
```rust
  fn default() -> Self
```

```rust
pub type ToolFilter = NameFilter
```

```rust
pub type SkillFilter = NameFilter
```

```rust
pub type McpFilter = NameFilter
```

```rust
fn all_matches_anything()
```

```rust
fn allow_list_matches_only_listed()
```

```rust
fn deny_list_matches_everything_not_listed()
```

#### `config/mcp.rs`

**枚举** `McpTransport`:
```rust
pub enum McpTransport {
  /// Spawn a local process and communicate over stdin/stdout.
  #[default]
  Stdio,
  /// HTTP POST transport.
  Http,
  /// Server-Sent Events transport.
  Sse,
}
```

**结构体** `McpServerConfig`:
```rust
pub struct McpServerConfig {
  /// Display name for the server (used for tool prefixing).
  #[serde(default)]
  pub name: String,
  /// Command to spawn (for Stdio transport).
  #[serde(default)]
  pub command: String,
  /// Arguments for the command.
  #[serde(default)]
  pub args: Vec<String>,
  /// Environment variables to set.
  #[serde(default)]
  pub env: HashMap<String, String>,
  /// Per-server tool call timeout (seconds).
  pub tool_timeout_secs: Option<u64>,
  /// Transport protocol.
  #[serde(default)]
  pub transport: McpTransport,
  /// URL for HTTP/SSE transports.
  pub url: Option<String>,
  /// Additional headers for HTTP/SSE transports.
  #[serde(default)]
  pub headers: HashMap<String, String>,
}
```

```rust
fn deserialize_mcp_stdio()
```

```rust
fn deserialize_mcp_http()
```

#### `config/mod.rs`

**结构体** `RawConfig`:
```rust
priv struct RawConfig {
  /// Workspace directory path.
  #[serde(default)]
  workspace_dir: Option<String>,
  /// 知识库目录路径。默认为 {workspace_dir}/memory。
  #[serde(default)]
  knowledge_dir: Option<String>,
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
  // ... 3 more fields
}
```

**结构体** `LoggingConfig`:
```rust
pub struct LoggingConfig {
  /// Default log level.
  #[serde(default)]
  pub level: Option<String>,
  /// Per-module log levels.
  #[serde(default)]
  pub modules: HashMap<String, String>,
}
```

**结构体** `AppConfig`:
```rust
pub struct AppConfig {
  /// Workspace directory (absolute path).
  pub workspace_dir: PathBuf,
  /// Knowledge directory (absolute path). Defaults to {workspace_dir}/memory.
  pub knowledge_dir: PathBuf,
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
  /// MCP server configurations.
  pub mcp_servers: Vec<McpServerConfig>,
  /// Logging configuration.
  pub logging: LoggingConfig,
}
```

**结构体** `ConfigLoader`:
```rust
pub struct ConfigLoader {
}
```

**Impl** `impl ConfigLoader`:
```rust
  pub fn from_file(path: impl AsRef<Path>) -> Result<AppConfig>
  pub fn from_toml(content: &str) -> Result<AppConfig>
  fn resolve(mut raw: RawConfig, config_path: PathBuf) -> Result<AppConfig>
  fn expand_env_vars(config: &mut RawConfig)
  fn expand_string(s: &str) -> String
  fn expand_path(path: &str) -> PathBuf
```

```rust
fn parse_minimal_config()
```

```rust
fn parse_empty_config()
```

```rust
fn parse_full_config()
```

```rust
fn env_var_expansion()
```

#### `config/provider.rs`

**枚举** `Protocol`:
```rust
pub enum Protocol {
  /// OpenAI Chat Completions protocol (or OpenAI-compatible).
  #[default]
  OpenAi,
  /// Anthropic Messages protocol (or Anthropic-compatible).
  Anthropic,
}
```

**枚举** `AuthStyle`:
```rust
pub enum AuthStyle {
  /// Bearer token in Authorization header.
  #[default]
  Bearer,
  /// x-api-key header (Anthropic style).
  XApiKey,
}
```

**Impl** `impl From<AuthStyle> for crate::providers::AuthStyle`:
```rust
  fn from(style: AuthStyle) -> Self
```

**结构体** `ChatSection`:
```rust
pub struct ChatSection {
  pub base_url: String,
  pub user_agent: Option<String>,
  pub api_key: Option<String>,
  pub auth_style: Option<AuthStyle>,
  /// Explicit protocol override. If not set, inferred from base_url host.
  pub protocol: Option<Protocol>,
  pub models: HashMap<String, ChatModelConfig>,
}
```

**结构体** `EmbeddingSection`:
```rust
pub struct EmbeddingSection {
  pub base_url: String,
  pub user_agent: Option<String>,
  pub api_key: Option<String>,
  pub auth_style: Option<AuthStyle>,
  /// Explicit protocol override. If not set, inferred from base_url host.
  pub protocol: Option<Protocol>,
  pub models: HashMap<String, EmbeddingModelConfig>,
}
```

**结构体** `CapabilitySection`:
```rust
pub struct CapabilitySection {
  pub base_url: String,
  pub user_agent: Option<String>,
  pub api_key: Option<String>,
  pub auth_style: Option<AuthStyle>,
  /// Explicit protocol override. If not set, inferred from base_url host.
  pub protocol: Option<Protocol>,
  pub models: HashMap<String, BasicModelConfig>,
}
```

**结构体** `ProviderConfig`:
```rust
pub struct ProviderConfig {
  /// Provider identity override (e.g. "glm", "anthropic").
  /// If not set, inferred from the capability base_url host.
  #[serde(default)]
  pub provider: Option<String>,
  /// Provider-level API key (supports `${ENV_VAR}` expansion).
  /// Capability-level api_key takes precedence when set.
  pub api_key: Option<String>,
  /// Multiple API keys for rotation (comma-separated or array).
  #[serde(default)]
  pub api_keys: Vec<String>,
  /// Credential rotation strategy when multiple keys are available.
  #[serde(default)]
  pub rotation_strategy: RotationStrategy,
  /// Default authentication style.
  #[serde(default)]
  pub auth_style: AuthStyle,
  /// Chat capability section.
  #[serde(default)]
  pub chat: Option<ChatSection>,
  /// Embedding capability section.
  #[serde(default)]
  pub embedding: Option<EmbeddingSection>,
  /// Image generation capability section.
  #[serde(default)]
  pub image_generation: Option<CapabilitySection>,
  /// Text-to-speech capability section.
  #[serde(default)]
  pub tts: Option<CapabilitySection>,
  /// Speech-to-text capability section.
  #[serde(default)]
  // ... 3 more fields
}
```

**Impl** `impl ProviderConfig`:
```rust
  pub fn effective_api_key(&self, capability_api_key: Option<&str>) -> Option<String>
  pub fn effective_api_keys(&self, capability_api_key: Option<&str>) -> Vec<String>
  pub fn effective_auth_style(&self, capability_auth: Option<AuthStyle>) -> AuthStyle
```

```rust
fn deserialize_new_provider_config()
```

```rust
fn effective_api_key_fallback()
```

```rust
fn effective_api_key_capability_override()
```

#### `config/routing.rs`

**枚举** `RoutingStrategy`:
```rust
pub enum RoutingStrategy {
  /// Use the first available model.
  #[default]
  Fixed,
  /// Try models in order; fall back on failure.
  Fallback,
  /// Pick the cheapest model (future).
  Cheapest,
  /// Pick the fastest model (future).
  Fastest,
}
```

**结构体** `RouteEntry`:
```rust
pub struct RouteEntry {
  /// Selection strategy.
  #[serde(default)]
  pub strategy: RoutingStrategy,
  /// Candidate model IDs (looked up across all providers).
  #[serde(default)]
  pub models: Vec<String>,
  /// Candidate provider names (for capabilities that route by provider).
  #[serde(default)]
  pub providers: Vec<String>,
}
```

**结构体** `RoutingConfig`:
```rust
pub struct RoutingConfig {
}
```

**Impl** `impl RoutingConfig`:
```rust
  pub fn get(&self, cap: Capability) -> Option<&RouteEntry>
  pub fn insert(&mut self, cap: Capability, entry: RouteEntry)
  pub fn iter(&self) -> impl Iterator<Item = (&str, &RouteEntry)>
  pub fn len(&self) -> usize
  pub fn is_empty(&self) -> bool
```

```rust
fn deserialize_routing()
```

#### `config/scheduler.rs`

```rust
fn default_every() -> String { "30m".to_string() }
```

```rust
fn default_target() -> String { "last".to_string() }
```

```rust
fn default_webhook_port() -> u16 { 18789 }
```

**结构体** `HeartbeatConfig`:
```rust
pub struct HeartbeatConfig {
  /// Enable periodic heartbeat checks.
  #[serde(default)]
  pub enabled: bool,
  /// Interval string: "5m", "30m", "1h". "0" disables.
  #[serde(default = "default_every")]
  pub every: String,
  /// Where to send heartbeat output: "last" | "none" | channel name.
  #[serde(default = "default_target")]
  pub target: String,
  /// Active hours, e.g. "08:00-24:00". None = always active.
  #[serde(default)]
  pub active_hours: Option<String>,
  /// Custom heartbeat prompt. None = default prompt.
  #[serde(default)]
  pub prompt: Option<String>,
}
```

**Impl** `impl Default for HeartbeatConfig`:
```rust
  fn default() -> Self
```

**结构体** `CronJob`:
```rust
pub struct CronJob {
  /// Cron expression (5-field: min hour day month weekday).
  /// e.g. "0 9 * * *" = every day at 9:00.
  pub schedule: String,
  /// Prompt to send to the agent when triggered.
  pub prompt: String,
  /// Where to send output: "last" | "none" | channel name.
  #[serde(default = "default_target")]
  pub target: String,
  /// Active hours restriction, e.g. "08:00-24:00". None = always active.
  #[serde(default)]
  pub active_hours: Option<String>,
}
```

**结构体** `CronConfig`:
```rust
pub struct CronConfig {
  /// Enable cron scheduler.
  #[serde(default)]
  pub enabled: bool,
}
```

**结构体** `WebhookConfig`:
```rust
pub struct WebhookConfig {
  /// Enable webhook HTTP server.
  #[serde(default)]
  pub enabled: bool,
  /// Port to listen on.
  #[serde(default = "default_webhook_port")]
  pub port: u16,
  /// Default secret for built-in endpoints (`/hooks/agent`, `/hooks/wake`).
  /// Individual webhook files can override with their own secret.
  #[serde(default)]
  pub secret: Option<String>,
}
```

**Impl** `impl Default for WebhookConfig`:
```rust
  fn default() -> Self
```

**结构体** `SchedulerConfig`:
```rust
pub struct SchedulerConfig {
  #[serde(default)]
  pub heartbeat: HeartbeatConfig,
  #[serde(default)]
  pub cron: CronConfig,
  #[serde(default)]
  pub webhook: WebhookConfig,
}
```

#### `config/sub_agent.rs`

**枚举** `AgentIsolation`:
```rust
pub enum AgentIsolation {
  /// Share workspace_dir with the main agent — no extra isolation.
  Shared,
  /// Use a git worktree for the sub-agent — isolated working directory.
  Worktree,
}
```

**Impl** `impl Default for AgentIsolation`:
```rust
  fn default() -> Self
```

**结构体** `SubAgentConfig`:
```rust
pub struct SubAgentConfig {
  /// Unique name for this sub-agent (used in agent_delegate tool call).
  pub name: String,
  /// System prompt for this sub-agent (body of AGENT.md).
  pub system_prompt: String,
  /// Tools this sub-agent is allowed to use. RFC v2 §三.A:
  /// `ToolFilter` (`[all]` / explicit allow-list /
  /// `{ except: [...] }` deny-list). Default = `[all]`.
  #[serde(default)]
  pub tools: crate::config::filters::ToolFilter,
  /// Skills this sub-agent may see in system reminders / load via skill_view.
  /// RFC v2 §三.B: NameFilter form `[all]` / `[skill_a, skill_b]` /
  /// `{ except: [...] }`. Default `all`.
  #[serde(default)]
  pub skills: crate::config::filters::SkillFilter,
  /// MCP server names whose tools are exposed to this sub-agent.
  /// Default `all`. Per-tool filtering still applies via `tools`.
  #[serde(default)]
  pub mcp: crate::config::filters::McpFilter,
  /// Hard cap on tool calls per delegation. Defaults to the parent agent's limit.
  #[serde(default)]
  pub max_tool_calls: Option<usize>,
  /// Optional description shown to the router agent in the agent_delegate tool.
  #[serde(default)]
  pub description: Option<String>,
  /// Optional model override — use a specific model instead of the default chat provider.
  /// Useful for routing summarization to cheaper models.
  #[serde(default)]
  pub model: Option<String>,
  /// File system isolation level. Defaults to "shared".
  #[serde(default)]
  // ... 1 more fields
}
```

**Impl** `impl SubAgentConfig`:
```rust
  pub fn allows_tool(&self, tool_name: &str) -> bool
  pub fn allows_skill(&self, skill_name: &str) -> bool
  pub fn allows_mcp(&self, server_name: &str) -> bool
```

**Impl** `impl SubAgentConfig`:
```rust
  pub fn description(&self) -> &str
```

```rust
fn deserialize_sub_agent()
```

```rust
fn deserialize_with_isolation()
```

```rust
fn default_isolation_is_shared()
```

```rust
fn allows_skill_default_is_all()
```

```rust
fn allows_tool_whitelist()
```

---

## `mcp/`

**模块说明**: MCP 协议客户端：Model Context Protocol 的 HTTP/SSE/STDIO 传输、工具发现与调用

**外部模块依赖**: `client`, `config_types`, `protocol`, `tool`, `tool_trait`, `transport`

#### `mcp/client.rs`

```rust
const RECV_TIMEOUT_SECS: u64 = 30
```

```rust
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 180
```

```rust
const MAX_TOOL_TIMEOUT_SECS: u64 = 600
```

**结构体** `McpServerInner`:
```rust
priv struct McpServerInner {
  config: McpServerConfig,
  transport: Box<dyn McpTransportConn>,
  #[cfg(target_has_atomic = "64")]
  next_id: AtomicU64,
  #[cfg(not(target_has_atomic = "64"))]
  next_id: AtomicU32,
  tools: Vec<McpToolDef>,
  /// Server instructions from initialize response.
  instructions: String,
}
```

**结构体** `McpServer`:
```rust
pub struct McpServer {
  inner: Arc<Mutex<McpServerInner>>,
}
```

**Impl** `impl McpServer`:
```rust
  pub async fn connect(config: McpServerConfig) -> Result<Self>
  pub async fn tools(&self) -> Vec<McpToolDef>
  pub async fn name(&self) -> String
  pub async fn instructions(&self) -> String
  pub async fn call_tool(
```

**结构体** `McpRegistry`:
```rust
pub struct McpRegistry {
  servers: Vec<McpServer>,
  /// prefixed_name → (server_index, original_tool_name)
  tool_index: HashMap<String, (usize, String)>,
}
```

**Impl** `impl McpRegistry`:
```rust
  pub async fn connect_all(configs: &[McpServerConfig]) -> Result<Self>
  pub fn tool_names(&self) -> Vec<String>
  pub async fn get_tool_def(&self, prefixed_name: &str) -> Option<McpToolDef>
  pub async fn call_tool(
  pub fn is_empty(&self) -> bool
  pub fn server_count(&self) -> usize
  pub fn tool_count(&self) -> usize
  pub async fn server_instructions(&self) -> Vec<(String, String)>
```

```rust
fn tool_name_prefix_format()
```

```rust
async fn connect_nonexistent_command_fails_cleanly()
```

```rust
async fn connect_all_nonfatal_on_single_failure()
```

```rust
fn http_transport_requires_url()
```

```rust
fn sse_transport_requires_url()
```

```rust
async fn empty_registry_is_empty()
```

```rust
async fn empty_registry_tool_names_is_empty()
```

```rust
async fn empty_registry_get_tool_def_returns_none()
```

```rust
async fn empty_registry_call_tool_unknown_name_returns_error()
```

```rust
async fn connect_all_empty_gives_zero_servers()
```

#### `mcp/config_types.rs`

**枚举** `McpTransport`:
```rust
pub enum McpTransport {
  /// Spawn a local process and communicate over stdin/stdout.
  #[default]
  Stdio,
  /// HTTP POST transport.
  Http,
  /// Server-Sent Events transport.
  Sse,
}
```

**结构体** `McpServerConfig`:
```rust
pub struct McpServerConfig {
  /// Display name for the server (used for tool prefixing).
  pub name: String,
  /// Command to spawn (for Stdio transport).
  pub command: String,
  /// Arguments for the command.
  #[serde(default)]
  pub args: Vec<String>,
  /// Environment variables to set.
  #[serde(default)]
  pub env: HashMap<String, String>,
  /// Per-server tool call timeout (seconds).
  pub tool_timeout_secs: Option<u64>,
  /// Transport protocol.
  #[serde(default)]
  pub transport: McpTransport,
  /// URL for HTTP/SSE transports.
  pub url: Option<String>,
  /// Additional headers for HTTP/SSE transports.
  #[serde(default)]
  pub headers: HashMap<String, String>,
}
```

#### `mcp/deferred.rs`

**结构体** `DeferredMcpToolStub`:
```rust
pub struct DeferredMcpToolStub {
  /// Prefixed name: `<server_name>__<tool_name>`.
  pub prefixed_name: String,
  /// Human-readable description (extracted from the MCP tool definition).
  pub description: String,
  /// The full tool definition — stored so we can construct a wrapper later.
  def: McpToolDef,
}
```

**Impl** `impl DeferredMcpToolStub`:
```rust
  pub fn new(prefixed_name: String, def: McpToolDef) -> Self
  pub fn activate(&self, registry: Arc<McpRegistry>) -> McpToolWrapper
```

**结构体** `DeferredMcpToolSet`:
```rust
pub struct DeferredMcpToolSet {
  /// All stubs — exposed for test construction.
  pub stubs: Vec<DeferredMcpToolStub>,
  /// Shared registry — exposed for test construction.
  pub registry: Arc<McpRegistry>,
}
```

**Impl** `impl DeferredMcpToolSet`:
```rust
  pub async fn from_registry(registry: Arc<McpRegistry>) -> Self
  pub fn stub_names(&self) -> Vec<&str>
  pub fn len(&self) -> usize
  pub fn is_empty(&self) -> bool
  pub fn get_by_name(&self, name: &str) -> Option<&DeferredMcpToolStub>
  pub fn search(&self, query: &str, max_results: usize) -> Vec<&DeferredMcpToolStub>
  pub fn activate(&self, name: &str) -> Option<Box<dyn Tool>>
  pub fn tool_spec(&self, name: &str) -> Option<ToolSpec>
```

**结构体** `ActivatedToolSet`:
```rust
pub struct ActivatedToolSet {
  tools: HashMap<String, Arc<dyn Tool>>,
}
```

**Impl** `impl ActivatedToolSet`:
```rust
  pub fn new() -> Self
  pub fn activate(&mut self, name: String, tool: Arc<dyn Tool>)
  pub fn is_activated(&self, name: &str) -> bool
  pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>>
  pub fn get_resolved(&self, name: &str) -> Option<Arc<dyn Tool>>
  pub fn tool_specs(&self) -> Vec<ToolSpec>
  pub fn tool_names(&self) -> Vec<&str>
```

**Impl** `impl Default for ActivatedToolSet`:
```rust
  fn default() -> Self
```

```rust
pub fn build_deferred_tools_section(deferred: &DeferredMcpToolSet) -> String
```

```rust
fn make_stub(name: &str, desc: &str) -> DeferredMcpToolStub
```

```rust
fn stub_uses_description_from_def()
```

```rust
fn stub_defaults_description_when_none()
```

```rust
fn activated_set_tracks_activation()
```

```rust
fn activated_set_resolves_unique_suffix()
```

```rust
fn activated_set_rejects_ambiguous_suffix()
```

```rust
fn build_deferred_section_empty_when_no_stubs()
```

```rust
fn build_deferred_section_lists_names()
```

```rust
fn build_deferred_section_includes_tool_search_instruction()
```

```rust
fn build_deferred_section_multiple_servers()
```

```rust
fn keyword_search_ranks_by_hits()
```

```rust
fn get_by_name_returns_correct_stub()
```

```rust
fn search_across_multiple_servers()
```

#### `mcp/mod.rs`

#### `mcp/protocol.rs`

```rust
pub const JSONRPC_VERSION: &str = "2.0"
```

```rust
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05"
```

```rust
pub const PARSE_ERROR: i32 = -32700
```

```rust
pub const INVALID_REQUEST: i32 = -32600
```

```rust
pub const METHOD_NOT_FOUND: i32 = -32601
```

```rust
pub const INVALID_PARAMS: i32 = -32602
```

```rust
pub const INTERNAL_ERROR: i32 = -32603
```

**结构体** `JsonRpcRequest`:
```rust
pub struct JsonRpcRequest {
  pub jsonrpc: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<serde_json::Value>,
  pub method: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub params: Option<serde_json::Value>,
}
```

**Impl** `impl JsonRpcRequest`:
```rust
  pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self
  pub fn notification(method: impl Into<String>, params: serde_json::Value) -> Self
```

**结构体** `JsonRpcResponse`:
```rust
pub struct JsonRpcResponse {
  pub jsonrpc: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub result: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error: Option<JsonRpcError>,
}
```

**结构体** `JsonRpcError`:
```rust
pub struct JsonRpcError {
  pub code: i32,
  pub message: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub data: Option<serde_json::Value>,
}
```

**结构体** `McpToolDef`:
```rust
pub struct McpToolDef {
  pub name: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(rename = "inputSchema")]
  pub input_schema: serde_json::Value,
}
```

**结构体** `McpToolsListResult`:
```rust
pub struct McpToolsListResult {
  pub tools: Vec<McpToolDef>,
}
```

```rust
fn request_serializes_with_id()
```

```rust
fn notification_omits_id()
```

```rust
fn response_deserializes()
```

```rust
fn tool_def_deserializes_input_schema()
```

```rust
fn request_params_included_when_present()
```

```rust
fn notification_has_no_id_field_in_serialized_json()
```

```rust
fn error_response_deserializes_with_code_and_message()
```

```rust
fn error_response_with_data_field()
```

```rust
fn jsonrpc_error_codes_match_spec()
```

```rust
fn mcp_protocol_version_constant_is_correct()
```

```rust
fn tool_def_description_is_optional()
```

```rust
fn tools_list_result_deserializes_multiple_tools()
```

```rust
fn response_round_trip_via_serde()
```

```rust
fn request_new_produces_numeric_id()
```

```rust
fn tools_list_result_with_empty_tools_array()
```

#### `mcp/tool.rs`

**结构体** `McpToolWrapper`:
```rust
pub struct McpToolWrapper {
  /// Prefixed name: `<server_name>__<tool_name>`.
  prefixed_name: String,
  /// Description extracted from the MCP tool definition. Stored as an owned
  /// String so that `description()` can return `&str` with self's lifetime.
  description: String,
  /// JSON schema for the tool's input parameters.
  input_schema: serde_json::Value,
  /// Shared registry — used to dispatch actual tool calls.
  registry: Arc<McpRegistry>,
}
```

**Impl** `impl McpToolWrapper`:
```rust
  pub fn new(prefixed_name: String, def: McpToolDef, registry: Arc<McpRegistry>) -> Self
```

**Impl** `impl Tool for McpToolWrapper`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn source(&self) -> crate::providers::ToolSource
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

```rust
fn make_def(name: &str, description: Option<&str>, schema: serde_json::Value) -> McpToolDef
```

```rust
async fn empty_registry() -> Arc<McpRegistry>
```

```rust
async fn name_returns_prefixed_name()
```

```rust
async fn description_returns_def_description()
```

```rust
async fn description_falls_back_to_mcp_tool_when_none()
```

```rust
async fn parameters_schema_returns_input_schema()
```

```rust
async fn spec_returns_all_three_fields()
```

```rust
async fn execute_returns_non_fatal_error_for_unknown_tool()
```

```rust
async fn execute_success_sets_success_true_and_output()
```

```rust
async fn execute_strips_approved_field_from_object_args()
```

```rust
async fn execute_handles_non_object_args_without_panic()
```

#### `mcp/tool_trait.rs`

### `mcp/transport/`

**子模块说明**: MCP 传输层：HTTP、SSE、STDIO 三种传输方式

#### `mcp/transport/http.rs`

**结构体** `HttpTransport`:
```rust
pub struct HttpTransport {
  url: String,
  client: reqwest::Client,
  headers: std::collections::HashMap<String, String>,
  pub(super) session_id: Option<String>,
}
```

**Impl** `impl HttpTransport`:
```rust
  pub fn new(config: &McpServerConfig) -> Result<Self>
  pub(super) fn apply_session_header(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder
  pub(super) fn update_session_id_from_headers(&mut self, headers: &reqwest::header::HeaderMap)
```

**Impl** `impl McpTransportConn for HttpTransport`:
```rust
  async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>
  async fn close(&mut self) -> Result<()>
```

#### `mcp/transport/mod.rs`

```rust
pub(super) const MAX_LINE_BYTES: usize = 4 * 1024 * 1024; // 4 MB
```

```rust
pub(super) const RECV_TIMEOUT_SECS: u64 = 30
```

```rust
pub(super) const MCP_STREAMABLE_ACCEPT: &str = "application/json, text/event-stream"
```

```rust
pub(super) const MCP_JSON_CONTENT_TYPE: &str = "application/json"
```

```rust
pub(super) const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id"
```

**Trait** `McpTransportConn`:
```rust
pub trait McpTransportConn {
  async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>
  async fn close(&mut self) -> Result<()>
}
```

```rust
pub(super) fn extract_json_from_sse_text(resp_text: &str) -> Cow<'_, str>
```

```rust
pub(super) fn parse_jsonrpc_response_text(resp_text: &str) -> Result<JsonRpcResponse>
```

```rust
pub(super) fn looks_like_sse_text(text: &str) -> bool
```

```rust
pub(super) async fn read_first_jsonrpc_from_sse_response(
```

```rust
pub fn create_transport(config: &McpServerConfig) -> Result<Box<dyn McpTransportConn>>
```

```rust
fn test_transport_default_is_stdio()
```

```rust
fn test_http_transport_requires_url()
```

```rust
fn test_sse_transport_requires_url()
```

```rust
fn test_extract_json_from_sse_data_no_space()
```

```rust
fn test_extract_json_from_sse_with_event_and_id()
```

```rust
fn test_extract_json_from_sse_multiline_data()
```

```rust
fn test_extract_json_from_sse_skips_bom_and_leading_whitespace()
```

```rust
fn test_extract_json_from_sse_uses_last_event_with_data()
```

```rust
fn test_parse_jsonrpc_response_text_handles_plain_json()
```

```rust
fn test_parse_jsonrpc_response_text_handles_sse_framed_json()
```

```rust
fn test_parse_jsonrpc_response_text_rejects_empty_payload()
```

```rust
fn http_transport_updates_session_id_from_response_headers()
```

```rust
fn http_transport_injects_session_id_header_when_available()
```

```rust
fn derive_message_url_replaces_sse_segment_with_messages()
```

```rust
fn derive_message_url_appends_when_no_sse_segment()
```

```rust
fn derive_message_url_returns_none_for_invalid_url()
```

```rust
fn derive_message_url_message_path_variant()
```

```rust
fn parse_endpoint_absolute_http_url_returned_as_is()
```

```rust
fn parse_endpoint_absolute_https_url_returned_as_is()
```

```rust
fn parse_endpoint_relative_path_resolved_against_base()
```

```rust
fn parse_endpoint_json_object_with_endpoint_key()
```

```rust
fn looks_like_sse_text_detects_data_prefix()
```

```rust
fn looks_like_sse_text_detects_event_prefix()
```

```rust
fn looks_like_sse_text_detects_embedded_data_line()
```

```rust
fn looks_like_sse_text_plain_json_is_not_sse()
```

```rust
fn extract_json_skips_comment_lines()
```

```rust
fn extract_json_empty_input_returns_empty_trimmed()
```

```rust
fn extract_json_plain_json_returned_unchanged()
```

```rust
fn parse_jsonrpc_response_rejects_whitespace_only()
```

```rust
fn parse_jsonrpc_response_with_error_result()
```

```rust
fn create_transport_stdio_fails_without_valid_command()
```

```rust
fn create_transport_http_without_url_fails()
```

```rust
fn create_transport_sse_without_url_fails()
```

```rust
fn create_transport_http_with_url_succeeds()
```

```rust
fn create_transport_sse_with_url_succeeds()
```

```rust
fn http_transport_ignores_empty_session_id_header()
```

```rust
fn http_transport_no_session_header_leaves_none()
```

```rust
fn http_transport_apply_session_header_noop_when_no_session()
```

#### `mcp/transport/sse.rs`

**枚举** `SseStreamState`:
```rust
priv enum SseStreamState {
  Unknown,
  Connected,
  Unsupported,
}
```

**结构体** `SseTransport`:
```rust
pub struct SseTransport {
  sse_url: String,
  server_name: String,
  client: reqwest::Client,
  headers: std::collections::HashMap<String, String>,
  stream_state: SseStreamState,
  shared: std::sync::Arc<Mutex<SseSharedState>>,
  notify: std::sync::Arc<Notify>,
  shutdown_tx: Option<oneshot::Sender<()>>,
  reader_task: Option<tokio::task::JoinHandle<()>>,
}
```

**Impl** `impl SseTransport`:
```rust
  pub fn new(config: &McpServerConfig) -> Result<Self>
  async fn ensure_connected(&mut self) -> Result<()>
  async fn get_message_url(&self) -> Result<(String, bool)>
  fn maybe_try_alternate_message_url(
```

**结构体** `SseSharedState`:
```rust
priv struct SseSharedState {
  message_url: Option<String>,
  message_url_from_endpoint: bool,
  pending: std::collections::HashMap<u64, oneshot::Sender<JsonRpcResponse>>,
}
```

```rust
pub(super) fn derive_message_url(sse_url: &str, message_path: &str) -> Option<String>
```

```rust
async fn handle_sse_event(
```

```rust
pub(super) fn parse_endpoint_from_data(sse_url: &str, data: &str) -> Option<String>
```

#### `mcp/transport/stdio.rs`

**结构体** `StdioTransport`:
```rust
pub struct StdioTransport {
  _child: Child,
  stdin: tokio::process::ChildStdin,
  stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}
```

**Impl** `impl StdioTransport`:
```rust
  pub fn new(config: &McpServerConfig) -> Result<Self>
  async fn send_raw(&mut self, line: &str) -> Result<()>
  async fn recv_raw(&mut self) -> Result<String>
```

**Impl** `impl McpTransportConn for StdioTransport`:
```rust
  async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>
  async fn close(&mut self) -> Result<()>
```

---

## `memory/`

**模块说明**: 持久化记忆：用户/项目级 key-value 记忆存储

#### `memory/mod.rs`

```rust
pub const MAX_INDEX_LINES: usize = 200
```

```rust
pub const MAX_INDEX_BYTES: usize = 25_000
```

```rust
pub const MEMORY_DIR_NAME: &str = "memory"
```

**枚举** `MemoryType`:
```rust
pub enum MemoryType {
  User,
  Feedback,
  Project,
  Reference,
}
```

**Impl** `impl MemoryType`:
```rust
  pub fn as_str(&self) -> &'static str
  pub fn from_str_lossy(s: &str) -> Option<Self>
  pub fn injected_types() -> &'static [MemoryType]
  pub fn all() -> &'static [MemoryType]
```

**结构体** `MemoryFile`:
```rust
pub struct MemoryFile {
  pub name: String,
  pub summary: String,
  pub tags: Vec<String>,
  pub mem_type: MemoryType,
  pub created_at: String,
  pub content: String,
  pub path: std::path::PathBuf,
}
```

**结构体** `IndexEntry`:
```rust
pub struct IndexEntry {
  pub mem_type: MemoryType,
  pub name: String,
  pub filename: String,
  pub summary: String,
  pub tags: Vec<String>,
}
```

**Impl** `impl From<&MemoryFile> for IndexEntry`:
```rust
  fn from(f: &MemoryFile) -> Self
```

```rust
pub fn ensure_memory_dir(knowledge_dir: &str) -> std::io::Result<std::path::PathBuf>
```

```rust
pub fn scan_memory_files(memory_dir: &Path) -> Vec<MemoryFile>
```

```rust
fn parse_memory_file(path: &Path) -> Option<MemoryFile>
```

```rust
fn parse_tags(value: &str) -> Vec<String>
```

```rust
pub fn format_memory_index(entries: &[IndexEntry]) -> String
```

```rust
pub fn truncate_index(content: &str, max_lines: usize, max_bytes: usize) -> String
```

```rust
pub fn build_memory_section(knowledge_dir: &str) -> String
```

```rust
fn test_parse_frontmatter_with_summary()
```

```rust
fn test_parse_frontmatter_no_summary()
```

```rust
fn test_parse_no_frontmatter()
```

```rust
fn test_format_index_only_user_and_feedback()
```

```rust
fn test_format_index_empty_injectable()
```

```rust
fn test_truncate_index()
```

---

## `providers/`

**模块说明**: LLM Provider 系统：多供应商适配（OpenAI/Anthropic/Google/GLM/Kimi/MiniMax/Xiaomi）、能力协商、流式传输、工具调用

**外部模块依赖**: `agents`, `capability`, `capability_chat`, `capability_embedding`, `config`, `image`, `search`, `stt`, `tts`, `video`

#### `providers/anthropic.rs`

```rust
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com"
```

**结构体** `AnthropicProvider`:
```rust
pub struct AnthropicProvider {
  base_url: String,
  api_key: String,
  user_agent: Option<String>,
}
```

**Impl** `impl AnthropicProvider`:
```rust
  pub fn new(api_key: String) -> Self
  pub fn with_base_url(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
```

**Impl** `impl ChatProvider for AnthropicProvider`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

#### `providers/capability.rs`

**枚举** `Capability`:
```rust
pub enum Capability {
  Chat,
  Embedding,
  ImageGeneration,
  TextToSpeech,
  SpeechToText,
  VideoGeneration,
  Search,
}
```

**Impl** `impl Capability`:
```rust
  pub fn as_str(&self) -> &'static str
```

**Impl** `impl std::str::FromStr for Capability`:
```rust
  fn from_str(s: &str) -> Result<Self, Self::Err>
```

**Impl** `impl fmt::Display for Capability`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

**枚举** `Modality`:
```rust
pub enum Modality {
  Text,
  Image,
  Audio,
  Video,
}
```

**Impl** `impl Modality`:
```rust
  pub fn as_str(&self) -> &'static str
```

**Impl** `impl std::str::FromStr for Modality`:
```rust
  fn from_str(s: &str) -> Result<Self, Self::Err>
```

**结构体** `ChatPricing`:
```rust
pub struct ChatPricing {
  pub input: Option<f64>,
  pub output: Option<f64>,
  pub cache_write: Option<f64>,
  pub cache_read: Option<f64>,
}
```

**结构体** `EmbeddingPricing`:
```rust
pub struct EmbeddingPricing {
  pub input: Option<f64>,
}
```

**结构体** `BasicPricing`:
```rust
pub struct BasicPricing {
  pub per_unit: Option<f64>,
}
```

**结构体** `ChatModelConfig`:
```rust
pub struct ChatModelConfig {
  pub input: Vec<Modality>,
  pub output: Vec<Modality>,
  pub context_window: Option<u64>,
  pub max_output_tokens: Option<u32>,
  pub pricing: Option<ChatPricing>,
  #[serde(default)]
  pub reasoning: bool,
}
```

**Impl** `impl ChatModelConfig`:
```rust
  pub fn supports_image_input(&self) -> bool
  pub fn supports_input(&self, modality: Modality) -> bool
```

**结构体** `EmbeddingModelConfig`:
```rust
pub struct EmbeddingModelConfig {
  pub dimensions: Option<u32>,
  pub max_tokens: Option<u32>,
  pub pricing: Option<EmbeddingPricing>,
}
```

**结构体** `BasicModelConfig`:
```rust
pub struct BasicModelConfig {
  pub pricing: Option<BasicPricing>,
}
```

#### `providers/capability_chat.rs`

**枚举** `ContentPart`:
```rust
pub enum ContentPart {
  Text { text: String },
  ImageUrl { url: String, detail: ImageDetail },
  ImageB64 {
  b64_json: String,
  /// MIME type of the image (e.g. "image/png"). When absent the renderer
  /// infers the type from the base64 header bytes.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  media_type: Option<String>,
  detail: ImageDetail,
  },
  /// On-disk reference to an externalized image blob (content-addressed by
  /// SHA-256 of the decoded bytes). EXISTS ONLY in persisted `history.jsonl`:
  /// `json_file::append_message` externalizes large `ImageB64` parts into
  /// `blobs/{sha256}.bin` + `ImageRef`; `load_messages` hydrates them back to
  /// `ImageB64` before the in-memory render path. Protocol renderers
  /// (OpenAI/Anthropic/GLM) treat it as `unreachable!` — it must never be
  /// rendered directly.
  ImageRef {
  hash: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  media_type: Option<String>,
  detail: ImageDetail,
  },
  // 音频：与 Image* 同构（无 detail）。AudioB64 内存态、AudioRef 落盘态。
  // 音频对每个模型都转写成文本，故只在「作为 STT aux 模型的输入」时抵达渲染器
  // （OpenAI 渲染为 input_audio 块；Anthropic/GLM 降级为 [audio] 文本；AudioRef 一律 unreachable!）。
  AudioB64 { b64_json: String, media_type: Option<String> },
  AudioRef { hash: String, media_type: Option<String> },
  /// Extended thinking block — stored in message history so it can be
  /// re-sent to the model on subsequent turns (Anthropic protocol requires
  /// the model to see its own reasoning, including the opaque signature).
  #[serde(rename = "thinking")]
  Thinking {
  thinking: String,
  /// Anthropic-issued signature that must be echoed back in subsequent
  /// turns when this block appears in the conversation history.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  signature: Option<String>,
  // ... 1 more variants
}
```

**枚举** `ImageDetail`:
```rust
pub enum ImageDetail {
  #[default]
  Auto,
  Low,
  High,
}
```

**Impl** `ContentPart` + free fn:
```rust
impl ContentPart {
  // 内容指纹：图片用解码字节 sha256（== blobs/{hash}.bin 文件名 == ImageRef.hash），
  // 故跨 ImageB64⇄ImageRef 外化/hydrate 与 b64 重编码往返均不变；URL 用 url 哈希；
  // 不可解码 b64 退化为哈希原串；非媒体 part → None。用作描述缓存键 + 描述 GC 键。
  pub fn content_fingerprint(&self) -> Option<String>
}
// 内容哈希单一实现，blob 存储与内容指纹共用以杜绝漂移：
pub fn sha256_hex(bytes: &[u8]) -> String
```

**结构体** `ChatMessage`:
```rust
pub struct ChatMessage {
  pub role: String,
  #[serde(default)]
  pub parts: Vec<ContentPart>,
  /// Tool call ID for "tool" role messages.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  /// Tool call ID (OpenAI: tool_call_id for "tool" role).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool_call_id: Option<String>,
  /// Tool calls from assistant (OpenAI: tool_calls in assistant message).
  /// Always stored in the canonical ToolCall format regardless of
  /// which provider generated them. Each provider's build_body() is
  /// responsible for translating this into its own wire format.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool_calls: Option<Vec<ToolCall>>,
  /// Whether this tool result message indicates an error.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub is_error: Option<bool>,
}
```

**Impl** `impl ChatMessage`:
```rust
  pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self
  pub fn user_text(text: impl Into<String>) -> Self { Self::text("user", text) }
  pub fn assistant_text(text: impl Into<String>) -> Self { Self::text("assistant", text) }
  pub fn system_text(text: impl Into<String>) -> Self { Self::text("system", text) }
  pub fn with_image_url(mut self, url: impl Into<String>) -> Self
  pub fn text_content(&self) -> String
```

```rust
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>
```

**枚举** `StreamEvent`:
```rust
pub enum StreamEvent {
  Delta { text: String },
  Thinking { text: String },
  /// Opaque signature for the preceding thinking block (Anthropic extended
  /// thinking). Must be stored alongside the thinking text and echoed back
  /// in subsequent requests.
  ThinkingSignature { signature: String },
  ToolCallStart { id: String, name: String, initial_arguments: String },
  ToolCallDelta { id: String, delta: String },
  ToolCallEnd { id: String, name: String, arguments: String },
  Usage(ChatUsage),
  Done { reason: StopReason },
  Error(String),
  /// HTTP-level error with status code; used for retry/fallback decisions.
  HttpError { status: u16, message: String },
}
```

**Impl** `impl StreamEvent`:
```rust
  pub fn is_retryable_error(&self) -> bool
  pub fn classify(&self) -> Option<crate::providers::ClassifiedError>
```

**枚举** `StopReason`:
```rust
pub enum StopReason {
  #[default]
  EndTurn,
  MaxTokens,
  StopSequence,
  ContentFilter,
  ToolUse,
  Timeout,
}
```

**结构体** `ChatUsage`:
```rust
pub struct ChatUsage {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub input_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub output_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cached_input_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reasoning_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cache_write_tokens: Option<u64>,
}
```

**结构体** `ToolCall`:
```rust
pub struct ToolCall {
  pub id: String,
  pub name: String,
  /// JSON string of tool arguments.
  pub arguments: String,
}
```

**Impl** `impl ToolCall`:
```rust
  pub fn to_openai(&self) -> serde_json::Value
```

**结构体** `ToolSpec`:
```rust
pub struct ToolSpec {
  pub name: String,
  pub description: Option<String>,
  pub input_schema: serde_json::Value,
}
```

**结构体** `ChatRequest`:
```rust
pub struct ChatRequest {
  /// Model identifier (filled by ProviderRegistry from routing config).
  pub model: &'a str,
  /// Message list.
  pub messages: &'a [ChatMessage],
  /// Temperature 0.0–2.0.
  pub temperature: Option<f64>,
  /// Maximum output tokens.
  pub max_tokens: Option<u32>,
  /// Reasoning/thinking configuration (set by config, not by user).
  pub thinking: Option<ThinkingConfig>,
  /// Stop sequences.
  pub stop: Option<Vec<String>>,
  /// Random seed.
  pub seed: Option<u64>,
  /// Tool definitions for providers with native tool calling support.
  pub tools: Option<&'a [ToolSpec]>,
  /// Stream flag (always true; caller must not set false).
  pub stream: bool,
}
```

**结构体** `ThinkingConfig`:
```rust
pub struct ThinkingConfig {
  /// Whether thinking/reasoning is enabled. Derived from model config's `reasoning` field.
  pub enabled: bool,
  /// Reasoning effort: "high" | "medium" | "low". Configurable at runtime.
  pub effort: Option<String>,
}
```

**结构体** `ChatResponse`:
```rust
pub struct ChatResponse {
  pub text: String,
  pub tool_calls: Vec<ToolCall>,
  pub usage: Option<ChatUsage>,
  pub reasoning_content: Option<String>,
  /// Anthropic-issued opaque signature for the thinking block.
  /// Must be echoed back when the thinking block is re-sent in subsequent turns.
  pub thinking_signature: Option<String>,
  pub stop_reason: StopReason,
}
```

**Impl** `impl ChatResponse`:
```rust
  pub async fn from_stream(stream: BoxStream<StreamEvent>) -> anyhow::Result<Self>
```

**Trait** `ChatProvider`:
```rust
pub trait ChatProvider {
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
}
```

#### `providers/capability_embedding.rs`

**结构体** `EmbedRequest`:
```rust
pub struct EmbedRequest {
  pub input: EmbedInput,
  pub model: String,
  /// Embedding dimensions (supported by a subset of providers).
  pub dimensions: Option<u32>,
}
```

**枚举** `EmbedInput`:
```rust
pub enum EmbedInput {
  Text(String),
  Texts(Vec<String>),
}
```

**结构体** `EmbedResponse`:
```rust
pub struct EmbedResponse {
  pub embeddings: Vec<f32>,
  pub usage: Option<EmbeddingUsage>,
  pub model: String,
}
```

**结构体** `EmbeddingUsage`:
```rust
pub struct EmbeddingUsage {
  pub prompt_tokens: u64,
}
```

**Trait** `EmbeddingProvider`:
```rust
pub trait EmbeddingProvider {
  fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse>
}
```

#### `providers/capability_tool.rs`

**枚举** `ToolSource`:
```rust
pub enum ToolSource {
  /// Hard-coded tools (shell, file_read, memory_*, etc.).
  Builtin,
  /// Loaded from `workspace/skills/<name>/SKILL.md`.
  Skill { name: String },
  /// Loaded from an MCP server (`mcp_servers` config).
  Mcp { server: String },
}
```

**结构体** `ToolResult`:
```rust
pub struct ToolResult {
  /// Whether the tool executed successfully.
  pub success: bool,
  /// Tool output (text or JSON string).
  pub output: String,
  /// Error message if success is false.
  pub error: Option<String>,
}
```

**结构体** `ToolSpec`:
```rust
pub struct ToolSpec {
  /// Tool name (unique identifier).
  pub name: String,
  /// Human-readable description.
  pub description: String,
  /// JSON Schema for the tool's parameters.
  pub parameters: serde_json::Value,
}
```

**Trait** `Tool`:
```rust
pub trait Tool {
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn source(&self) -> ToolSource
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, session: &Session) -> anyhow::Result<ToolResult>
  fn spec(&self) -> ToolSpec
}
```

#### `providers/credential_pool.rs`

**枚举** `CredentialStatus`:
```rust
pub enum CredentialStatus {
  /// Key is healthy and available.
  Active,
  /// Key is temporarily exhausted (rate limit, billing) — on cooldown.
  Exhausted,
  /// Key has been manually disabled.
  Disabled,
}
```

**结构体** `CredentialEntry`:
```rust
pub struct CredentialEntry {
  pub key: String,
  pub status: CredentialStatus,
  pub exhausted_until: Option<Instant>,
  pub last_used: Option<Instant>,
  pub use_count: u64,
}
```

**枚举** `RotationStrategy`:
```rust
pub enum RotationStrategy {
  /// Use the first key until exhausted, then move to next.
  #[default]
  FillFirst,
  /// Round-robin across all keys.
  RoundRobin,
  /// Random selection among active keys.
  Random,
  /// Pick the key with the lowest use_count.
  LeastUsed,
}
```

**结构体** `CredentialPool`:
```rust
pub struct CredentialPool {
  entries: Vec<CredentialEntry>,
  strategy: RotationStrategy,
  provider_name: String,
  /// Index for round-robin.
  round_robin_idx: usize,
}
```

**Impl** `impl CredentialPool`:
```rust
  pub fn new(provider_name: impl Into<String>, keys: Vec<String>, strategy: RotationStrategy) -> Self
  pub fn len(&self) -> usize
  pub fn is_empty(&self) -> bool
  pub fn refresh(&mut self)
  pub fn next_credential(&mut self) -> Option<&str>
  pub fn mark_exhausted(&mut self, key: &str, reason: &FailoverReason)
  pub fn snapshot(&self) -> Vec<(String, CredentialStatus, Option<Duration>)>
  fn cooldown_for_reason(reason: &FailoverReason) -> Duration
  fn mask_key(key: &str) -> String
```

**结构体** `SharedCredentialPool`:
```rust
pub struct SharedCredentialPool {
  inner: Arc<Mutex<CredentialPool>>,
}
```

**Impl** `impl SharedCredentialPool`:
```rust
  pub fn new(pool: CredentialPool) -> Self
  pub fn next_credential(&self) -> Option<String>
  pub fn mark_exhausted(&self, key: &str, reason: &FailoverReason)
  pub fn snapshot(&self) -> Vec<(String, CredentialStatus, Option<Duration>)>
  pub fn len(&self) -> usize
  pub fn is_empty(&self) -> bool
```

```rust
fn fill_first_uses_first_active()
```

```rust
fn round_robin_rotates()
```

```rust
fn exhausted_key_skipped()
```

```rust
fn cooldown_expires_and_restores()
```

```rust
fn mask_key_hides_middle()
```

#### `providers/error_class.rs`

**结构体** `ProviderHttpError`:
```rust
pub struct ProviderHttpError {
  pub status: u16,
  pub message: String,
}
```

**枚举** `ErrorCategory`:
```rust
pub enum ErrorCategory {
  /// Authentication failure (401/403).
  Auth,
  /// Permanent authentication failure (API key invalid/revoked).
  AuthPermanent,
  /// Billing/quota exhaustion.
  Billing,
  /// Rate limiting (429).
  RateLimit,
  /// Provider overloaded (503/529).
  Overloaded,
  /// Internal server error (500/502).
  ServerError,
  /// Timeout (504, connection timeout, missing HTTP status).
  Timeout,
  /// Model not found (404).
  ModelNotFound,
  /// Context window overflow.
  ContextOverflow,
  /// Request payload too large (413).
  PayloadTooLarge,
  // ... 1 more variants
}
```

**Impl** `impl fmt::Display for ErrorCategory`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

**枚举** `FailoverReason`:
```rust
pub enum FailoverReason {
  /// Transient auth failure (401/403) — refresh/rotate.
  Auth,
  /// Auth failed after refresh — abort.
  AuthPermanent,
  /// Billing exhausted (402 / confirmed credit depletion) — rotate immediately.
  Billing,
  /// Rate limit (429 / quota-based throttling) — backoff then rotate.
  RateLimit,
  /// Provider overloaded (503/529) — backoff.
  Overloaded,
  /// Internal server error (500/502) — retry.
  ServerError,
  /// Connection/read timeout — rebuild client + retry.
  Timeout,
  /// Context too large — compress, not failover.
  ContextOverflow,
  /// Payload too large (413) — compress payload.
  PayloadTooLarge,
  /// Model not found (404) — fallback to different model.
  ModelNotFound,
  // ... 2 more variants
}
```

**Impl** `impl From<ErrorCategory> for FailoverReason`:
```rust
  fn from(cat: ErrorCategory) -> Self
```

**结构体** `RecoveryHints`:
```rust
pub struct RecoveryHints {
  /// Whether the operation should be retried.
  pub retry: bool,
  /// How long to wait before retrying.
  pub cooldown: Option<Duration>,
  /// Whether to report this error upstream (e.g. to monitoring).
  pub report: bool,
}
```

**结构体** `ClassifiedError`:
```rust
pub struct ClassifiedError {
  /// The error category (primary classification).
  pub category: ErrorCategory,
  /// Backward-compatible failover reason.
  pub reason: FailoverReason,
  /// HTTP status code (`None` if unavailable / status 0).
  pub status_code: Option<u16>,
  /// Provider name.
  pub provider: Option<String>,
  /// Model identifier.
  pub model: Option<String>,
  /// Human-readable error message.
  pub message: String,
  /// Retry-after duration extracted from response body.
  pub retry_after: Option<Duration>,
  // ── Backward-compat boolean flags (derived from category) ──
  /// Whether the error is transient and retry may succeed.
  pub retryable: bool,
  /// Whether to trigger context compression before retry.
  pub should_compress: bool,
  /// Whether to rotate to the next credential before retry.
  pub should_rotate_credential: bool,
  /// Whether to failover to the next provider in the chain.
  pub should_fallback: bool,
  /// Cooldown duration before retrying this credential.
  pub cooldown: Option<Duration>,
}
```

**Impl** `impl ClassifiedError`:
```rust
  pub fn classify(provider: &str, status: u16, body: &str) -> Self
  pub fn new(reason: FailoverReason, message: impl Into<String>) -> Self
  pub fn from_http(status: u16, message: Option<&str>) -> Self
  pub fn from_message(message: &str) -> Self
  pub fn with_provider(
  pub fn is_auth(&self) -> bool
  pub fn recovery_hints(&self) -> RecoveryHints
  pub fn cooldown_duration(&self) -> Option<Duration>
  pub fn should_report(&self) -> bool
  fn from_parts(
```

```rust
fn recovery_hints_for(
```

```rust
fn classify_http(status: u16) -> Option<ErrorCategory>
```

```rust
fn classify_provider(
```

```rust
fn body_contains_code(body: &str, code: u64) -> bool
```

```rust
fn extract_retry_after(body: &str) -> Option<Duration>
```

```rust
fn json_get_seconds(json: &serde_json::Value, key: &str) -> Option<Duration>
```

```rust
fn layer1_401_is_auth()
```

```rust
fn layer1_503_is_overloaded()
```

```rust
fn layer1_504_is_timeout()
```

```rust
fn layer1_404_is_model_not_found()
```

```rust
fn layer1_413_is_payload_too_large()
```

```rust
fn layer2_glm_429_1312_overloaded()
```

```rust
fn layer2_glm_429_1308_billing()
```

```rust
fn layer2_glm_429_1309_billing()
```

```rust
fn layer2_openai_429_insufficient_quota()
```

```rust
fn layer2_429_generic_is_rate_limit()
```

```rust
fn layer2_glm_400_1261_context_overflow()
```

```rust
fn layer2_400_context_length_exceeded()
```

```rust
fn layer2_400_generic_is_format_error()
```

```rust
fn layer3_status_0_is_timeout()
```

```rust
fn from_message_is_timeout()
```

```rust
fn auth_cooldown_30min()
```

```rust
fn rate_limit_uses_retry_after()
```

```rust
fn rate_limit_default_cooldown_1h()
```

```rust
fn billing_report_true()
```

```rust
fn format_error_report_true()
```

```rust
fn overloaded_report_false()
```

```rust
fn new_constructs_from_failover_reason()
```

```rust
fn is_auth_works()
```

```rust
fn with_provider_sets_metadata()
```

```rust
fn body_code_compact_and_spaced()
```

```rust
fn retry_after_from_json()
```

```rust
fn retry_after_from_top_level()
```

```rust
fn retry_after_empty_body()
```

```rust
fn retry_after_no_field()
```

#### `providers/fallback.rs`

```rust
pub const CHAIN_EXHAUSTED_TAG: &str = "fallback_chain_exhausted"
```

```rust
pub const CHAIN_ALL_COOLING_TAG: &str = "fallback_chain_all_cooling"
```

**结构体** `FallbackEntry`:
```rust
pub struct FallbackEntry {
  pub provider: Arc<dyn ChatProvider>,
  pub model_id: String,
  /// Optional credential pool for same-provider key rotation.
  pub credential_pool: Option<SharedCredentialPool>,
}
```

**结构体** `FallbackChatProvider`:
```rust
pub struct FallbackChatProvider {
  chain: Vec<FallbackEntry>,
  /// Per-model cooldown deadlines, shared across clones so all requests see
  /// the same state.  Keyed by model_id; value is the earliest Instant at
  /// which the model should be tried again.
  model_cooldowns: Arc<Mutex<HashMap<String, Instant>>>,
}
```

**Impl** `impl FallbackChatProvider`:
```rust
  pub fn new(chain: Vec<FallbackEntry>) -> Self
```

```rust
fn is_provider_error(cat: &ErrorCategory) -> bool
```

```rust
fn record_cooldown(
```

**Impl** `impl ChatProvider for FallbackChatProvider`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

#### `providers/glm.rs`

```rust
const DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/paas"
```

**结构体** `GlmProvider`:
```rust
pub struct GlmProvider {
  base_url: String,
  api_key: String,
  client: Client,
  user_agent: Option<String>,
}
```

**Impl** `impl GlmProvider`:
```rust
  pub fn new(api_key: String) -> Self
  pub fn with_base_url(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
  fn auth(&self) -> String
  fn embeddings_url(&self) -> String
  fn web_search_url(&self) -> String
```

**Impl** `impl ChatProvider for GlmProvider`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

```rust
fn build_glm_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value
```

```rust
fn parse_glm_sse(line: &str, saw_tool_call: &mut bool) -> Option<Vec<StreamEvent>>
```

**Impl** `impl EmbeddingProvider for GlmProvider`:
```rust
  fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse>
```

**Impl** `impl SearchProvider for GlmProvider`:
```rust
  fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults>
```

#### `providers/google.rs`

```rust
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com"
```

```rust
const DEFAULT_MODEL: &str = "gemini-2.0-flash"
```

**结构体** `GoogleProvider`:
```rust
pub struct GoogleProvider {
  base_url: String,
  api_key: String,
  client: reqwest::Client,
}
```

**Impl** `impl GoogleProvider`:
```rust
  pub fn new(api_key: String) -> Self
  pub fn with_base_url(api_key: String, base_url: String) -> Self
```

**结构体** `GeminiResponse`:
```rust
priv struct GeminiResponse {
  candidates: Option<Vec<Candidate>>,
  error: Option<GeminiError>,
}
```

**结构体** `Candidate`:
```rust
priv struct Candidate {
  content: Option<Content>,
  #[serde(rename = "groundingMetadata")]
  grounding_metadata: Option<GroundingMetadata>,
}
```

**结构体** `Content`:
```rust
priv struct Content {
  parts: Option<Vec<Part>>,
}
```

**结构体** `Part`:
```rust
priv struct Part {
  text: Option<String>,
}
```

**结构体** `GroundingMetadata`:
```rust
priv struct GroundingMetadata {
  #[serde(rename = "groundingChunks")]
  grounding_chunks: Option<Vec<GroundingChunk>>,
  #[serde(rename = "groundingSupports")]
  #[allow(dead_code)]
  grounding_supports: Option<Vec<GroundingSupport>>,
}
```

**结构体** `GroundingChunk`:
```rust
priv struct GroundingChunk {
  web: Option<WebChunk>,
}
```

**结构体** `WebChunk`:
```rust
priv struct WebChunk {
  uri: Option<String>,
  title: Option<String>,
}
```

**结构体** `GroundingSupport`:
```rust
priv struct GroundingSupport {
  #[serde(rename = "groundingChunkIndices")]
  #[allow(dead_code)]
  grounding_chunk_indices: Option<Vec<u64>>,
  #[allow(dead_code)]
  segment: Option<Segment>,
}
```

**结构体** `Segment`:
```rust
priv struct Segment {
  #[allow(dead_code)]
  text: Option<String>,
}
```

**结构体** `GeminiError`:
```rust
priv struct GeminiError {
  code: Option<u32>,
  message: Option<String>,
  status: Option<String>,
}
```

**Impl** `impl SearchProvider for GoogleProvider`:
```rust
  fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults>
```

#### `providers/http.rs`

```rust
pub fn build_reqwest_client() -> Client
```

#### `providers/image.rs`

**结构体** `ImageRequest`:
```rust
pub struct ImageRequest {
  pub model: String,
  pub prompt: String,
  pub response_format: Option<ImageFormat>,
  pub size: Option<ImageSize>,
  pub quality: Option<ImageQuality>,
  pub n: Option<u32>,
}
```

**枚举** `ImageFormat`:
```rust
pub enum ImageFormat {
  Url,
  B64Json,
}
```

**枚举** `ImageSize`:
```rust
pub enum ImageSize {
  Square1024,
  Landscape1792,
  Portrait1024,
}
```

**枚举** `ImageQuality`:
```rust
pub enum ImageQuality {
  Standard,
  HD,
}
```

**结构体** `ImageResponse`:
```rust
pub struct ImageResponse {
  pub images: Vec<ImageOutput>,
  pub usage: Option<ImageGenerationUsage>,
}
```

**结构体** `ImageOutput`:
```rust
pub struct ImageOutput {
  pub url: Option<String>,
  pub b64_json: Option<String>,
  pub revised_prompt: Option<String>,
}
```

**结构体** `ImageGenerationUsage`:
```rust
pub struct ImageGenerationUsage {
  pub prompt_tokens: u64,
  pub completion_tokens: Option<u64>,
}
```

**Trait** `ImageGenerationProvider`:
```rust
pub trait ImageGenerationProvider {
  fn generate_image(&self, req: ImageRequest) -> anyhow::Result<ImageResponse>
}
```

#### `providers/kimi.rs`

```rust
const DEFAULT_BASE_URL: &str = "https://api.moonshot.cn"
```

**结构体** `KimiProvider`:
```rust
pub struct KimiProvider {
  base_url: String,
  api_key: String,
  user_agent: Option<String>,
}
```

**Impl** `impl KimiProvider`:
```rust
  pub fn new(api_key: String) -> Self
  pub fn with_base_url(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
```

**Impl** `impl ChatProvider for KimiProvider`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

#### `providers/minimax.rs`

```rust
const DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/anthropic"
```

```rust
const SEARCH_BASE_URL: &str = "https://api.minimaxi.com"
```

**结构体** `MiniMaxProvider`:
```rust
pub struct MiniMaxProvider {
  inner: AnthropicProvider,
  api_key: String,
  base_url: String,
}
```

**Impl** `impl MiniMaxProvider`:
```rust
  pub fn new(api_key: String) -> Self
  pub fn with_base_url(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
```

**Impl** `impl ChatProvider for MiniMaxProvider`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

**Impl** `impl SearchProvider for MiniMaxProvider`:
```rust
  fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults>
```

#### `providers/mod.rs`

#### `providers/openai.rs`

```rust
const DEFAULT_BASE_URL: &str = "https://api.openai.com"
```

**结构体** `OpenAiProvider`:
```rust
pub struct OpenAiProvider {
  base_url: String,
  api_key: String,
  client: Client,
  user_agent: Option<String>,
}
```

**Impl** `impl OpenAiProvider`:
```rust
  pub fn new(api_key: String) -> Self
  pub fn with_base_url(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
  fn auth(&self) -> String
  fn images_url(&self) -> String { format!("{}/v1/images/generations", self.base_url.trim_end_matches('/')) }
  fn embeddings_url(&self) -> String { format!("{}/v1/embeddings", self.base_url.trim_end_matches('/')) }
  fn tts_url(&self) -> String { format!("{}/v1/audio/speech", self.base_url.trim_end_matches('/')) }
  fn common_headers(&self) -> reqwest::header::HeaderMap
```

**Impl** `impl ChatProvider for OpenAiProvider`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

**Impl** `impl ImageGenerationProvider for OpenAiProvider`:
```rust
  fn generate_image(&self, req: ImageRequest) -> anyhow::Result<ImageResponse>
```

**Impl** `impl TtsProvider for OpenAiProvider`:
```rust
  fn synthesize(&self, req: TtsRequest) -> anyhow::Result<crate::providers::tts::AudioResponse>
```

**Impl** `impl EmbeddingProvider for OpenAiProvider`:
```rust
  fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse>
```

#### `providers/provider_factory.rs`

**结构体** `BuildChatProviderRequest`:
```rust
pub struct BuildChatProviderRequest {
  pub provider_key: String,
  pub provider_id: ProviderId,
  pub protocol: Option<Protocol>,
  pub base_url: String,
  pub api_key: String,
  pub auth_style: AuthStyle,
  pub user_agent: Option<String>,
}
```

**结构体** `BuildEmbeddingProviderRequest`:
```rust
pub struct BuildEmbeddingProviderRequest {
  pub provider_key: String,
  pub provider_id: ProviderId,
  pub base_url: String,
  pub api_key: String,
  pub auth_style: AuthStyle,
  pub user_agent: Option<String>,
}
```

**结构体** `BuildImageProviderRequest`:
```rust
pub struct BuildImageProviderRequest {
  pub provider_key: String,
  pub provider_id: ProviderId,
  pub base_url: String,
  pub api_key: String,
  pub auth_style: AuthStyle,
  pub user_agent: Option<String>,
}
```

**结构体** `BuildTtsProviderRequest`:
```rust
pub struct BuildTtsProviderRequest {
  pub provider_key: String,
  pub provider_id: ProviderId,
  pub base_url: String,
  pub api_key: String,
  pub auth_style: AuthStyle,
  pub user_agent: Option<String>,
}
```

**结构体** `BuildSearchProviderRequest`:
```rust
pub struct BuildSearchProviderRequest {
  pub provider_key: String,
  pub provider_id: ProviderId,
  pub base_url: String,
  pub api_key: String,
  pub auth_style: AuthStyle,
  pub user_agent: Option<String>,
}
```

**结构体** `BuildVideoProviderRequest`:
```rust
pub struct BuildVideoProviderRequest {
  pub provider_key: String,
  pub provider_id: ProviderId,
  pub base_url: String,
  pub api_key: String,
  pub auth_style: AuthStyle,
  pub user_agent: Option<String>,
}
```

**结构体** `BuildSttProviderRequest`:
```rust
pub struct BuildSttProviderRequest {
  pub provider_key: String,
  pub provider_id: ProviderId,
  pub base_url: String,
  pub api_key: String,
  pub auth_style: AuthStyle,
  pub user_agent: Option<String>,
}
```

**结构体** `ProviderFactory`:
```rust
pub struct ProviderFactory {
}
```

**Impl** `impl Default for ProviderFactory`:
```rust
  fn default() -> Self
```

**Impl** `impl ProviderFactory`:
```rust
  pub fn new() -> Self
  fn resolve_protocol(provider_id: &ProviderId, configured: Option<Protocol>) -> Protocol
  pub fn build_chat_provider(
  pub fn build_embedding_provider(
  pub fn build_image_provider(
  pub fn build_tts_provider(
  pub fn build_search_provider(
  pub fn build_video_provider(
  pub fn build_stt_provider(
```

#### `providers/provider_id.rs`

**结构体** `ProviderId`:
```rust
pub struct ProviderId {
}
```

**Impl** `impl ProviderId`:
```rust
  pub fn new(value: impl Into<String>) -> Self
  pub fn as_str(&self) -> &str
```

**Impl** `impl fmt::Display for ProviderId`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

```rust
pub const GENERIC: &str = "generic"
```

```rust
pub const OPENAI: &str = "openai"
```

```rust
pub const ANTHROPIC: &str = "anthropic"
```

```rust
pub const GLM: &str = "glm"
```

```rust
pub const XIAOMI: &str = "xiaomi"
```

```rust
pub const KIMI: &str = "kimi"
```

```rust
pub const MINIMAX: &str = "minimax"
```

```rust
pub const GOOGLE: &str = "google"
```

```rust
pub fn detect_from_url(base_url: &str) -> Option<ProviderId>
```

```rust
fn detect_glm()
```

```rust
fn detect_xiaomi()
```

```rust
fn detect_openai()
```

```rust
fn detect_unknown()
```

#### `providers/provider_registry.rs`

**结构体** `ProviderSummary`:
```rust
pub struct ProviderSummary {
  /// Provider key (e.g. "openai", "google", "minimax").
  pub name: String,
  /// Model IDs registered for Chat capability.
  pub chat_models: Vec<String>,
  /// Model IDs registered for Search capability.
  pub search_models: Vec<String>,
}
```

**Trait** `ProviderRegistry`:
```rust
pub trait ProviderRegistry {
  fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>
  fn get_chat_provider_with_hint(&self, capability: Capability, provider_hint: Option<&str>) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>
  fn get_chat_fallback_chain(&self, capability: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>>
  fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)>
  fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)>
  fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)>
  fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)>
  fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)>
  fn get_search_fallback_chain(&self) -> anyhow::Result<Vec<(Arc<dyn SearchProvider>, String, String)>>
  fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)>
  fn get_chat_model_config(&self, model_id: &str) -> anyhow::Result<&ChatModelConfig>
  fn get_chat_provider_by_model(&self, model_id: &str) -> Option<(Arc<dyn ChatProvider>, String)>
  fn get_chat_routing_models(&self) -> Vec<String>
  fn get_all_provider_summaries(&self) -> Vec<ProviderSummary>
  // Default method: searches the user-declared chat routing chain for a model
  // whose ChatModelConfig.input contains `modality` (no global auto-discovery).
  fn find_chat_model_with_modality(&self, modality: Modality) -> Option<(Arc<dyn ChatProvider>, String)>
}
```

#### `providers/search.rs`

**结构体** `SearchRequest`:
```rust
pub struct SearchRequest {
  pub query: String,
  pub limit: Option<usize>,
  pub search_type: Option<String>,
}
```

**结构体** `SearchResults`:
```rust
pub struct SearchResults {
  pub results: Vec<SearchResult>,
  pub total: Option<u64>,
  pub query: String,
}
```

**结构体** `SearchResult`:
```rust
pub struct SearchResult {
  pub title: String,
  pub url: String,
  pub snippet: String,
  pub published_at: Option<String>,
}
```

**Trait** `SearchProvider`:
```rust
pub trait SearchProvider {
  fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults>
}
```

#### `providers/shared.rs`

**枚举** `AuthStyle`:
```rust
pub enum AuthStyle {
  Bearer,
  XApiKey,
}
```

```rust
pub fn build_auth(auth: &AuthStyle, credential: &str) -> String
```

#### `providers/stt.rs`

**结构体** `SttRequest`:
```rust
pub struct SttRequest {
  pub model: String,
  pub audio: SttAudioInput,
  pub language: Option<String>,
  pub auto_detect: Option<bool>,
}
```

**枚举** `SttAudioInput`:
```rust
pub enum SttAudioInput {
  Url(String),
  Bytes { data: Vec<u8>, mime_type: String },
}
```

**结构体** `TranscriptionResponse`:
```rust
pub struct TranscriptionResponse {
  pub text: String,
  pub language: Option<String>,
  pub duration_secs: Option<f32>,
  pub segments: Option<Vec<SttSegment>>,
  pub usage: Option<SttUsage>,
}
```

**结构体** `SttSegment`:
```rust
pub struct SttSegment {
  pub start_secs: f32,
  pub end_secs: f32,
  pub text: String,
}
```

**结构体** `SttUsage`:
```rust
pub struct SttUsage {
  pub audio_duration_secs: f32,
  pub prompt_tokens: Option<u64>,
}
```

**Trait** `SttProvider`:
```rust
pub trait SttProvider {
  fn transcribe(&self, req: SttRequest) -> anyhow::Result<TranscriptionResponse>
}
```

#### `providers/tts.rs`

**结构体** `TtsRequest`:
```rust
pub struct TtsRequest {
  pub model: String,
  pub input: String,
  pub voice: TtsVoice,
  pub response_format: Option<TtsFormat>,
  /// Playback speed 0.25–4.0, default 1.0.
  pub speed: Option<f32>,
}
```

**枚举** `TtsVoice`:
```rust
pub enum TtsVoice {
  Id(String),
}
```

**枚举** `TtsFormat`:
```rust
pub enum TtsFormat {
  Mp3,
  Opus,
  Flac,
  Wav,
}
```

**结构体** `AudioResponse`:
```rust
pub struct AudioResponse {
  pub audio: AudioData,
  pub usage: Option<TtsUsage>,
}
```

**结构体** `AudioData`:
```rust
pub struct AudioData {
  pub bytes: Vec<u8>,
  pub mime_type: String,
}
```

**结构体** `TtsUsage`:
```rust
pub struct TtsUsage {
  pub characters: u64,
  pub audio_duration_secs: Option<f32>,
}
```

**Trait** `TtsProvider`:
```rust
pub trait TtsProvider {
  fn synthesize(&self, req: TtsRequest) -> anyhow::Result<AudioResponse>
}
```

#### `providers/video.rs`

**结构体** `VideoRequest`:
```rust
pub struct VideoRequest {
  pub model: String,
  pub prompt: String,
  pub duration_secs: Option<u32>,
  pub resolution: Option<VideoResolution>,
  pub aspect_ratio: Option<AspectRatio>,
}
```

**枚举** `VideoResolution`:
```rust
pub enum VideoResolution {
  Standard,
  HD,
}
```

**枚举** `AspectRatio`:
```rust
pub enum AspectRatio {
  Landscape16x9,
  Portrait9x16,
  Square1x1,
}
```

**结构体** `VideoResponse`:
```rust
pub struct VideoResponse {
  pub videos: Vec<VideoOutput>,
  pub usage: Option<VideoUsage>,
}
```

**结构体** `VideoOutput`:
```rust
pub struct VideoOutput {
  pub url: Option<String>,
  pub path: Option<String>,
  pub revised_prompt: Option<String>,
}
```

**结构体** `VideoUsage`:
```rust
pub struct VideoUsage {
  pub video_duration_secs: u32,
  pub prompt_tokens: u64,
}
```

**Trait** `VideoGenerationProvider`:
```rust
pub trait VideoGenerationProvider {
  fn generate_video(&self, req: VideoRequest) -> anyhow::Result<VideoResponse>
}
```

#### `providers/xiaomi.rs`

```rust
const DEFAULT_BASE_URL: &str = "https://api.xiaomimimo.com/anthropic"
```

**结构体** `XiaomiProvider`:
```rust
pub struct XiaomiProvider {
  base_url: String,
  api_key: String,
  user_agent: Option<String>,
}
```

**Impl** `impl XiaomiProvider`:
```rust
  pub fn new(api_key: String) -> Self
  pub fn with_base_url(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
```

```rust
fn patch_mimo_thinking(body: &mut serde_json::Value)
```

**Impl** `impl ChatProvider for XiaomiProvider`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

### `providers/protocols/`

**子模块说明**: LLM 协议层：OpenAI Chat Completions 和 Anthropic Messages API 的消息渲染与协议适配

#### `providers/protocols/anthropic/message_rendering.rs`

```rust
fn detect_image_media_type(b64: &str) -> &'static str
```

**结构体** `RenderedAnthropicMessages`:
```rust
pub struct RenderedAnthropicMessages {
  pub system_prompt: Option<String>,
  pub messages: Vec<serde_json::Value>,
}
```

```rust
pub fn render_anthropic_messages<'a>(req: &ChatRequest<'a>) -> RenderedAnthropicMessages
```

```rust
pub fn build_anthropic_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value
```

#### `providers/protocols/anthropic/messages.rs`

**结构体** `AnthropicMessagesClient`:
```rust
pub struct AnthropicMessagesClient {
  base_url: String,
  api_key: String,
  client: Client,
  user_agent: Option<String>,
}
```

**Impl** `impl ChatProvider for AnthropicMessagesClient`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

**Impl** `impl AnthropicMessagesClient`:
```rust
  pub fn new(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
  fn chat_url(&self) -> String
  pub fn chat_with_body(
```

```rust
fn parse_anthropic_error_body(body: &str) -> Option<String>
```

```rust
fn parse_anthropic_sse(
```

#### `providers/protocols/anthropic/mod.rs`

#### `providers/protocols/mod.rs`

#### `providers/protocols/openai/chat_completions.rs`

**结构体** `OpenAiChatCompletionsClient`:
```rust
pub struct OpenAiChatCompletionsClient {
  base_url: String,
  api_key: String,
  client: Client,
  user_agent: Option<String>,
}
```

**Impl** `impl OpenAiChatCompletionsClient`:
```rust
  pub fn new(api_key: String, base_url: String) -> Self
  pub fn with_user_agent(mut self, user_agent: String) -> Self
  fn auth(&self) -> String
  fn chat_url(&self) -> String
  fn common_headers(&self) -> reqwest::header::HeaderMap
```

**Impl** `impl ChatProvider for OpenAiChatCompletionsClient`:
```rust
  fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>
```

```rust
fn parse_openai_sse(line: &str) -> Vec<StreamEvent>
```

#### `providers/protocols/openai/chat_message_rendering.rs`

```rust
fn detect_image_media_type(b64: &str) -> &'static str
```

```rust
pub fn render_openai_chat_body<'a>(req: &ChatRequest<'a>) -> serde_json::Value
```

#### `providers/protocols/openai/mod.rs`

---

## `registry/`

**模块说明**: 服务注册表：Provider 和 Channel 的注册与路由

**外部模块依赖**: `providers`

#### `registry/mod.rs`

**结构体** `ProviderConfig`:
```rust
pub struct ProviderConfig {
  pub api: String,
  pub api_key: Option<String>,
  pub base_url: Option<String>,
  pub models: Vec<ModelConfig>,
}
```

**结构体** `ModelConfig`:
```rust
pub struct ModelConfig {
  pub model_id: String,
  pub capabilities: Vec<Capability>,
  pub context_window: Option<u64>,
  pub max_tokens: Option<u32>,
  pub reasoning: bool,
}
```

**Impl** `impl ModelConfig`:
```rust
  pub fn supports(&self, capability: Capability) -> bool
```

**结构体** `Registry`:
```rust
pub struct Registry {
  providers: HashMap<String, ProviderConfig>,
  model_index: HashMap<String, (String, ModelConfig)>,
  routing: RoutingConfig,
  chat_providers: HashMap<String, Arc<dyn ChatProvider>>,
  chat_model_configs: HashMap<String, crate::providers::capability::ChatModelConfig>,
  embedding_providers: HashMap<String, Arc<dyn EmbeddingProvider>>,
  image_providers: HashMap<String, Arc<dyn ImageGenerationProvider>>,
  tts_providers: HashMap<String, Arc<dyn TtsProvider>>,
  video_providers: HashMap<String, Arc<dyn VideoGenerationProvider>>,
  search_providers: HashMap<String, Arc<dyn SearchProvider>>,
  stt_providers: HashMap<String, Arc<dyn SttProvider>>,
  // Stored separately so the primary model's raw entry in chat_providers is
  // never overwritten and remains reachable via get_chat_provider_by_model.
  fallback_chat_provider: Option<(Arc<dyn ChatProvider>, String)>,
}
```

**Impl** `impl Registry`:
```rust
  pub fn new(providers: HashMap<String, ProviderConfig>, routing: RoutingConfig) -> Self
  fn find_provider_by_model(&self, model_id: &str) -> anyhow::Result<(&str, &ModelConfig)>
  fn select_model(&self, entry: &RouteEntry, capability: Capability) -> anyhow::Result<&ModelConfig>
  pub fn get_chat_routing(&self) -> anyhow::Result<&RouteEntry>
  pub fn resolve_model(&self, model_id: &str) -> anyhow::Result<(&str, &ModelConfig)>
  pub fn provider_names(&self) -> impl Iterator<Item = &str>
  pub fn get_provider(&self, name: &str) -> Option<&ProviderConfig>
  pub fn from_config(
```

**Impl** `impl From<crate::config::provider::ProviderConfig> for ProviderConfig`:
```rust
  fn from(cfg: crate::config::provider::ProviderConfig) -> Self
```

**Impl** `impl RoutingConfig`:
```rust
  pub fn from_other(other: &crate::config::routing::RoutingConfig) -> Self
  fn convert_entry(e: &crate::config::routing::RouteEntry) -> RouteEntry
```

**Impl** `impl Registry`:
```rust
  pub fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String, model_config: crate::providers::capability::ChatModelConfig)
  pub fn register_embedding(&mut self, provider: Box<dyn EmbeddingProvider>, model_id: String)
  pub fn register_image(&mut self, provider: Box<dyn ImageGenerationProvider>, model_id: String)
  pub fn register_tts(&mut self, provider: Box<dyn TtsProvider>, model_id: String)
  pub fn register_video(&mut self, provider: Box<dyn VideoGenerationProvider>, model_id: String)
  pub fn register_search(&mut self, provider: Box<dyn SearchProvider>, model_id: String)
  pub fn register_stt(&mut self, provider: Box<dyn SttProvider>, model_id: String)
  pub fn maybe_wrap_chat_fallback(&mut self, routing: &crate::config::routing::RoutingConfig)
```

**Impl** `impl ProviderRegistry for Registry`:
```rust
  fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>
  fn get_chat_provider_with_hint(
  fn get_chat_fallback_chain(&self, capability: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>>
  fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)>
  fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)>
  fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)>
  fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)>
  fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)>
  fn get_search_fallback_chain(&self) -> anyhow::Result<Vec<(Arc<dyn SearchProvider>, String, String)>>
  fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)>
  fn get_chat_model_config(&self, model_id: &str) -> anyhow::Result<&crate::providers::capability::ChatModelConfig>
  fn get_chat_provider_by_model(&self, model_id: &str) -> Option<(Arc<dyn ChatProvider>, String)>
  fn get_chat_routing_models(&self) -> Vec<String>
  fn get_all_provider_summaries(&self) -> Vec<crate::providers::ProviderSummary>
```

**Impl** `impl Registry`:
```rust
  // Auxiliary-model resolution for non-text modalities reuses the
  // `ProviderRegistry::find_chat_model_with_modality` trait default (walks the
  // `[routing.chat]` chain only, no global discovery). No dedicated
  // `aux_model_for` wrapper — per the multimodal RFC §4.6 the per-modality
  // override is not implemented, so the wrapper would be dead code.
  fn route_capability<T: ?Sized + Send + Sync>(
```

#### `registry/routing.rs`

**枚举** `RoutingStrategy`:
```rust
pub enum RoutingStrategy {
  #[default]
  Fixed,
  Fallback,
  Cheapest,
  Fastest,
}
```

**结构体** `RouteEntry`:
```rust
pub struct RouteEntry {
  pub strategy: RoutingStrategy,
  pub models: Vec<String>,
  /// Candidate provider names (for capabilities that route by provider).
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub providers: Vec<String>,
}
```

**结构体** `RoutingConfig`:
```rust
pub struct RoutingConfig {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub chat: Option<RouteEntry>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub search: Option<RouteEntry>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub embedding: Option<RouteEntry>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub image_generation: Option<RouteEntry>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub text_to_speech: Option<RouteEntry>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub speech_to_text: Option<RouteEntry>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub video_generation: Option<RouteEntry>,
}
```

**Impl** `impl RoutingConfig`:
```rust
  pub fn get(&self, capability: Capability) -> Option<&RouteEntry>
```

---

## `storage/`

**模块说明**: 存储后端：JSON 文件存储、Session 持久化、共享/私有 KV 存储

#### `storage/json_file.rs`

**图片 blob 外化 (多模态 Stage 1)**: 大尺寸内联图片会膨胀 `history.jsonl`。
`append_message` 在序列化前 **externalize**：每个 base64 长度 > 8KB 的
`ContentPart::ImageB64` 被解码 → `sha256(bytes)` → 写入内容寻址 blob
`sessions/{session_id}/blobs/{sha256}.bin`（原始解码字节），并替换为
`ImageRef { hash, media_type, detail }`；小图（≤8KB）保持内联。
`load_messages` / `read_history_with_ids` 在反序列化后 **hydrate**：每个
`ImageRef` 读取 blob → 重新编码回 `ImageB64`；blob 缺失/损坏时降级为
`Text { "[image unavailable]" }`，不会让整次加载失败。
`write_blob` 幂等（文件已存在则跳过 = 内容寻址去重），`read_blob` 读取原始字节。
GC：`rotate_history` 与 `truncate_messages` 写入存活消息后做 mark-and-sweep,
**同时清扫 blob 与描述**——`extend_archived_live_sets` 单次扫描 archive 段填充两个 live 集:
`collect_blob_hashes`(存活消息的 `ImageRef.hash` → `sweep_blobs` 删孤儿 `blobs/*.bin`)
与 `collect_description_keys`(存活消息**全部**媒体 part 的 `content_fingerprint` →
`sweep_descriptions` 删孤儿 `descriptions/*.txt`)。描述键 = 内容指纹 = blob hash(`sha256_hex`),
故描述 live 集是 blob live 集的超集(含内联 `ImageB64`/`ImageUrl`),内联小图描述不会被误删。
archive 段同样被外化。

**结构体** `SessionMeta`:
```rust
priv struct SessionMeta {
  id: String,
  owner: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  display_name: Option<String>,
  created_at: DateTime<Utc>,
  last_activity: DateTime<Utc>,
  /// 1-based line count of the active history.jsonl; used as the next-ID base.
  message_count: usize,
  /// Number of completed rotations; used to name archive files.
  #[serde(default)]
  segment: u32,
  /// Compaction version (0 = never compacted).
  #[serde(default)]
  compact_version: u32,
  /// Token estimate from the last compaction summary, if any.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  compact_token_estimate: Option<u64>,
  /// Last known total token count (input + cached + output) from the API.
  /// Persisted after each response so the value survives restarts.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  last_total_tokens: Option<u64>,
  /// Per-session runtime overrides (JSON-encoded SessionOverride).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  session_override: Option<String>,
  /// Last incoming ChannelMessage. Carries sender / reply_target /
  /// attachments / images so startup recovery can replay the routing
  /// context. RFC v2 §三.A made this the canonical replacement for
  /// the older standalone `last_reply_target` field.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  last_message: Option<crate::channels::ChannelMessage>,
  // ... 2 more fields
}
```

**结构体** `ActiveMap`:
```rust
priv struct ActiveMap {
  #[serde(flatten)]
  map: std::collections::HashMap<String, String>,
}
```

**结构体** `JsonFileBackend`:
```rust
pub struct JsonFileBackend {
  root: PathBuf,
}
```

**Impl** `impl JsonFileBackend`:
```rust
  pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self>
  fn session_dir(&self, session_id: &str) -> PathBuf
  fn meta_path(&self, session_id: &str) -> PathBuf
  fn history_path(&self, session_id: &str) -> PathBuf
  fn archive_dir(&self, session_id: &str) -> PathBuf
  fn active_path(&self) -> PathBuf
  fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()>
  fn read_meta(&self, session_id: &str) -> Option<SessionMeta>
  fn write_meta(&self, meta: &SessionMeta) -> std::io::Result<()>
  fn read_active(&self) -> ActiveMap
  fn write_active(&self, map: &ActiveMap) -> std::io::Result<()>
  fn generate_session_id() -> String
  fn read_history_with_ids(&self, session_id: &str) -> Vec<(i64, ChatMessage)>  // hydrate 后返回
  fn meta_to_info(meta: &SessionMeta) -> SessionInfo
  fn rotate_history_impl(  // 重新 externalize 存活消息 + sweep 孤儿 blob/description
  // ── 图片 blob 存储 ──
  fn blobs_dir(&self, session_id: &str) -> PathBuf
  fn blob_path(&self, session_id: &str, hash: &str) -> PathBuf
  fn write_blob(&self, session_id: &str, hash: &str, bytes: &[u8]) -> std::io::Result<()>  // 幂等去重
  fn read_blob(&self, session_id: &str, hash: &str) -> std::io::Result<Vec<u8>>
  fn externalize(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<ChatMessage>
  fn hydrate(&self, session_id: &str, message: &ChatMessage) -> ChatMessage
  fn sweep_blobs(&self, session_id: &str, live: &HashSet<String>)         // mark-and-sweep blobs/*.bin
  fn sweep_descriptions(&self, session_id: &str, live: &HashSet<String>)  // mark-and-sweep descriptions/*.txt
  fn extend_archived_live_sets(&self, session_id: &str, blob_hashes: &mut HashSet<String>, desc_keys: &mut HashSet<String>)
// free fns: collect_blob_hashes(msg, set) / collect_description_keys(msg, set)
```

**Impl** `impl SessionBackend for JsonFileBackend`:
```rust
  fn create_session(&self, owner: &str, display_name: Option<&str>) -> std::io::Result<SessionInfo>
  fn delete_session(&self, session_id: &str) -> std::io::Result<()>
  fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()>
  fn get_session(&self, session_id: &str) -> Option<SessionInfo>
  fn list_sessions(&self, owner: &str) -> Vec<SessionInfo>
  fn list_all_sessions(&self) -> Vec<SessionInfo>
  fn get_active_session(&self, user_id: &str) -> Option<String>
  fn set_active_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()>
  fn load_messages(&self, session_id: &str) -> Vec<ChatMessage>
  fn append_message(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<i64>
  fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool>
  fn truncate_messages(&self, session_id: &str, keep_count: usize) -> std::io::Result<()>
  fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()>
  fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord>
  fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)>
  fn clear_summary(&self, session_id: &str) -> std::io::Result<()>
  fn rotate_history(
  fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize>
  fn save_token_count(&self, session_id: &str, total: u64) -> std::io::Result<()>
  fn load_token_count(&self, session_id: &str) -> Option<u64>
  fn save_session_override(&self, session_id: &str, json: &str) -> std::io::Result<()>
  fn load_session_override(&self, session_id: &str) -> Option<String>
  fn save_last_message(
  fn load_last_message(&self, session_id: &str) -> Option<crate::channels::ChannelMessage>
  fn save_agent_name(&self, session_id: &str, name: &str) -> std::io::Result<()>
  fn load_agent_name(&self, session_id: &str) -> Option<String>
  fn save_parent_session_id(&self, session_id: &str, parent: &str) -> std::io::Result<()>
  fn load_parent_session_id(&self, session_id: &str) -> Option<String>
```

#### `storage/memory.rs`

**结构体** `MemoryEntry`:
```rust
pub struct MemoryEntry {
  pub id: String,
  pub key: String,
  pub content: String,
  pub category: MemoryCategory,
  pub timestamp: String,
  pub session_id: Option<String>,
  pub score: Option<f64>,
  #[serde(default = "default_namespace")]
  pub namespace: String,
  /// Importance score (0.0–1.0) for prioritized retrieval.
  #[serde(default)]
  pub importance: Option<f64>,
  /// If this entry was superseded by a newer conflicting entry.
  #[serde(default)]
  pub superseded_by: Option<String>,
}
```

```rust
fn default_namespace() -> String
```

**Impl** `impl fmt::Debug for MemoryEntry`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

**枚举** `MemoryCategory`:
```rust
pub enum MemoryCategory {
  /// Long-term facts, preferences, decisions.
  Core,
  /// Daily session logs.
  Daily,
  /// Conversation context.
  Conversation,
  /// User-defined custom category.
  Custom(String),
}
```

**Impl** `impl fmt::Display for MemoryCategory`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

**Impl** `impl Serialize for MemoryCategory`:
```rust
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error>
```

**Impl** `impl Deserialize<'de> for MemoryCategory`:
```rust
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error>
```

**结构体** `ExportFilter`:
```rust
pub struct ExportFilter {
  pub namespace: Option<String>,
  pub session_id: Option<String>,
  pub category: Option<MemoryCategory>,
  /// RFC 3339 lower bound (inclusive) on created_at.
  pub since: Option<String>,
  /// RFC 3339 upper bound (inclusive) on created_at.
  pub until: Option<String>,
}
```

**结构体** `ProceduralMessage`:
```rust
pub struct ProceduralMessage {
  pub role: String,
  pub content: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
}
```

**Trait** `Memory`:
```rust
pub trait Memory {
  fn name(&self) -> &str
  async fn store(
  async fn recall(
  async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>>
  async fn list(
  async fn forget(&self, key: &str) -> anyhow::Result<bool>
  async fn purge_namespace(&self, _namespace: &str) -> anyhow::Result<usize>
  async fn purge_session(&self, _session_id: &str) -> anyhow::Result<usize>
  async fn count(&self) -> anyhow::Result<usize>
  async fn health_check(&self) -> bool
  async fn store_procedural(
  async fn recall_namespaced(
  async fn store_with_metadata(
}
```

#### `storage/mod.rs`

#### `storage/private.rs`

**结构体** `PrivateMemory`:
```rust
pub struct PrivateMemory {
  inner: Arc<dyn Memory>,
  session_id: String,
}
```

**Impl** `impl fmt::Debug for PrivateMemory`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

**Impl** `impl PrivateMemory`:
```rust
  pub fn new(inner: Arc<dyn Memory>, session_id: String) -> Self
  pub fn session_id(&self) -> &str
  fn namespace(&self) -> String
```

**Impl** `impl Memory for PrivateMemory`:
```rust
  fn name(&self) -> &str
  async fn store(
  async fn recall(
  async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>>
  async fn list(
  async fn forget(&self, key: &str) -> anyhow::Result<bool>
  async fn purge_session(&self, _session_id: &str) -> anyhow::Result<usize>
  async fn count(&self) -> anyhow::Result<usize>
  async fn health_check(&self) -> bool
```

#### `storage/session.rs`

**结构体** `SessionInfo`:
```rust
pub struct SessionInfo {
  pub id: String,
  pub owner: String,
  pub display_name: Option<String>,
  pub created_at: DateTime<Utc>,
  pub last_activity: DateTime<Utc>,
  pub message_count: usize,
}
```

**结构体** `SummaryRecord`:
```rust
pub struct SummaryRecord {
  pub id: i64,
  pub version: u32,
  pub summary: String,
  pub up_to_message: i64,
  pub token_estimate: Option<u64>,
  pub created_at: DateTime<Utc>,
}
```

**Trait** `SessionBackend`:
```rust
pub trait SessionBackend {
  fn create_session(&self, owner: &str, display_name: Option<&str>) -> std::io::Result<SessionInfo>
  fn delete_session(&self, session_id: &str) -> std::io::Result<()>
  fn rename_session(&self, session_id: &str, name: &str) -> std::io::Result<()>
  fn get_session(&self, session_id: &str) -> Option<SessionInfo>
  fn list_sessions(&self, owner: &str) -> Vec<SessionInfo>
  fn list_all_sessions(&self) -> Vec<SessionInfo>
  fn get_active_session(&self, user_id: &str) -> Option<String>
  fn set_active_session(&self, user_id: &str, session_id: &str) -> std::io::Result<()>
  fn load_messages(&self, session_id: &str) -> Vec<ChatMessage>
  fn append_message(&self, session_id: &str, message: &ChatMessage) -> std::io::Result<i64>
  fn remove_last_message(&self, session_id: &str) -> std::io::Result<bool>
  fn truncate_messages(&self, _session_id: &str, _keep_count: usize) -> std::io::Result<()>
  fn save_summary(&self, session_id: &str, summary: &SummaryRecord) -> std::io::Result<()>
  fn load_latest_summary(&self, session_id: &str) -> Option<SummaryRecord>
  fn load_incremental(&self, session_id: &str, after_message_id: i64) -> Vec<(i64, ChatMessage)>
  fn clear_summary(&self, session_id: &str) -> std::io::Result<()>
  fn rotate_history(
  fn save_token_count(&self, _session_id: &str, _total: u64) -> std::io::Result<()>
  fn load_token_count(&self, _session_id: &str) -> Option<u64>
  fn save_session_override(&self, _session_id: &str, _json: &str) -> std::io::Result<()>
  fn load_session_override(&self, _session_id: &str) -> Option<String>
  fn save_last_message(
  fn load_last_message(&self, _session_id: &str) -> Option<crate::channels::ChannelMessage>
  fn save_agent_name(&self, _session_id: &str, _name: &str) -> std::io::Result<()>
  fn load_agent_name(&self, _session_id: &str) -> Option<String>
  fn save_parent_session_id(
  fn load_parent_session_id(&self, _session_id: &str) -> Option<String>
  fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize>
}
```

#### `storage/shared.rs`

**结构体** `SharedMemory`:
```rust
pub struct SharedMemory {
  inner: Arc<dyn Memory>,
}
```

**Impl** `impl SharedMemory`:
```rust
  pub fn new(inner: Arc<dyn Memory>) -> Self
  pub fn name(&self) -> &str
```

**Impl** `impl fmt::Debug for SharedMemory`:
```rust
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
```

**Impl** `impl Memory for SharedMemory`:
```rust
  fn name(&self) -> &str
  async fn store(
  async fn recall(
  async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>>
  async fn list(
  async fn forget(&self, key: &str) -> anyhow::Result<bool>
  async fn count(&self) -> anyhow::Result<usize>
  async fn health_check(&self) -> bool
```

#### `storage/types.rs`

**枚举** `SearchMode`:
```rust
pub enum SearchMode {
  /// Pure keyword search (FTS5 BM25)
  Bm25,
  /// Pure vector/semantic search
  Embedding,
  /// Weighted combination of keyword + vector (default)
  #[default]
  Hybrid,
}
```

**结构体** `MemoryPolicyConfig`:
```rust
pub struct MemoryPolicyConfig {
  /// Maximum entries per namespace (0 = unlimited).
  #[serde(default)]
  pub max_entries_per_namespace: usize,
  /// Maximum entries per category (0 = unlimited).
  #[serde(default)]
  pub max_entries_per_category: usize,
  /// Retention days by category (overrides global).
  /// Keys: "core", "daily", "conversation".
  #[serde(default)]
  pub retention_days_by_category: HashMap<String, u32>,
  /// Namespaces that are read-only (writes are rejected).
  #[serde(default)]
  pub read_only_namespaces: Vec<String>,
}
```

**结构体** `MemoryConfig`:
```rust
pub struct MemoryConfig {
  /// "sqlite" | "lucid" | "qdrant" | "markdown" | "none"
  #[serde(default = "default_backend")]
  pub backend: String,
  /// Auto-save user input to memory.
  #[serde(default)]
  pub auto_save: bool,
  /// Run hygiene (archiving + retention cleanup).
  #[serde(default = "default_true")]
  pub hygiene_enabled: bool,
  /// Search strategy.
  #[serde(default)]
  pub search_mode: SearchMode,
  /// Default namespace.
  #[serde(default = "default_namespace")]
  pub default_namespace: String,
  /// Policy configuration.
  #[serde(default)]
  pub policy: MemoryPolicyConfig,
}
```

```rust
fn default_backend() -> String
```

```rust
fn default_true() -> bool
```

```rust
fn default_namespace() -> String
```

```rust
pub fn build_proxy_client(_service_key: &str) -> reqwest::Client
```

**Trait** `Provider`:
```rust
pub trait Provider {
  async fn simple_chat(
  async fn chat_with_system(
}
```

---

## `tools/`

**模块说明**: 内置工具集：文件操作、Shell 执行、Web 搜索/请求、记忆管理、任务管理、代理委派、技能系统

**外部模块依赖**: `agents`, `channels`, `providers`, `str_utils`

#### `tools/agent_kill.rs`

**结构体** `AgentKillTool`:
```rust
pub struct AgentKillTool {
  delegator: Arc<DelegationCoordinator>,
}
```

**Impl** `impl AgentKillTool`:
```rust
  pub fn new(delegator: Arc<DelegationCoordinator>) -> Self
```

**Impl** `impl Tool for AgentKillTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/agent_list.rs`

**结构体** `AgentListTool`:
```rust
pub struct AgentListTool {
  delegator: Arc<DelegationCoordinator>,
}
```

**Impl** `impl AgentListTool`:
```rust
  pub fn new(delegator: Arc<DelegationCoordinator>) -> Self
```

**Impl** `impl Tool for AgentListTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, _args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/ask_user.rs`

```rust
const ASK_USER_TIMEOUT: Duration = Duration::from_secs(300)
```

**结构体** `AskUserTool`:
```rust
pub struct AskUserTool {
  router: Arc<AskRouter>,
}
```

**Impl** `impl AskUserTool`:
```rust
  pub fn new(router: Arc<AskRouter>) -> Self
```

**Impl** `impl Tool for AskUserTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(
```

#### `tools/calculator.rs`

**结构体** `CalculatorTool`:
```rust
pub struct CalculatorTool {
}
```

**Impl** `impl CalculatorTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Default for CalculatorTool`:
```rust
  fn default() -> Self
```

**Impl** `impl Tool for CalculatorTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

```rust
fn eval_math(input: &str) -> Result<f64, String>
```

**枚举** `Token`:
```rust
priv enum Token {
  Number(f64),
  Plus,
  Minus,
  Star,
  Slash,
  Caret,
  LParen,
  RParen,
  Ident(String),
}
```

```rust
fn tokenize(input: &str) -> Result<Vec<Token>, String>
```

```rust
fn parse_expr(tokens: &[Token], pos: &mut usize) -> Result<f64, String>
```

```rust
fn parse_term(tokens: &[Token], pos: &mut usize) -> Result<f64, String>
```

```rust
fn parse_power(tokens: &[Token], pos: &mut usize) -> Result<f64, String>
```

```rust
fn parse_unary(tokens: &[Token], pos: &mut usize) -> Result<f64, String>
```

```rust
fn parse_primary(tokens: &[Token], pos: &mut usize) -> Result<f64, String>
```

```rust
fn test_basic_arithmetic()
```

```rust
fn test_precedence()
```

```rust
fn test_power()
```

```rust
fn test_functions()
```

```rust
fn test_constants()
```

```rust
fn test_complex()
```

```rust
fn test_unary()
```

```rust
fn test_division_by_zero()
```

#### `tools/cronjob_tool.rs`

**结构体** `CronJobTool`:
```rust
pub struct CronJobTool {
  scheduler: SharedScheduler,
}
```

**Impl** `impl CronJobTool`:
```rust
  pub fn new(scheduler: SharedScheduler) -> Self
```

**Impl** `impl Tool for CronJobTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

**Impl** `impl CronJobTool`:
```rust
  fn handle_create(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult>
  fn handle_update(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult>
  fn handle_list(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult>
  fn handle_set_enabled(&self, args: &serde_json::Value, enabled: bool) -> anyhow::Result<ToolResult>
  fn handle_run(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult>
  fn handle_remove(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult>
  fn handle_log(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult>
```

```rust
fn err_result(msg: &str) -> ToolResult
```

```rust
fn format_run_record(i: usize, run: &crate::agents::scheduling::cron_types::RunRecord) -> String
```

```rust
fn parse_schedule_input(input: &str) -> Result<(String, Option<ScheduleKind>), String>
```

```rust
fn parse_duration_to_ms(s: &str) -> Result<u64, String>
```

```rust
fn parse_delivery(value: Option<&serde_json::Value>) -> Option<DeliveryConfig>
```

```rust
fn parse_retry_config(v: &serde_json::Value) -> Option<RetryConfig>
```

```rust
fn parse_failure_alert(v: &serde_json::Value) -> Option<FailureAlertConfig>
```

```rust
fn parse_string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>>
```

#### `tools/delegate.rs`

**结构体** `AgentDelegateTool`:
```rust
pub struct AgentDelegateTool {
  delegator: Arc<dyn AgentDelegator>,
}
```

**Impl** `impl AgentDelegateTool`:
```rust
  pub fn new(delegator: Arc<dyn AgentDelegator>) -> Self
```

**Impl** `impl Tool for AgentDelegateTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/file_ops.rs`

```rust
fn validate_path(path: &str) -> anyhow::Result<std::path::PathBuf>
```

**结构体** `FileReadTool`:
```rust
pub struct FileReadTool {
}
```

**Impl** `impl FileReadTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Tool for FileReadTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

**结构体** `FileWriteTool`:
```rust
pub struct FileWriteTool {
}
```

**Impl** `impl FileWriteTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Tool for FileWriteTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

**结构体** `FileEditTool`:
```rust
pub struct FileEditTool {
}
```

**Impl** `impl FileEditTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Tool for FileEditTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

```rust
fn find_line_number(haystack: &str, needle: &str) -> usize
```

#### `tools/http.rs`

**结构体** `HttpRequestTool`:
```rust
pub struct HttpRequestTool {
  client: reqwest::Client,
}
```

**Impl** `impl HttpRequestTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Default for HttpRequestTool`:
```rust
  fn default() -> Self
```

**Impl** `impl Tool for HttpRequestTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/list_dir.rs`

**结构体** `ListDirTool`:
```rust
pub struct ListDirTool {
}
```

**Impl** `impl ListDirTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Default for ListDirTool`:
```rust
  fn default() -> Self
```

**Impl** `impl Tool for ListDirTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/memory_tool.rs`

```rust
const MAX_CONTENT_CHARS: usize = 10_000
```

```rust
const MAX_SUMMARY_CHARS: usize = 500
```

```rust
const MAX_NAME_LENGTH: usize = 64
```

```rust
fn validate_name(name: &str) -> Result<(), String>
```

```rust
fn scan_entries(memory_dir: &Path) -> Vec<crate::memory::IndexEntry>
```

**结构体** `MemoryPaths`:
```rust
priv struct MemoryPaths {
  memory_dir: PathBuf,
}
```

**Impl** `impl MemoryPaths`:
```rust
  fn for_user(workspace_dir: &Path, user_id: &str) -> Result<Self, String>
```

```rust
fn user_id_for(session: &Session, resolver: &UserResolver) -> String
```

```rust
fn scan_memory_content(content: &str) -> Option<String>
```

```rust
fn scan_memory_content_opt(content: &str) -> Result<(), String>
```

```rust
fn atomic_write(target: &Path, content: &str) -> std::io::Result<()>
```

```rust
fn build_frontmatter(name: &str, summary: &str, tags: &[String], mem_type: &crate::memory::MemoryType, created_at: &str) -> String
```

**结构体** `MemoryListTool`:
```rust
pub struct MemoryListTool {
  workspace_dir: PathBuf,
  resolver: Arc<UserResolver>,
}
```

**Impl** `impl MemoryListTool`:
```rust
  pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self
```

**Impl** `impl Tool for MemoryListTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, _args: serde_json::Value, session: &Session) -> anyhow::Result<ToolResult>
```

**结构体** `MemoryViewTool`:
```rust
pub struct MemoryViewTool {
  workspace_dir: PathBuf,
  resolver: Arc<UserResolver>,
}
```

**Impl** `impl MemoryViewTool`:
```rust
  pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self
```

**Impl** `impl Tool for MemoryViewTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, args: serde_json::Value, session: &Session) -> anyhow::Result<ToolResult>
```

**结构体** `MemorySearchTool`:
```rust
pub struct MemorySearchTool {
  workspace_dir: PathBuf,
  resolver: Arc<UserResolver>,
}
```

**Impl** `impl MemorySearchTool`:
```rust
  pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self
```

**Impl** `impl Tool for MemorySearchTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, args: serde_json::Value, session: &Session) -> anyhow::Result<ToolResult>
```

**结构体** `MemoryManageTool`:
```rust
pub struct MemoryManageTool {
  workspace_dir: PathBuf,
  resolver: Arc<UserResolver>,
}
```

**Impl** `impl MemoryManageTool`:
```rust
  pub fn new(workspace_dir: PathBuf, resolver: Arc<UserResolver>) -> Self
```

**Impl** `impl Tool for MemoryManageTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, args: serde_json::Value, session: &Session) -> anyhow::Result<ToolResult>
```

**Impl** `impl MemoryManageTool`:
```rust
  fn action_add(&self, name: &str, args: &serde_json::Value, user_id: &str) -> Result<serde_json::Value, String>
  fn action_replace(&self, name: &str, args: &serde_json::Value, user_id: &str) -> Result<serde_json::Value, String>
  fn action_remove(&self, name: &str, user_id: &str) -> Result<serde_json::Value, String>
  fn resolve_type(&self, args: &serde_json::Value) -> crate::memory::MemoryType
  fn resolve_summary(&self, args: &serde_json::Value, content: &str) -> String
  fn resolve_tags(&self, args: &serde_json::Value) -> Vec<String>
```

#### `tools/mod.rs`

```rust
pub fn builtin_tools() -> Vec<Arc<dyn Tool>>
```

#### `tools/search.rs`

**结构体** `GlobSearchTool`:
```rust
pub struct GlobSearchTool {
}
```

**Impl** `impl GlobSearchTool`:
```rust
  pub fn new() -> Self
```

```rust
fn glob_to_regex(pattern: &str) -> String
```

```rust
fn walk_dir(dir: &Path, results: &mut Vec<String>, max: usize) -> std::io::Result<()>
```

**Impl** `impl Tool for GlobSearchTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

**结构体** `ContentSearchTool`:
```rust
pub struct ContentSearchTool {
}
```

**Impl** `impl ContentSearchTool`:
```rust
  pub fn new() -> Self
```

```rust
fn search_in_file(path: &Path, re: &regex::Regex, max_lines: usize) -> Option<Vec<String>>
```

**Impl** `impl Tool for ContentSearchTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/search_cooldown.rs`

```rust
const DEFAULT_COOLDOWN_SECS: u64 = 1800
```

**结构体** `SearchProviderCooldown`:
```rust
pub struct SearchProviderCooldown {
  inner: Mutex<HashMap<String, Instant>>,
}
```

**Impl** `impl Default for SearchProviderCooldown`:
```rust
  fn default() -> Self
```

**Impl** `impl SearchProviderCooldown`:
```rust
  pub fn new() -> Self
  pub fn is_cooled_down(&self, provider_name: &str) -> bool
  pub fn record(&self, provider_name: &str, duration: Duration)
  pub fn record_failure_with_cooldown(
  pub fn record_failure(&self, provider_name: &str)
  pub fn classify_and_record(
```

```rust
fn parse_http_error(msg: &str) -> (Option<u16>, &str)
```

```rust
pub fn parse_search_cooldown(body: &str) -> Option<Duration>
```

```rust
fn extract_json_seconds(json: &serde_json::Value, key: &str) -> Option<Duration>
```

```rust
fn extract_regex_seconds(re: &Regex, text: &str) -> Option<Duration>
```

```rust
fn parse_glm_error()
```

```rust
fn parse_google_error()
```

```rust
fn parse_no_status()
```

```rust
fn cooldown_skips_provider()
```

```rust
fn cooldown_extends_never_shortens()
```

```rust
fn record_failure_uses_default_cooldown()
```

```rust
fn record_failure_with_custom_cooldown()
```

```rust
fn parse_cooldown_json_retry_after_field()
```

```rust
fn parse_cooldown_json_retry_after_hyphen()
```

```rust
fn parse_cooldown_json_nested_error_retry_after()
```

```rust
fn parse_cooldown_json_retry_after_as_float()
```

```rust
fn parse_cooldown_json_zero_ignored()
```

```rust
fn parse_cooldown_text_retry_after_seconds()
```

```rust
fn parse_cooldown_text_retry_in_seconds()
```

```rust
fn parse_cooldown_text_try_again()
```

```rust
fn parse_cooldown_text_rate_limit_keywords()
```

```rust
fn parse_cooldown_text_quota_exceeded()
```

```rust
fn parse_cooldown_empty_body()
```

```rust
fn parse_cooldown_no_match()
```

```rust
fn classify_and_record_uses_body_cooldown()
```

```rust
fn classify_and_record_falls_back_to_default_cooldown()
```

#### `tools/shell.rs`

**结构体** `ShellTool`:
```rust
pub struct ShellTool {
}
```

**Impl** `impl ShellTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Tool for ShellTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/skill_manage_tool.rs`

```rust
const MAX_NAME_LENGTH: usize = 64
```

```rust
const MAX_DESCRIPTION_LENGTH: usize = 1024
```

```rust
const MAX_CONTENT_CHARS: usize = 100_000
```

```rust
const MAX_FILE_BYTES: usize = 1_048_576
```

```rust
const ALLOWED_SUBDIRS: &[&str] = &["references", "scripts", "templates", "assets"]
```

**结构体** `SkillManageTool`:
```rust
pub struct SkillManageTool {
  skills: Arc<RwLock<SkillManager>>,
  workspace_dir: PathBuf,
}
```

**Impl** `impl SkillManageTool`:
```rust
  pub fn new(skills: Arc<RwLock<SkillManager>>, workspace_dir: PathBuf) -> Self
```

**Impl** `impl Tool for SkillManageTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

**Impl** `impl SkillManageTool`:
```rust
  fn action_create(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String>
  fn action_edit(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String>
  fn action_patch(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String>
  fn action_delete(&self, name: &str) -> Result<serde_json::Value, String>
  fn action_write_file(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String>
  fn action_remove_file(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String>
  fn get_skill_dir(&self, name: &str) -> Result<PathBuf, String>
  fn refresh_skills(&self)
```

```rust
fn validate_name(name: &str) -> Result<(), String>
```

```rust
fn validate_frontmatter(content: &str, expected_name: &str) -> Result<(), String>
```

```rust
fn validate_content_size(content: &str) -> Result<(), String>
```

```rust
fn validate_supporting_file_path(file_path: &str) -> Result<(), String>
```

```rust
fn validate_patch_file_path(file_path: &str, skill_dir: &Path) -> Result<(), String>
```

```rust
fn atomic_write(target: &Path, content: &str) -> std::io::Result<()>
```

```rust
fn scan_skill_files(dir: &Path) -> HashMap<String, Vec<String>>
```

```rust
fn collect_files_recursive(dir: &Path) -> Vec<PathBuf>
```

```rust
fn setup(workspace: &Path, skill_name: &str) -> Arc<RwLock<SkillManager>>
```

```rust
async fn test_create_and_refresh()
```

```rust
async fn test_create_reserved_name()
```

```rust
async fn test_create_duplicate()
```

```rust
async fn test_name_mismatch()
```

```rust
async fn test_patch_success()
```

```rust
async fn test_patch_not_found()
```

```rust
async fn test_delete()
```

```rust
async fn test_delete_self_rejected()
```

```rust
async fn test_write_and_remove_file()
```

```rust
async fn test_path_traversal_rejected()
```

#### `tools/skill_tool.rs`

**结构体** `SkillTool`:
```rust
pub struct SkillTool {
  skills: Arc<RwLock<SkillManager>>,
}
```

**Impl** `impl SkillTool`:
```rust
  pub fn new(skills: Arc<RwLock<SkillManager>>) -> Self
```

**Impl** `impl Tool for SkillTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

```rust
fn scan_skill_files(dir: &Path) -> HashMap<String, Vec<String>>
```

```rust
fn collect_files_recursive(dir: &Path) -> Vec<PathBuf>
```

```rust
fn substitute_template_vars(content: &str, skill_dir: &Path) -> String
```

```rust
fn make_skill(name: &str, agent_invocable: bool) -> Skill
```

```rust
fn test_skill_tool_spec()
```

```rust
async fn test_execute_known_skill()
```

```rust
async fn test_execute_unknown_skill()
```

```rust
async fn test_execute_non_agent_invocable()
```

```rust
async fn test_path_traversal_rejected()
```

#### `tools/skills_list_tool.rs`

**结构体** `SkillsListTool`:
```rust
pub struct SkillsListTool {
  skills: Arc<RwLock<SkillManager>>,
}
```

**Impl** `impl SkillsListTool`:
```rust
  pub fn new(skills: Arc<RwLock<SkillManager>>) -> Self
```

**Impl** `impl Tool for SkillsListTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  async fn execute(&self, _args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

```rust
fn scan_skill_files(dir: &Path) -> HashMap<String, Vec<String>>
```

```rust
fn collect_files_recursive(dir: &Path) -> Vec<PathBuf>
```

```rust
fn make_skill(name: &str, desc: &str) -> Skill
```

```rust
async fn test_empty_skills()
```

```rust
async fn test_lists_all_skills()
```

```rust
async fn test_non_invocable_fields_included()
```

#### `tools/task.rs`

**结构体** `Task`:
```rust
pub struct Task {
  pub id: String,
  pub parent_id: Option<String>,
  pub subject: String,
  pub description: String,
  pub status: String, // "pending", "in_progress", "completed", "cancelled"
  pub created_at: String,
}
```

**结构体** `TaskState`:
```rust
pub struct TaskState {
  pub tasks: Vec<Task>,
  pub next_id: u32,
}
```

**Impl** `impl TaskState`:
```rust
  fn next_id(&mut self) -> String
  fn find_task(&self, id: &str) -> Option<&Task>
  fn find_task_mut(&mut self, id: &str) -> Option<&mut Task>
  fn collect_descendant_ids(&self, id: &str) -> Vec<String>
```

**结构体** `TaskManagerTool`:
```rust
pub struct TaskManagerTool {
  state: Arc<RwLock<TaskState>>,
}
```

**Impl** `impl TaskManagerTool`:
```rust
  pub fn new(state: Arc<RwLock<TaskState>>) -> Self
  pub fn shared_state() -> Arc<RwLock<TaskState>>
```

**Impl** `impl Tool for TaskManagerTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

**Impl** `impl TaskManagerTool`:
```rust
  async fn handle_create(&self, args: &Value) -> anyhow::Result<ToolResult>
  async fn handle_list(&self, args: &Value) -> anyhow::Result<ToolResult>
  async fn handle_update(&self, args: &Value) -> anyhow::Result<ToolResult>
  async fn handle_delete(&self, args: &Value) -> anyhow::Result<ToolResult>
  async fn handle_progress(&self, args: &Value) -> anyhow::Result<ToolResult>
```

```rust
async fn test_batch_create()
```

```rust
async fn test_batch_create_subtasks()
```

#### `tools/tool_search.rs`

**结构体** `ToolSearchTool`:
```rust
pub struct ToolSearchTool {
  tools: Arc<ToolRegistry>,
}
```

**Impl** `impl ToolSearchTool`:
```rust
  pub fn new(tools: Arc<ToolRegistry>) -> Self
```

**Impl** `impl Tool for ToolSearchTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/truncation.rs`

```rust
pub fn approx_tokens(text: &str) -> usize
```

```rust
pub fn truncate_output(text: &str, max_tokens: usize) -> String
```

```rust
pub fn truncate_tool_result(output: &str, max_tokens: usize) -> String
```

```rust
fn test_short_text_unchanged()
```

```rust
fn test_long_text_truncated()
```

```rust
fn test_preserves_head_and_tail()
```

```rust
fn test_approx_tokens()
```

```rust
fn test_truncate_tool_result()
```

```rust
fn test_boundary_exact_limit()
```

#### `tools/web.rs`

**结构体** `WebFetchTool`:
```rust
pub struct WebFetchTool {
  client: reqwest::Client,
}
```

**Impl** `impl WebFetchTool`:
```rust
  pub fn new() -> Self
```

**Impl** `impl Default for WebFetchTool`:
```rust
  fn default() -> Self
```

```rust
fn strip_html(html: &str) -> String
```

**Impl** `impl Tool for WebFetchTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

#### `tools/web_search.rs`

**结构体** `WebSearchTool`:
```rust
pub struct WebSearchTool {
  registry: Arc<dyn ProviderRegistry>,
  cooldown: Arc<SearchProviderCooldown>,
}
```

**Impl** `impl WebSearchTool`:
```rust
  pub fn new(registry: Arc<dyn ProviderRegistry>, cooldown: Arc<SearchProviderCooldown>) -> Self
```

**Impl** `impl Tool for WebSearchTool`:
```rust
  fn name(&self) -> &str
  fn description(&self) -> &str
  fn parameters_schema(&self) -> serde_json::Value
  fn max_output_tokens(&self) -> usize
  async fn execute(&self, args: serde_json::Value, _session: &crate::agents::session::Session) -> anyhow::Result<ToolResult>
```

---

## `tui/`

**模块说明**: 终端 UI：基于 ratatui 的交互式终端界面

#### `tui/app.rs`

**枚举** `ServerMsg`:
```rust
priv enum ServerMsg {
  #[serde(rename = "chunk")]
  Chunk { delta: String },
  #[serde(rename = "thinking")]
  Thinking { delta: String },
  #[serde(rename = "tool_call")]
  ToolCall { name: String, args: serde_json::Value },
  #[serde(rename = "tool_result")]
  ToolResult { name: String, output: String },
  #[serde(rename = "done")]
  Done { text: String },
  #[serde(rename = "cancelled")]
  Cancelled { partial: String },
  #[serde(rename = "error")]
  Error { message: String },
}
```

**枚举** `AppEvent`:
```rust
priv enum AppEvent {
  Key(event::KeyEvent),
  WsConnected,
  WsMessage(String),
  WsClosed,
  WsError(String),
}
```

**结构体** `ChatLine`:
```rust
priv struct ChatLine {
  prefix: String,
  content: String,
  color: Color,
}
```

**结构体** `App`:
```rust
pub struct App {
  /// WebSocket URL.
  url: String,
  /// True while the event loop should keep running.
  running: bool,
  /// Current connection status string shown in the status bar.
  status: String,
  /// Whether we are connected to the server.
  connected: bool,
  /// Whether we are currently receiving a streamed response.
  streaming: bool,
  /// User input buffer (current line being typed).
  input: String,
  /// Chat history lines for display.
  lines: Vec<ChatLine>,
  /// Current response accumulator (cleared on each new "done"/"cancelled"/"error").
  response_buf: String,
  /// Thinking accumulator.
  thinking_buf: String,
  /// Scroll offset (0 = bottom).
  scroll: u16,
  /// Sender half of WebSocket writer channel.
  ws_tx: Option<mpsc::Sender<String>>,
}
```

**Impl** `impl App`:
```rust
  pub fn new(url: String) -> Self
  pub async fn run(&mut self) -> Result<()>
  fn handle_event(&mut self, ev: AppEvent)
  fn handle_key(&mut self, key: event::KeyEvent)
  fn send_user_input(&mut self)
  fn cancel_current_turn(&mut self)
  fn handle_ws_message(&mut self, raw: &str)
  fn push_line(&mut self, prefix: String, content: String, color: Color)
  fn draw(&self, f: &mut Frame)
  fn draw_status_bar(&self, f: &mut Frame, area: Rect)
  fn draw_messages(&self, f: &mut Frame, area: Rect)
  fn draw_input(&self, f: &mut Frame, area: Rect)
```

```rust
async fn connect_and_run_ws(
```

```rust
fn truncate_str(s: &str, max: usize) -> String
```

#### `tui/mod.rs`

---

## 模块依赖关系图

```
main.rs ──→ daemon.rs ──→ lib.rs (所有模块)
    │
    ├── config/ ←── agents/, channels/, providers/, mcp/, tools/
    │
    ├── agents/ ──→ providers/, channels/, storage/, mcp/, tools/, config/
    │   ├── session/ ──→ storage/
    │   ├── scheduling/ ──→ agents/
    │   ├── workspace/ ──→ config/, mcp/
    │   ├── commands/ ──→ agents/, config/
    │   └── tools/ ──→ providers/, agents/
    │
    ├── channels/ ──→ agents/, config/
    │   ├── telegram/ ──→ channels/
    │   ├── qqbot/ ──→ channels/
    │   └── client/ ──→ channels/, agents/
    │
    ├── providers/ ──→ config/, storage/
    │   └── protocols/ ──→ providers/
    │
    ├── mcp/ ──→ providers/, config/
    │   └── transport/ ──→ mcp/
    │
    ├── tools/ ──→ providers/, agents/, storage/
    │
    ├── storage/ ──→ (独立，无外部依赖)
    │
    ├── registry/ ──→ providers/, config/
    │
    ├── cli/ ──→ daemon/, config/
    │
    └── tui/ ──→ agents/
```
