# RFC: Session 架构重构

> 状态：草案 v2
> 日期：2026-05-21
> 背景：WebUI sessions.switch bug 修复过程中发现的设计问题

---

## 一、当前问题

### 1. AgentLoop 不应持有 Session

`AgentLoop` 通过 ownership 持有 `Session`（history + metadata），而 Session 的生命周期管理（创建/切换/删除）属于更上层的 `SessionManager`。`get_or_create()` 将 Session 从 SessionManager move 进 AgentLoop 后，两层数据分家，外部切 session 时需要 evict 手动同步。

### 2. 不必要的间接层

消息流转经过四层：

```
orchestrator → handle.tx.send(msg) → run_session_actor 从 rx 取出
  → mutex.lock(AgentLoop) → run()
```

`run_session_actor` 只是一个 `while let` 循环，纯粹为了串行化。actor task 拿着 `Arc<Mutex<AgentLoop>>`，但同一时刻只有它自己在用这个 mutex。

### 3. AgentLoop 不需要存在

`AgentLoop` 持有的东西分三类：

| 内容 | 性质 | 应该在 |
|------|------|--------|
| `registry`, `tools`, `skills` | 全局共享的基础设施引用 | Agent |
| `request_builder`, `compactor` | registry/tools 的包装 | 见下文：拆掉 |
| `session` | 对话数据 | SessionContext |
| `loop_breaker`, `policy` | per-turn 计数器 | `run()` 的局部变量 |
| `persist_hook` | 持久化回调 | 全局共享 |

真正 per-turn 的 `LoopBreaker`（tool call 计数）和 `CompactionPolicy`（token 追踪）不需要持久化，作为 `Agent.run()` 的局部变量就行。AgentLoop 整个 struct 不需要存在。

### 4. 主 agent 与子代理配置不统一

- 主 agent 配置散落在 `config.toml` 的 `[agent]` 段
- 子代理配置在 `workspace/agents/<name>/AGENT.md`
- 两者格式不同，字段不对等，构建路径完全不同

### 5. Prompt 构建对主 agent 特化

`SystemPromptBuilder` 硬编码了主 agent 的 prompt 结构（读 IDENTITY.md、SOUL.md、USER.md），子代理只多了个 `identity_header` workaround。

### 6. user_id 名不副实

`user_id` 实际是路由 key（`"telegram:bot1:12345"`），不是真正的用户标识。WebUI 开两个标签页就是两个"用户"，session 互不可见。用户信息（USER.md）是全局共享的，无法 per-user。

### 7. RequestBuilder 职责混杂

`RequestBuilder` 名为"请求构建器"，实际承担五件事：
1. system prompt 字符串存储
2. 热重载监听（`change_rx`）
3. 附件管理（`AttachmentManager`：skill / agent / mcp / memory / date 增量通告）
4. 图片暂存（pending images）
5. 消息列表组装（`build()`）

五件事分属四种生命周期，挤在一个 struct 里。其中只有 AttachmentManager 是真正有状态的 per-session 数据，其他都可以拆。

### 8. 配置参数耦合在 AgentConfig

`max_tool_calls`、`compact_threshold`、`retain_work_units`、`max_history`、`tool_timeout_secs` 等是部署级运行参数，跟 agent 身份无关，但目前混在 AgentConfig 里 per-agent 配置。`permission_mode`、`run_mode`、`model`、`thinking`、`isolation` 是用户/触发源交互层面的临时 override，也不应该写死在 agent 配置里。

---

## 二、目标架构

### 整体结构

```
config.toml (全局配置)
  ├─ [runtime]   timezone
  ├─ [limits]    max_tool_calls, max_history, max_output_bytes,
  │              tool_timeout_secs, stream_first_chunk_timeout_secs,
  │              loop_breaker_threshold
  ├─ [context]   compact_threshold, retain_work_units
  ├─ [defaults]  permission_mode, model
  ├─ [prompt]    max_chars, bootstrap_max_chars, native_tools
  └─ [providers] [channels] [mcp_servers] [scheduler] [users]

daemon.rs (Composition Root)
  │
  ├─ ServiceRegistry        ← 全局共享
  ├─ ToolRegistry           ← 全局共享
  ├─ SkillManager           ← Arc<RwLock>，WorkspaceWatcher 自动更新
  ├─ McpManager             ← 全局共享
  ├─ ContextEngine          ← 全局共享（compaction 引擎）
  ├─ WorkspaceWatcher       ← 自维护，own skills/sub_agents RwLock
  ├─ AskRouter              ← 全局共享（pending_asks 注册器）
  ├─ DelegationCoordinator  ← 全局共享（子代理编排）
  ├─ UserResolver           ← 全局共享（routing_key → user_id）
  │
  ├─ AgentRegistry          ← 加载 workspace/agents/*/AGENT.md
  │
  └─ SessionManager
       └─ SessionContext    ← per session
```

### Agent

```rust
struct AgentConfig {
    name: String,
    description: Option<String>,
    tools: ToolFilter,
    skills: SkillFilter,
    mcp: McpFilter,
    isolation: AgentIsolation,        // 仅作为 sub-agent 被调用时生效
    system_prompt: String,            // AGENT.md body
}

enum ToolFilter {
    All,
    Allow(Vec<String>),
    Deny(Vec<String>),
}
// SkillFilter / McpFilter 同形

// Agent 退化为"纯身份"——无 Arc 字段
struct Agent {
    config: AgentConfig,
    cached_prompt: String,             // AGENT.md body 部分
}

// 全局基础设施 bundle，daemon 启动时构造，run 时传入
struct AgentRuntime {
    registry: Arc<dyn ServiceRegistry>,
    context_engine: Arc<ContextEngine>,
    tool_executor: Arc<ToolExecutor>,    // 内部 ToolRegistry 含所有自带 Arc 的 tool
    loop_breaker: Arc<LoopBreaker>,
}

impl Agent {
    async fn run(
        &self,
        session: &mut Session,
        input: TurnInput,
        ctx: &TurnContext<'_>,
        rt: &AgentRuntime,
    ) -> Result<TurnResult>;
}
```

**Agent 不持有 ask_router / delegator / skills**——这些是工具的内部依赖：
- `AskUserTool` 持 `Arc<AskRouter>`
- `DelegateTool` 持 `Arc<dyn AgentDelegator>`
- daemon 构造 tool 时注入这些 Arc，tool 进 ToolRegistry，Agent 通过 ToolExecutor 间接访问

"主 agent"和"子代理"是同一个类型的不同实例。

### Session 与 SessionContext 的关系

**SessionContext 拥有 Session**。两类不同的关注点：

| 类 | 关注点 | 含字段 |
|----|--------|--------|
| **Session** | 对话**本身**的数据 | history、message_ids、token_tracker、session_override 等；transient backing：persist、channel |
| **SessionContext** | 运行这段对话的**容器** | agent 绑定、attachment 通告状态、pending_retry、turn 串行化 |

类比：Session 是录音，SessionContext 是正在播放它的播放器（含绑定的音箱、播放进度、音量）。录音可以独立存在（list_sessions、序列化磁盘）；播放器只在"打开"录音时存在。

### SessionContext

```rust
struct SessionContext {
    session: Mutex<Session>,           // ← owns
    agent: Arc<Agent>,                 // ref
    user_profile: Arc<UserProfile>,    // ref (per user)

    attachments: Mutex<AttachmentManager>,
    pending_retry: Mutex<Option<String>>,
    turn_lock: tokio::sync::Mutex<()>,
}

impl SessionContext {
    async fn process_turn(
        &self,
        input: TurnInput,
        channel: Arc<dyn Channel>,
        reply_target: String,
        rt: &AgentRuntime,
    ) -> Result<String> {
        let _guard = self.turn_lock.lock().await;
        {
            let mut session = self.session.lock();
            session.channel = Some(channel.clone());           // 透传给工具
            session.last_reply_target = Some(reply_target.clone());
        }
        // attachment diff → merge reminder → agent.run
        ...
    }
    
    async fn recover_pending_turn(&self, rt: &AgentRuntime, ...) -> Result<()>;
}
```

**职责**：
- 拥有 per-session 的对话数据 + attachment 状态
- 保证 turn 串行化（turn_lock）
- 把 channel/reply_target 透传到 Session.channel / Session.last_reply_target，工具直接从 session 读
- 不持有 channel 字段（每轮注入到 session）

### TurnContext / TurnInput / TurnResult

边界：`SessionContext.process_turn()` 是"配置解析层"——组装 system_prompt、resolve model/permission_mode/run_mode/thinking、算 attachment reminder；`Agent.run()` 是纯执行层，收到的全是已解析的标量与少量借用。

```rust
struct TurnContext<'a> {
    // ── 本轮决策（process_turn 解析后传入）──
    system_prompt: &'a str,           // 含 builtin + body + profile + runtime + skills
    model_id: &'a str,
    thinking: Option<&'a ThinkingConfig>,
    permission_mode: PermissionMode,
    run_mode: RunMode,

    // ── 流式（None = Collect 模式）──
    stream: Option<TurnStream<'a>>,
}

struct TurnStream<'a> {
    event_tx: &'a mpsc::Sender<TurnEvent>,
    cancel: &'a CancellationToken,
}

struct TurnInput {
    text: String,
    image_urls: Option<Vec<String>>,
    image_base64: Option<Vec<String>>,
}

struct TurnResult {
    text: String,
    stop_reason: StopReason,
    pending_retry: Option<String>,
}
```

**TurnContext 极简的原因**——所有需要的能力都已收敛到合适的归属：

| 不在 TurnContext 的东西 | 去处 |
|------------------------|------|
| `persist` | Session 内部（transient 字段 + `add_*` 方法） |
| `channel` | Session.channel（transient，process_turn 写入） |
| `reply_target` | Session.last_reply_target（process_turn 写入） |
| `ask_router` | `AskUserTool` 内部 Arc 字段（Agent 不感知） |
| `delegator` | `DelegateTool` 内部 Arc 字段（Agent 不感知） |
| `tool_executor` / `loop_breaker` / `context_engine` | `AgentRuntime` 字段，run() 参数 |
| `tool_timeout` / `max_tool_calls` 等 limit | 各执行器内部 |
| `user_profile` / `AttachmentManager` | process_turn 边界消化（profile 拼进 system_prompt，attachment 算成 reminder） |
| `session_key` | `session.id` |

TurnContext 只保留"本轮 LLM 调用需要的决策 + 流式控制"。

### Session 自负责持久化 + 持有 transient channel

Session 内嵌 `Option<Arc<dyn PersistHook>>` 和 `Option<Arc<dyn Channel>>`（都 `#[serde(skip)]`），对外暴露 `add_*` 方法。Agent.run() 不再关心持久化——写 history 即落盘。工具读 `session.channel` 自己发送。

```rust
struct Session {
    // ── 持久化字段 ──
    id: String,
    owner: String,
    history: Vec<ChatMessage>,
    message_ids: Vec<i64>,
    compact_version: u32,
    summary_metadata: Option<SummaryMetadata>,
    session_override: SessionOverride,
    token_tracker: TokenTracker,
    incomplete_turn: bool,
    last_reply_target: Option<String>,

    // ── transient 运行时 backing（不持久化）──
    #[serde(skip)]
    persist: Option<Arc<dyn PersistHook>>,    // None = ephemeral（CLI 模式）
    #[serde(skip)]
    channel: Option<Arc<dyn Channel>>,        // process_turn 每次写入

impl Session {
    pub fn add_user_text(&mut self, text: String) {
        let msg = ChatMessage::user_text(&text);
        let id = self.persist.as_ref()
            .and_then(|h| h.persist_message(&self.id, &msg).ok())
            .unwrap_or(0);
        self.history.push(msg);
        self.message_ids.push(id);
    }
    pub fn add_assistant_text(&mut self, text: String) { /* 同 pattern */ }
    pub fn add_tool_call(&mut self, ...) { /* 同 */ }
    pub fn add_tool_result(&mut self, ...) { /* 同 */ }
    pub fn save_summary(&mut self, ...) { /* 同 */ }
    pub fn save_override(&mut self) { /* 同 */ }
    pub fn update_token_count(&mut self) { /* 持久化 token_tracker.total */ }
}
```

**收益**：
- Agent.run() 内不可能写了 history 忘记 persist（编译保证）
- TurnContext 不需要 `persist: &'a dyn PersistHook` 字段
- 持久化关注点完全封装在 Session 内部
- CLI / 测试场景 `persist = None`，零成本退化

### SessionManager

单表 `sessions: routing_key → Arc<SessionContext>`。受 **1:1 不变量**约束：表里所有 SessionContext 的 `session.id` 互不重复——一个 session 同时只被一个 routing_key 指向。

```rust
struct SessionManager {
    backend: Arc<dyn SessionBackend>,
    agents: Arc<AgentRegistry>,
    resolver: Arc<UserResolver>,
    sessions: RwLock<HashMap<String, Arc<SessionContext>>>,  // routing_key → ctx
}

impl SessionManager {
    /// 命中返回，否则从 backend 加载并缓存
    fn get_context(&self, routing_key: &str) -> Arc<SessionContext>;

    /// 切换。冲突检查：target_sid 是否被别的 rk 占着
    fn switch_session(&self, routing_key: &str, target_sid: &str) -> Result<(), SessionInUse>;

    /// 显式创建并切换到新 session
    fn create_session(&self, routing_key: &str, agent_name: &str, name: Option<&str>) -> Result<Arc<SessionContext>>;

    /// 子代理 session（不进 sessions 表，调用方持 Arc 管生命周期）
    fn create_sub_session(&self, parent_sid: &str, agent_name: &str) -> Result<Arc<SessionContext>>;

    /// 删除（清空对应 rk 的条目 + backend 删除）
    fn delete_session(&self, routing_key: &str, sid: &str) -> Result<()>;

    /// 列出某 user 的所有 session（含别的 rk 占着的、未加载的）
    fn list_sessions_for_user(&self, user_id: &str) -> Vec<SessionInfo>;
}

#[derive(Debug)]
pub struct SessionInUse {
    pub session_id: String,
    pub held_by: String,    // routing_key
}
```

**为什么单表够用**：
- DelegationCoordinator 拿 parent ctx 信息从 `&Session` 直接取（channel / last_reply_target）
- AskRouter 内部用 session_id 索引 pending oneshot，跟 SessionManager 无关
- 启动恢复从 backend 直接扫，临时构造 ctx 跑完即弃
- 唯一的 sid → rk 反查是 switch_session 的冲突检查，O(n) on 活跃 session 数，低频可接受

**1:1 不变量带来的简化**：
- 不需要 channel.send 的"广播 fallback"——同 sender 多 tab 仍然只走单 listener
- session.channel / session.last_reply_target 不会被并发覆盖
- ask_user 知道发哪个 channel：当前 turn 的发起方 rk 唯一

### SessionOverride

只剩用户交互或触发源真正需要 override 的字段：

```rust
struct SessionOverride {
    run_mode: Option<RunMode>,              // 触发源决定，cron=Background
    permission_mode: Option<PermissionMode>,// 用户在 WebUI/命令改
    model: Option<String>,                  // 用户切模型
    thinking: Option<ThinkingConfig>,       // 跟 model
}
```

不含 `isolation`（agent 字段说了算，委派时由 DelegationCoordinator 直接读 agent.isolation）。运行参数（max_tool_calls / compact_threshold 等）从对应执行器读，session 不感知。

### 全局执行器：ContextEngine / ToolExecutor / LoopBreaker

每个执行器持有自己的配置（从对应 GlobalConfig section 反序列化），daemon 启动时构造为全局 Arc。

```rust
// CompactionPolicy + CompactionExecutor 合并
struct ContextEngine {
    compact_threshold: f64,
    retain_work_units: usize,
    registry: Arc<dyn ServiceRegistry>,
    tools: Arc<ToolRegistry>,
    // ...
}

impl ContextEngine {
    fn should_compact(&self, session: &Session, last_total_tokens: u64) -> bool;
    async fn compact(&self, session: &mut Session, ...) -> Result<()>;
}

// DefaultToolExecutor 重命名 + 简化
struct ToolExecutor {
    tools: Arc<ToolRegistry>,
    timeout: Duration,                  // from [tool_executor].timeout_secs
}

impl ToolExecutor {
    async fn execute(&self, call: &ToolCall, session: &mut Session, ...) -> Result<ToolResult>;
}

// LoopBreaker 拆为全局 policy + per-turn counter
struct LoopBreaker {
    max_tool_calls: usize,              // from [loop_breaker].max_tool_calls
    threshold: usize,                   // from [loop_breaker].threshold
}

impl LoopBreaker {
    fn new_counter(&self) -> LoopBreakerCounter;  // Agent.run() 每轮调一次
}

struct LoopBreakerCounter<'a> {
    policy: &'a LoopBreaker,
    tool_count: usize,
    recent: VecDeque<String>,
}
```

LLM 流式读取**不需要 struct**——常量 + 函数，挂在模块上：

```rust
// src/agents/llm_stream.rs
const FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

pub async fn read(stream: BoxStream<StreamEvent>) -> Result<CollectedResponse>;
pub async fn read_streamed(
    stream: BoxStream<StreamEvent>,
    event_tx: &mpsc::Sender<TurnEvent>,
    cancel: &CancellationToken,
) -> Result<CollectedResponse>;
```

`first_chunk_timeout` 和 `max_output_bytes` 是安全网（防 LLM 卡死/无限输出），不暴露为配置。

### WorkspaceWatcher（自维护）

```rust
struct WorkspaceWatcher {
    skills: Arc<RwLock<SkillManager>>,
    sub_agents: Arc<RwLock<Vec<AgentConfig>>>,
    // 内部 spawn task 监听 workspace/skills/ 和 workspace/agents/ 变化，
    // 直接更新 RwLock
}
```

不再让每个 AgentLoop 各持一份 `change_rx`。Agent.run 读 RwLock 时自动看到最新值。AttachmentManager 在下一轮 turn 算 diff 时自然处理"通告变化"。

### UserResolver (阶段 4)

```rust
struct UserResolver {
    explicit: HashMap<String, String>,  // routing_key → user_id（来自 config.toml）
}

impl UserResolver {
    fn resolve(&self, routing_key: &str) -> String {
        self.explicit.get(routing_key).cloned()
            .unwrap_or_else(|| routing_key.to_string())
    }
}
```

未配置映射时 user_id = routing_key，行为等价现状。

**与 1:1 不变量的关系**：UserResolver **不打通"多端共享同一 active session"**——同一 user 的 rk_telegram 和 rk_webui 仍然各有各的 active session。user_id 只用于：
- 列 session 列表（`list_sessions_for_user(uid)` 返回 owner = uid 的全部 session，跨 rk 可见）
- Memory 路径（`workspace/users/{uid}/memory/`）
- UserProfile 加载
- 不用于 active 表的 key——active 仍然按 rk

跨端"接管同一 session"是显式动作：用户在 webui 看到的 session 列表里点"接管"，触发 `switch_session(rk_webui, sid)`——这一步会强制清掉 rk_telegram 的 active（如果它正占着 sid），然后切到 sid。

### WebUI sender 要求

WebUI 必须和其他 channel 一样有**稳定的 sender 身份**——sender 就是 auth token：

```
WebSocket auth 消息：
  { "type": "auth", "token": "..." }

verify_token(token) 通过后：
  sender = "web:{token}"  // 或 hash(token) 避免日志泄漏
  routing_key = "client:default:{sender}"
```

- **同一用户的多 tab 共享 token** → 共享 routing_key → 共享 SessionContext → turn_lock 串行保护
- **不同用户用不同 token** → 不同 routing_key → 不同 session 视图
- 不再支持"匿名连接 / per-conn id"——auth message 缺 token 直接拒绝

实现层面 `ClientChannel` 内部仍然维护 `connections: DashMap<conn_id, ws_sender>` 用于具体 WS 投递，但**对外（向 Orchestrator）只暴露 sender 这一层**。多 tab 同 sender → 多个 conn_id → channel.send 找最近活跃 conn 投递；turn 流式事件由发起 turn 的 conn 接收（基于 pending_streams: sender → (conn_id, StreamContext)）。

### UserProfile (阶段 4)

```rust
struct UserProfile {
    id: String,
    name: Option<String>,
    timezone: Option<String>,
    language: Option<String>,
    preferences: HashMap<String, String>,
    free_form: String,                  // profile.md body
}
```

存储：
```
workspace/users/{user_id}/
  profile.md          ← 基本身份信息（替代 USER.md）
  preferences.md      ← 积累的偏好
  memory/             ← memory 目录 per-user
```

加载顺序（高→低优先级）：
1. `workspace/users/{id}/profile.md`
2. `workspace/users/{id}/preferences.md`
3. channel 元数据（Telegram first_name 等）
4. `workspace/DEFAULT_USER.md`

---

## 三、配置文件分层

### config.toml（全局，部署级）

按**模块**组织，每个 section 对应一个组件，daemon 直接整体反序列化为该组件的 config struct，无跨段拼装。

```toml
[locale]
timezone = "Asia/Shanghai"

[prompt]
max_chars = 0
bootstrap_max_chars = 20000
native_tools = true

[agent]
permission_mode = "default"          # 全局 agent 行为默认（SessionOverride 可覆盖）

[tool_executor]
timeout_secs = 180

[loop_breaker]
max_tool_calls = 100
threshold = 3

[context_engine]
compact_threshold = 0.7
retain_work_units = 3

[providers]
# 各 provider 注册顺序决定隐式默认 model（第一个 chat-capable）

[channels]
# ...

[mcp_servers]
# ...

[scheduler]
# ...

[users]    # 阶段 4
albert.routing_keys = ["telegram:bot1:12345", "client:webui:any"]
```

**不出现的旧字段**：
- `max_history` — 死代码，删
- `first_chunk_timeout` / `max_output_bytes` — 硬编码为安全网常量
- `[defaults].model` — provider 注册顺序提供隐式默认

### AGENT.md（per-agent，能力定义）

```markdown
---
name: coder
description: "Expert programmer for writing and editing code"
tools: [shell, file_read, file_write, file_edit]
skills: [code-review]
mcp: [github, filesystem]
---

You are an expert programmer. Write clean, idiomatic code.

## Behavioral Principles

Be concise. Don't over-engineer.
```

含 permission_mode 与 isolation 字段：
```markdown
---
name: coder
description: "Expert programmer"
tools: [shell, file_read, file_write, file_edit]
skills: [code-review]
mcp: [github, filesystem]
isolation: worktree                # 仅作为 sub-agent 时生效
---

You are an expert programmer...
```

主 agent：
```markdown
---
name: main
tools: [all]
skills: [all]
mcp: [all]
---

(IDENTITY.md + SOUL.md + RULES.md 合并)
```

### 三层解析

```
permission_mode:
    SessionOverride > [agent].permission_mode > 内置 PermissionMode::Default

model:
    SessionOverride > providers 注册顺序的第一个 chat-capable

thinking / run_mode:
    SessionOverride > 内置默认

tool_timeout / max_tool_calls / threshold / compact_threshold / retain_work_units:
    各执行器的 config struct（GlobalConfig 单一来源，无 override）

tools / skills / mcp / isolation / system_prompt:
    AgentConfig（单一来源，AGENT.md 决定）
```

---

## 四、Prompt 构建

由 `SessionContext.process_turn()` 在每轮 turn 开始时调用：

```rust
fn build_prompt(
    agent: &AgentConfig,
    profile: &UserProfile,
    permission_mode: PermissionMode,
    run_mode: RunMode,
    prompt_cfg: &PromptConfig,
) -> String {
    [
        builtin_sections(permission_mode, run_mode, prompt_cfg.native_tools),
        agent.system_prompt.clone(),
        profile.to_prompt_section(),
        runtime_info(),
        skill_instructions(&agent.skills),
    ].into_iter().filter(|s| !s.is_empty()).join("\n\n")
}
```

- Agent 启动时算一次"半成品"（仅 `agent.system_prompt` 部分），缓存在 `Agent.cached_prompt`
- 每轮 turn 由 process_turn 把 profile + builtin（按当前 perm/run mode）+ runtime 拼上
- 完全去掉 IDENTITY.md / SOUL.md / USER.md / RULES.md 的硬编码读取

---

## 五、消息流转对比

### 现在

```
用户消息 → Channel → orchestrator
  → get_or_create(sk) 拿 SessionHandle
    ├─ SessionManager.get_or_create(sk)
    ├─ agent.loop_for_with_persist(session)
    ├─ 绑 ask_user/delegate handler 闭包
    ├─ spawn run_session_actor
    └─ 存进 DashMap
  → handle.tx.send(TurnMessage)
  → run_session_actor → run_message_task → loop_.lock().await → AgentLoop.run()
```

### 重构后

```
用户消息 / cron / heartbeat / webhook
    → Channel/Scheduler/WebhookHandler （仅做协议解码）
    → mpsc::send(OrchestratorEvent)
    → Orchestrator.run() 主循环
        → session_manager.get_context(routing_key)
        → ctx.process_turn(input, channel, reply_target, rt)
            → turn_lock.lock()
            → agent.run(&mut session, input, &turn_ctx, rt)

子代理委派（特例）：
  agent.run() 内 → DelegateTool.execute(args, &session)
    → DelegationCoordinator.delegate(name, task, &session)
      ├─ 从 &session 取 channel / last_reply_target
      ├─ session_manager.create_sub_session(parent_sid, agent_name) → Arc<SessionContext>
      │   （sub ctx 不进 sessions 表，由调用方持 Arc 管生命周期）
      └─ sub_ctx.process_turn(task, channel, reply_target)
  （DelegationCoordinator 就近调，不绕 Orchestrator）
```

`OrchestratorEvent` 是统一的事件类型：

```rust
enum OrchestratorEvent {
    UserMessage(ChannelMessage, ChannelKey),    // 各 channel 用户消息
    Scheduler(SchedulerEvent),                  // cron / heartbeat
    Webhook(WebhookEvent),                      // HTTP webhook 触发
    Delegation(DelegationEvent),                // 子代理完成回填
}
```

四条入口路径走同一条调用链，区别只是 sk 生成规则和 SessionOverride 字段：

| 来源 | 进入 Orchestrator 的事件 | sk | channel | run_mode |
|------|------------------------|-----|---------|----------|
| 用户消息 | `UserMessage` | `{type}:{account}:{sender}` | 来源 channel | Interactive |
| Heartbeat | `Scheduler` | `_heartbeat:{uuid}` (ephemeral) | last_channel | Background |
| Cron | `Scheduler` | `cron:{job_id}` | job 配置 target | Background |
| Webhook | `Webhook` | `webhook:{job_id}` | job 配置 target | Background |
| 子代理 | （不进 Orchestrator） | `sub:{parent_sk}:{agent}:{uuid}` | 父 session 同 channel | Interactive |

**WebhookHandler 退化为协议适配器**：跟 TelegramChannel 同位，只负责签名校验 + 模板渲染 + 把 `WebhookEvent` 发给 Orchestrator。原来直接持 `Arc<SessionManager>` 的旁路删除。

---

## 六、关键改动点

| 组件 | 现在 | 重构后 |
|------|------|--------|
| `Agent` | AgentLoop 工厂 + 大量 Arc 字段 | 纯身份（config + cached_prompt），无 Arc 字段 |
| `AgentLoop` | 持有 Session ownership | 删除 |
| `AgentRuntime` | （不存在） | 新增，bundle 全局基础设施（registry/context_engine/tool_executor/loop_breaker），Agent.run() 参数 |
| `RequestBuilder` | 五职责混杂 struct | 删除（system_prompt 入 process_turn，AttachmentManager 入 SessionContext，change_rx 入 WorkspaceWatcher，images 入 TurnInput，build() 改无状态函数） |
| `LoopRegistry` | DashMap 平行缓存 | 删除 |
| `SessionHandle` | Arc<Mutex<AgentLoop>> + Sender | 删除 |
| `run_session_actor` | channel + actor | 删除（turn_lock 替代） |
| `ask_user` handler | per-session 闭包 | `AskUserTool` 内部持 `Arc<AskRouter>`，Agent 不感知 |
| `delegate` handler | per-session 闭包 | `DelegateTool` 内部持 `Arc<dyn AgentDelegator>`，Agent 不感知 |
| `persist_hook` | per-AgentLoop 创建，Agent.run() 手动调用 | Session 内嵌（transient `Option<Arc<dyn PersistHook>>`），通过 `Session::add_*` 方法自动持久化 |
| `Session.channel` | （不存在） | 新增 transient 字段（`Option<Arc<dyn Channel>>`），process_turn 写入，工具直接读取 |
| `SessionManager.cache` + `LoopRegistry.sessions` | 双层缓存（rk→sid + sid→AgentLoop），需 evict 同步 | 单表 `sessions: HashMap<rk, Arc<SessionContext>>` + **1:1 不变量**（不允许 rk 共享 sid） |
| `WebhookContext` | 独立持 SessionManager + sessions 等 | 删除；`WebhookHandler` 退化为协议适配器，发 `WebhookEvent` 给 Orchestrator |
| WebUI sender | 可选 client_id 或 per-conn id | 必须 = auth token，1:1 对应 user |
| `CompactionPolicy` + `CompactionExecutor` | per-AgentLoop | 合并为全局 `ContextEngine` |
| `AttachmentManager` | RequestBuilder 字段 | `SessionContext` 字段 |
| `change_rx` | 传给每个 AgentLoop | `WorkspaceWatcher` 自维护 RwLock |
| `IDENTITY.md` / `SOUL.md` / `RULES.md` | workspace 根目录 | 合并进 main/AGENT.md body |
| `USER.md` | workspace 根目录全局 | per-user UserProfile（阶段 4） |
| `SubAgentConfig` | 与主 agent 配置不同 | 统一为 `AgentConfig` |
| `SubAgentDelegator` | 持 agent 配置 + 临时构建 AgentLoop | `DelegationCoordinator` 编排 worktree，agent 通过 SessionManager 统一执行 |
| `max_tool_calls` 等 limits | AgentConfig + SessionOverride | 仅 `GlobalConfig.limits` |
| `tool_timeout_secs` | `AgentConfig.tool_timeout_secs` | `ToolExecutor` 字段（来自 `[tool_executor]`） |
| `max_tool_calls` / `loop_breaker_threshold` | `AgentConfig` + SessionOverride | `LoopBreaker` 字段（来自 `[loop_breaker]`） |
| `compact_threshold` / `retain_work_units` | `AgentConfig.context` + SessionOverride | `ContextEngine` 字段（来自 `[context_engine]`） |
| `stream_first_chunk_timeout_secs` / `max_output_bytes` | `AgentConfig` | **硬编码常量**（`llm_stream` 模块） |
| `max_history` | `AgentConfig` | **删除**（死代码） |
| `permission_mode` | `AgentConfig` + SessionOverride | `[agent].permission_mode` + SessionOverride 覆盖 |
| `model` | `AgentConfig` + SessionOverride | SessionOverride + providers 隐式默认（无全局 [defaults]） |
| `thinking` | `AgentConfig` + SessionOverride | 仅 `SessionOverride` |
| `run_mode` | SessionOverride | 仅 `SessionOverride`（触发源决定） |
| `isolation` | SessionOverride | 仅 `AgentConfig`（agent 身份） |

---

## 七、实施策略

### 阶段 1：AgentLoop 解耦 Session

1. `Agent.run()` / `AgentLoop.run()` 接收 `&mut Session` 参数，不再从 `self.session` 读取
2. ~57 处 `self.session` → `session` 机械替换（run.rs 22 处，compaction.rs 32 处，tools.rs 3 处）
3. `persist_hook` 改为 `run()` 参数
4. `AgentLoop.session` 字段删除

可独立部署，顺带消除 `evict_loop` workaround 的根因。

### 阶段 2：SessionContext + 去间接层 + 拆解 RequestBuilder

1. 定义 `TurnContext` / `TurnInput` / `TurnResult` / `TurnStream`
2. 引入 `AskRouter`、`AgentDelegator` trait
3. Session 内嵌 `persist: Option<Arc<dyn PersistHook>>`，加 `add_user_text` / `add_assistant_text` / `add_tool_*` / `save_summary` / `update_token_count` 等方法。`Session.token_tracker` 替代 `last_total_tokens` 单字段
4. 新建 `SessionContext`（不存 channel；持 `AttachmentManager` 和 `pending_retry`）
5. `SessionManager` 单表 `sessions: HashMap<routing_key, Arc<SessionContext>>`，去掉 `cache`；加 1:1 不变量（switch_session 时检查目标 sid 未被别的 rk 占着，否则返回 SessionInUse 错误）
6. `SessionContext.process_turn()` 承担"配置解析层"职责：组装 system_prompt、resolve model/permission_mode/run_mode/thinking、attachment diff，把已解析的标量传入 Agent.run()
7. 拆解 `RequestBuilder`：
   - `system_prompt` 由 `process_turn` 每轮组装，传 `&str` 给 Agent
   - `AttachmentManager` 挪进 `SessionContext.attachments`
   - `change_rx` 删除（WorkspaceWatcher 自维护）
   - pending images 改为 `TurnInput` 字段
   - `build()` / `merge_attachments()` 改为无状态函数
8. 拆 `CompactionPolicy` + `CompactionExecutor` → 全局 `ContextEngine`（持 compact_threshold/retain_work_units）+ `Session.token_tracker`（per-session 状态）
9. `DefaultToolExecutor` 重命名为 `ToolExecutor`，timeout 内化为字段
10. 拆 `LoopBreaker` 为全局 policy + per-turn counter，配置从 `[loop_breaker]` 读
11. `LlmResponseReader` 退化为 `llm_stream::read` 模块函数 + 硬编码常量
12. 删 `AgentLoop`、`SessionHandle`、`LoopRegistry`、`run_session_actor`
13. `DelegationCoordinator` 接管 worktree / merge / cleanup；`SubAgentDelegator` 删除
14. ClientChannel 删 `loop_registry`、`evict_loop`
15. WorkspaceWatcher 改为自维护（own RwLock，文件变更直接更新）
16. Scheduler / Webhook 路径收编：删 `SchedulerContext` / `WebhookContext`，全走 `session_manager.get_context()`
17. 删 `AgentConfig.max_history`（死代码）

### 阶段 3：Agent 配置统一

1. `AgentConfig` 简化：仅 `name` / `description` / `tools` / `skills` / `mcp` / `isolation` / `system_prompt`
2. 实现 `ToolFilter` / `SkillFilter` / `McpFilter` enum
3. `GlobalConfig` 按模块拆段：`[locale]` / `[prompt]` / `[agent]` / `[tool_executor]` / `[loop_breaker]` / `[context_engine]` / `[providers]` / `[channels]` / `[mcp_servers]` / `[scheduler]` / `[users]`
4. `AgentRegistry` 加载所有 `workspace/agents/*/AGENT.md`（含 main）
5. `prompt.rs` 删 IDENTITY/SOUL/USER.md 读取；prompt 组合改为参数化（`build_prompt(...)` 函数）
6. 启动校验：`workspace/agents/main/AGENT.md` 缺失则报错退出
7. 提供 `scripts/migrate_main_agent.sh` 一次性脚本

### 阶段 4：用户信息 per-user

1. `UserResolver`：routing_key → user_id 映射（默认透传）
2. `UserProfile` 加载/序列化/`to_prompt_section`
3. `SessionContext` 加 `user_profile: Arc<UserProfile>`
4. `build_prompt` 补齐 profile section
5. Memory tools 读写 `workspace/users/{id}/memory/`
6. 提供 `scripts/migrate_memory.sh`（默认目标 `users/_default/memory/`）
7. 提供 `scripts/migrate_user_profile.sh`（USER.md → `users/_default/profile.md`）

阶段 3 和 4 可并行。

---

## 八、迁移脚本

### scripts/migrate_main_agent.sh

读：
- `config.toml`（旧布局：`[agent]` / `[limits]` / `[context]` / `[defaults]` / `[prompt]` 等扁平段）
- `workspace/IDENTITY.md`、`SOUL.md`、`RULES.md`

写：
- 重写 `config.toml` 为新的按模块布局
  - `[locale]` / `[prompt]` / `[agent]` / `[tool_executor]` / `[loop_breaker]` / `[context_engine]`
  - 删除 `max_history` / `stream_first_chunk_timeout_secs` / `max_output_bytes` 字段
- `workspace/agents/main/AGENT.md`
  - front matter: `name=main, tools=[all], skills=[all], mcp=[all]`
  - body: `IDENTITY.md + "\n\n" + SOUL.md + "\n\n" + RULES.md`

提示用户手动确认后删除：
- `workspace/IDENTITY.md`、`SOUL.md`、`RULES.md`

### scripts/migrate_memory.sh

```
workspace/memory/  →  workspace/users/_default/memory/
```

未在 `config.toml::[users]` 配置映射的 routing_key 走 `_default`，等价原全局共享。

### scripts/migrate_user_profile.sh

```
workspace/USER.md  →  workspace/users/_default/profile.md
```

格式转换：纯 Markdown → YAML front matter + body。

---

## 当前 workaround

在阶段 1 完成前，session switch/create/delete 的 API handler 通过 evict LoopRegistry 保证一致性（commit `a7a31ff`）。阶段 1 落地后这段代码删除。
