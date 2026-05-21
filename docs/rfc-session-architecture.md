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
    system_prompt: String,         // AGENT.md body
}

enum ToolFilter {
    All,
    Allow(Vec<String>),
    Deny(Vec<String>),
}
// SkillFilter / McpFilter 同形

struct Agent {
    config: AgentConfig,
    cached_prompt: String,         // 启动时算一次的系统提示词（不含 profile）

    // 共享基础设施引用
    registry: Arc<dyn ServiceRegistry>,
    tool_impls: Arc<ToolRegistry>,
    skills: Arc<RwLock<SkillManager>>,
    context_engine: Arc<ContextEngine>,
}

impl Agent {
    /// 无状态执行一轮 turn。LoopBreaker、运行限制从 ctx.global 读。
    async fn run(
        &self,
        session: &mut Session,
        input: TurnInput,
        ctx: &TurnContext<'_>,
    ) -> Result<TurnResult>;
}
```

"主 agent"和"子代理"是同一个类型的不同实例。

### SessionContext

```rust
struct SessionContext {
    session: Mutex<Session>,
    agent: Arc<Agent>,
    user_profile: Arc<UserProfile>,

    // per-session 状态（跨 turn 保留，不持久化）
    attachments: Mutex<AttachmentManager>,
    pending_retry: Mutex<Option<String>>,

    turn_lock: tokio::sync::Mutex<()>,
}

impl SessionContext {
    /// channel 每轮注入，不存储 — 支持同一 session 跨通道访问
    async fn process_turn(
        &self,
        input: TurnInput,
        channel: Arc<dyn Channel>,
        reply_target: String,
        env: &TurnEnv,
    ) -> Result<String>;
}
```

**职责**：
- 持有 per-session 的对话数据 + attachment 状态
- 保证 turn 串行化（turn_lock）
- 不持有 channel（每轮注入）
- 不持有 AgentLoop / persist_hook（全局共享）

### TurnContext / TurnInput / TurnResult

边界：`SessionContext.process_turn()` 是"配置解析层"，把 SessionOverride / GlobalConfig / Agent.config / UserProfile 解析为标量，组装 system_prompt；`Agent.run()` 是纯执行层，收到的全是已解析值。

```rust
struct TurnContext<'a> {
    // ── 已 resolve 的运行参数 ──
    system_prompt: &'a str,          // 含 builtin + body + profile + runtime + skills
    model_id: &'a str,
    thinking: Option<&'a ThinkingConfig>,
    permission_mode: PermissionMode,
    run_mode: RunMode,
    max_tool_calls: usize,
    tool_timeout_secs: u64,
    stream_first_chunk_timeout_secs: u64,
    max_output_bytes: usize,

    // ── 工具的回弹路径 ──
    channel: &'a dyn Channel,        // ask_user 透传
    reply_target: &'a str,
    ask_router: &'a AskRouter,
    delegator: &'a dyn AgentDelegator,

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

注意 TurnContext **没有 persist**——持久化由 Session 自己负责（见下节）。也没有 `user_profile` / `GlobalConfig` / `SessionOverride` 引用——所有需要的值在 process_turn 解析后以标量传入。

### Session 自负责持久化

Session 内嵌 `Option<Arc<dyn PersistHook>>`，对外只暴露 `add_*` 方法。Agent.run() 不再关心持久化——写 history 即落盘。

```rust
struct Session {
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

    #[serde(skip)]
    persist: Option<Arc<dyn PersistHook>>,    // None = ephemeral（CLI 模式）
}

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

```rust
struct SessionManager {
    backend: Arc<dyn SessionBackend>,
    agents: Arc<AgentRegistry>,
    active: HashMap<String, String>,        // routing_key → session_id
    contexts: HashMap<String, Arc<SessionContext>>,  // session_id → context
}

impl SessionManager {
    fn switch_session(&self, routing_key: &str, session_id: &str) {
        self.active.insert(routing_key, session_id);
        // 天然一致 — context 是 session_id 的函数，与 routing_key 无关
    }

    fn get_context(&self, routing_key: &str) -> Arc<SessionContext>;
    fn create_session(&self, routing_key: &str, agent_name: &str, ...) -> Arc<SessionContext>;
}
```

### SessionOverride

只剩用户交互或触发源真正需要 override 的字段：

```rust
struct SessionOverride {
    run_mode: Option<RunMode>,              // 触发源决定，cron=Background
    permission_mode: Option<PermissionMode>,// 用户在 WebUI/命令改
    model: Option<String>,                  // 用户切模型
    thinking: Option<ThinkingConfig>,       // 跟 model
    isolation: Option<AgentIsolation>,      // 子代理委派时设
}
```

运行参数（max_tool_calls / compact_threshold / retain_work_units 等）全部从 GlobalConfig 读，session 不感知。

### ContextEngine

```rust
struct ContextEngine {
    threshold: f64,
    retain_work_units: usize,
    registry: Arc<dyn ServiceRegistry>,
    // ...
}

impl ContextEngine {
    fn should_compact(&self, session: &Session, last_total_tokens: u64) -> bool;
    async fn compact(&self, session: &mut Session, ...) -> Result<()>;
}
```

`CompactionPolicy` + `CompactionExecutor` 合并为 `ContextEngine`，全局单例。配置从 `GlobalConfig.context` 读，不暴露给 session/agent。

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

```toml
[runtime]
timezone = "Asia/Shanghai"

[limits]
max_tool_calls = 100
max_history = 200
max_output_bytes = 102400
tool_timeout_secs = 180
stream_first_chunk_timeout_secs = 600
loop_breaker_threshold = 3

[context]
compact_threshold = 0.7
retain_work_units = 3

[defaults]
permission_mode = "default"
model = "claude-sonnet-4-6"

[prompt]
max_chars = 0
bootstrap_max_chars = 20000
native_tools = true

# providers / channels / mcp_servers / scheduler 同现状

[users]    # 阶段 4
albert.routing_keys = ["telegram:bot1:12345", "client:webui:any"]
```

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
permission_mode / model / thinking / run_mode:
    SessionOverride > GlobalConfig.defaults

max_tool_calls / context.* / limits.*:
    GlobalConfig（单一来源，不可 per-session override）

tools / skills / mcp / system_prompt:
    AgentConfig（单一来源）

isolation:
    SessionOverride（仅子代理委派时设）
```

---

## 四、Prompt 构建

```rust
fn build_prompt(
    agent: &AgentConfig,
    profile: &UserProfile,
    permission_mode: PermissionMode,
    run_mode: RunMode,
    global: &GlobalConfig,
) -> String {
    [
        builtin_sections(permission_mode, run_mode, global.prompt.native_tools),
        agent.system_prompt.clone(),
        profile.to_prompt_section(),
        runtime_info(),
        skill_instructions(&agent.skills),
    ].into_iter().filter(|s| !s.is_empty()).join("\n\n")
}
```

- Agent 启动时算一次"半成品"（不含 profile），缓存在 `Agent.cached_prompt`
- 每轮 turn 把 profile 拼上去（同 agent 不同用户 prompt 不同）
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
用户消息 → Channel → orchestrator
  → session_manager.get_context(routing_key)
  → ctx.process_turn(input, channel, reply_target, env)
    → turn_lock.lock()
    → agent.run(&mut session, input, &turn_ctx)
```

四条入口路径（用户消息 / heartbeat / cron / webhook / 子代理）全部走同一条调用链，区别只是 sk 生成规则和 SessionOverride 字段：

| 来源 | sk | channel | run_mode |
|------|-----|---------|----------|
| 用户消息 | `{type}:{account}:{sender}` | 来源 channel | Interactive |
| Heartbeat | `_heartbeat_{uuid}` (ephemeral) | last_channel | Background |
| Cron | `cron:{job_id}` | job 配置 target | Background |
| Webhook | `webhook:{job_id}` | job 配置 target | Background |
| 子代理 | `sub:{parent_sk}:{agent}:{uuid}` | 父 session 同 channel | Interactive |

---

## 六、关键改动点

| 组件 | 现在 | 重构后 |
|------|------|--------|
| `Agent` | AgentLoop 工厂 | 无状态执行器，`run(&mut session, ...)` |
| `AgentLoop` | 持有 Session ownership | 删除 |
| `RequestBuilder` | 五职责混杂 struct | 删除（system_prompt 入 Agent，AttachmentManager 入 SessionContext，change_rx 入 WorkspaceWatcher，images 入 TurnInput，build() 改无状态函数） |
| `LoopRegistry` | DashMap 平行缓存 | 删除 |
| `SessionHandle` | Arc<Mutex<AgentLoop>> + Sender | 删除 |
| `run_session_actor` | channel + actor | 删除（turn_lock 替代） |
| `ask_user` handler | per-session 闭包 | `AskRouter` 全局共享 |
| `delegate` handler | per-session 闭包 | `DelegationCoordinator` 全局共享 |
| `persist_hook` | per-AgentLoop 创建，Agent.run() 手动调用 | Session 内嵌（Option<Arc<dyn PersistHook>>），通过 `Session::add_*` 方法自动持久化 |
| `CompactionPolicy` + `CompactionExecutor` | per-AgentLoop | 合并为全局 `ContextEngine` |
| `AttachmentManager` | RequestBuilder 字段 | `SessionContext` 字段 |
| `change_rx` | 传给每个 AgentLoop | `WorkspaceWatcher` 自维护 RwLock |
| `IDENTITY.md` / `SOUL.md` / `RULES.md` | workspace 根目录 | 合并进 main/AGENT.md body |
| `USER.md` | workspace 根目录全局 | per-user UserProfile（阶段 4） |
| `SubAgentConfig` | 与主 agent 配置不同 | 统一为 `AgentConfig` |
| `SubAgentDelegator` | 持 agent 配置 + 临时构建 AgentLoop | `DelegationCoordinator` 编排 worktree，agent 通过 SessionManager 统一执行 |
| `max_tool_calls` 等 limits | AgentConfig + SessionOverride | 仅 `GlobalConfig.limits` |
| `compact_threshold` 等 context | AgentConfig + SessionOverride | 仅 `GlobalConfig.context`（ContextEngine 内部） |
| `permission_mode` / `model` / `thinking` / `run_mode` / `isolation` | AgentConfig + SessionOverride | 仅 `SessionOverride`（agent 不感知） |

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
5. `SessionManager` 增 `contexts: HashMap<sid, Arc<SessionContext>>`，去掉 `cache`
6. `SessionContext.process_turn()` 承担"配置解析层"职责：组装 system_prompt、resolve model/permission_mode/run_mode/thinking、attachment diff，把已解析的标量传入 Agent.run()
7. 拆解 `RequestBuilder`：
   - `system_prompt` 由 `process_turn` 每轮组装，传 `&str` 给 Agent
   - `AttachmentManager` 挪进 `SessionContext.attachments`
   - `change_rx` 删除（WorkspaceWatcher 自维护）
   - pending images 改为 `TurnInput` 字段
   - `build()` / `merge_attachments()` 改为无状态函数
8. 拆 `CompactionPolicy` → 全局 `ContextEngine`（仅持无状态部分）+ `Session.token_tracker`（per-session 状态）
9. 删 `AgentLoop`、`SessionHandle`、`LoopRegistry`、`run_session_actor`
10. `DelegationCoordinator` 接管 worktree / merge / cleanup；`SubAgentDelegator` 删除
11. ClientChannel 删 `loop_registry`、`evict_loop`
12. WorkspaceWatcher 改为自维护（own RwLock，文件变更直接更新）
13. Scheduler / Webhook 路径收编：删 `SchedulerContext` / `WebhookContext`，全走 `session_manager.get_context()`

### 阶段 3：Agent 配置统一

1. `AgentConfig` 简化：仅 `name` / `description` / `tools` / `skills` / `mcp` / `system_prompt`
2. 实现 `ToolFilter` / `SkillFilter` / `McpFilter` enum
3. `GlobalConfig` 拆出 `[limits]` / `[context]` / `[defaults]` / `[prompt]` 段
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
- `config.toml::agent.*` 段（permission_mode / prompt.*）
- `workspace/IDENTITY.md`、`SOUL.md`、`RULES.md`

写：
- `workspace/agents/main/AGENT.md`
  - front matter: `name=main, tools=[all], skills=[all], mcp=[all]`
  - body: `IDENTITY.md + "\n\n" + SOUL.md + "\n\n" + RULES.md`

提示用户手动确认后删除：
- `workspace/IDENTITY.md`、`SOUL.md`、`RULES.md`
- `config.toml::[agent]` 段（合并到 `[defaults]` 和 `[prompt]`）

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
