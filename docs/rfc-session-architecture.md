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
| `persist_hook` | 持久化回调 | Session 内部 transient 字段 + `add_*` 方法 |

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
config.toml (全局配置 — 按模块分段)
  ├─ [locale]   timezone
  ├─ [prompt]   max_chars / bootstrap_max_chars / native_tools
  ├─ [agent]    permission_mode (全局默认)
  ├─ [tool_executor]   timeout_secs
  ├─ [loop_breaker]    max_tool_calls / threshold
  ├─ [context_engine]  compact_threshold / retain_work_units
  └─ [providers] [channels] [mcp_servers] [scheduler] [users]

daemon.rs (Composition Root)
  │
  ├─ ProviderRegistry       ← 全局共享（LLM providers，原 ServiceRegistry 改名）
  ├─ ToolRegistry           ← 全局工具池（注入各种 Arc 后注册的 tools）
  ├─ SkillManager           ← Arc<RwLock>，WorkspaceWatcher 自动更新
  ├─ McpManager             ← 全局共享
  ├─ ContextEngine          ← 全局（compaction）
  ├─ ToolExecutor           ← 全局（timeout 包装器）
  ├─ LoopBreaker            ← 全局（max_tool_calls / threshold）
  ├─ AskRouter              ← 全局（pending oneshot 注册器，注入给 AskUserTool）
  ├─ DelegationCoordinator  ← 全局（实现 AgentDelegator，注入给 DelegateTool）
  ├─ UserResolver           ← 全局（routing_key → user_id）
  ├─ WorkspaceWatcher       ← 全局，自维护 SkillManager + AgentRegistry
  │
  ├─ AgentRegistry          ← Arc<RwLock<HashMap<name, Arc<Agent>>>>
  │                           加载 workspace/agents/*/AGENT.md
  │                           WorkspaceWatcher 文件变更时重建并 swap
  │
  ├─ AgentRuntime           ← bundle 上面的全局执行器，给 Agent.run() 用
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

impl ToolFilter {
    fn allows(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allow(list) => list.iter().any(|n| n == name),
            Self::Deny(list) => list.iter().all(|n| n != name),
        }
    }
}

impl AgentConfig {
    /// 三维独立过滤：按来源决定查哪个 filter
    fn allows_tool(&self, tool: &dyn Tool) -> bool {
        match tool.source() {
            ToolSource::Builtin => self.tools.allows(tool.name()),
            ToolSource::McpServer(server) => self.mcp.allows(&server),
            ToolSource::Skill(skill) => self.skills.allows(&skill),
        }
    }
}

// Agent 退化为"纯身份"——仅一个 config 字段
struct Agent {
    config: AgentConfig,
}

// 全局基础设施 bundle，daemon 启动时构造，run 时传入
struct AgentRuntime {
    provider_registry: Arc<dyn ProviderRegistry>,    // LLM 调用（原 ServiceRegistry 改名）
    tool_registry: Arc<ToolRegistry>,                // 全局工具池（含 builtin + MCP + skill tools）
    skills: Arc<RwLock<SkillManager>>,               // 给 build_prompt 与 attachment diff
    agents: Arc<AgentRegistry>,                      // 给 attachment diff 与 DelegationCoordinator
    context_engine: Arc<ContextEngine>,
    tool_executor: Arc<ToolExecutor>,                // 仅 timeout 包装器
    loop_breaker: Arc<LoopBreaker>,
    defaults: RuntimeDefaults,                       // runtime 还需要的 GlobalConfig 子集
}

/// 从 config.toml 的 [agent] / [prompt] 段抽出的"运行时还要读"的值
#[derive(Clone)]
struct RuntimeDefaults {
    permission_mode: PermissionMode,                 // [agent].permission_mode (SessionOverride fallback)
    prompt: PromptConfig,                            // [prompt] 段
}

#[derive(Clone)]
struct PromptConfig {
    max_chars: usize,
    bootstrap_max_chars: usize,
    native_tools: bool,
}

impl Agent {
    /// Agent.run() 不接收单独的 input 参数——
    /// 本轮 user message 已经被 process_turn push 进 session.history，
    /// channel/reply_target/images 等都在 session.last_message 与 session.channel
    async fn run(
        &self,
        session: &mut Session,
        ctx: &TurnContext<'_>,
        rt: &AgentRuntime,
    ) -> Result<TurnResult>;
}
```

**Agent.run() turn 起手做一次工具过滤**：从全局 ToolRegistry 过出当前 agent 允许的子集，转 spec 传给 LLM，turn 内全程用同一份子集：

```rust
async fn run(...) {
    let allowed_tools: Vec<Arc<dyn Tool>> = rt.tool_registry.all()
        .into_iter()
        .filter(|t| self.config.allows_tool(t.as_ref()))
        .collect();
    let tool_specs: Vec<ToolSpec> = allowed_tools.iter().map(|t| t.spec()).collect();
    
    loop {
        // LLM 只看到 tool_specs（agent 的允许子集）
        let response = call_llm(messages, &tool_specs).await?;
        for call in response.tool_calls {
            let result = rt.tool_executor
                .execute(&call, &session, &allowed_tools)
                .await?;
        }
    }
}
```

**双层防御**：
- 第一层：tool_specs 只含允许工具，LLM 不知道有别的
- 第二层：ToolExecutor 在 `allowed_tools` 子集内查名，越权调用直接报错

**Agent 不持有 ask_router / delegator / skills**——这些是工具的内部依赖：
- `AskUserTool` 持 `Arc<AskRouter>`
- `DelegateTool` 持 `Arc<dyn AgentDelegator>`
- daemon 构造 tool 时注入这些 Arc，tool 进 ToolRegistry

### Tool trait

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    fn source(&self) -> ToolSource;        // 让 filter 知道按哪类规则过滤
    async fn execute(&self, args: Value, session: &Session) -> Result<ToolResult>;
}

enum ToolSource {
    Builtin,
    McpServer(String),    // server name，e.g., "github"
    Skill(String),        // skill name
}
```

`&Session`（不可变）就够——工具不直接 mutate session，结果以 `ToolResult` 返回，由 Agent 调 `session.add_tool_result(...)` 写进 history。

### ToolExecutor 退化为 timeout 包装器

```rust
struct ToolExecutor {
    timeout: Duration,
    // 不持 ToolRegistry — 工具池由 Agent 传入
}

impl ToolExecutor {
    async fn execute(
        &self,
        call: &ToolCall,
        session: &Session,
        allowed: &[Arc<dyn Tool>],
    ) -> Result<ToolResult> {
        let tool = allowed.iter()
            .find(|t| t.name() == call.name)
            .ok_or_else(|| anyhow!("tool '{}' not in agent's allowed set", call.name))?;
        tokio::time::timeout(self.timeout, tool.execute(call.args.clone(), session))
            .await
            .map_err(|_| anyhow!("tool '{}' timed out", call.name))?
    }
}
```

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
        msg: ChannelMessage,                   // 整个 ChannelMessage 进来
        channel: Arc<dyn Channel>,
        rt: &AgentRuntime,
    ) -> Result<TurnResult> {
        let _guard = self.turn_lock.lock().await;
        
        // 1. 写 transient 状态：channel + 持久化 last_message
        {
            let mut s = self.session.lock();
            s.channel = Some(channel.clone());
            s.last_message = Some(msg.clone());     // 也会随 session 持久化
        }
        
        // 2. attachment diff（按 turn 起手 snapshot）
        let skills = rt.skills.read().snapshot();
        let agents = rt.agents.list();
        let reminder = self.attachments.lock()
            .diff_and_render(&skills, &agents, &self.session.lock().history);
        
        // 3. 拼 user 消息（reminder + 原文本）写进 history（自动持久化）
        let user_text = match reminder {
            Some(r) => format!("{r}\n\n{}", msg.content),
            None => msg.content.clone(),
        };
        self.session.lock().add_user_text(user_text);
        
        // 4. resolve 配置参数（SessionOverride > [agent] 默认 > 内置）
        //    model 不在这里 resolve — None 时 Agent.run 内部走 ProviderRegistry fallback
        let s = self.session.lock();
        let permission_mode = s.session_override.permission_mode
            .unwrap_or(rt.defaults.permission_mode);
        let model_id_owned = s.session_override.model.clone();   // Option<String>
        let run_mode = s.session_override.run_mode.unwrap_or(RunMode::Interactive);
        let thinking = s.session_override.thinking.as_ref();
        drop(s);
        
        // 5. build system prompt（每轮拼装）
        let system_prompt = build_prompt(
            &self.agent.config,
            &self.user_profile,           // Phase 4 引入
            permission_mode,
            run_mode,
            &rt.defaults.prompt,
        );
        
        // 6. 调 Agent
        let ctx = TurnContext { 
            system_prompt: &system_prompt, 
            model_id: model_id_owned.as_deref(),      // Option<&str>
            thinking,
            permission_mode,
            run_mode,
        };
        let result = self.agent.run(&mut *self.session.lock(), &ctx, rt).await?;
        
        // 7. 处理 pending_retry（跨 turn 保留）
        if let Some(ref retry) = result.pending_retry {
            *self.pending_retry.lock() = Some(retry.clone());
        }
        
        // 8. 把最终文本发回用户（流式 turn 已 push_event 过，这条是 fallback / 总结）
        if !result.text.is_empty() {
            let target = &msg.reply_target;
            let _ = channel.send(SendMessage::new(&result.text, target)).await;
        }
        
        Ok(result)
    }
    
    async fn recover_pending_turn(&self, channel: Arc<dyn Channel>, rt: &AgentRuntime) -> Result<()>;
}
```

**职责**：
- 拥有 per-session 的对话数据 + attachment 状态
- 保证 turn 串行化（turn_lock）
- 把 channel 写进 session.channel，整条 ChannelMessage 写进 session.last_message，工具 / 后续 turn / 恢复都从 session 读取

### TurnContext / TurnResult

边界：`SessionContext.process_turn()` 是"配置解析层"——组装 system_prompt、resolve model/permission_mode/run_mode/thinking、算 attachment reminder；`Agent.run()` 是纯执行层，收到的全是已解析的标量。

```rust
struct TurnContext<'a> {
    system_prompt: &'a str,                // 含 builtin + body + profile + runtime + skills
    model_id: Option<&'a str>,             // None = Agent.run 内部走 ProviderRegistry 注册顺序 fallback
    thinking: Option<&'a ThinkingConfig>,
    permission_mode: PermissionMode,
    run_mode: RunMode,
}

struct TurnResult {
    text: String,
    stop_reason: StopReason,
    pending_retry: Option<String>,
}
```

**TurnContext 极简**——5 个字段，纯"本轮决策"。其他东西去哪：

| 不在 TurnContext 的东西 | 去处 |
|------------------------|------|
| `persist` | Session 内部（transient 字段 + `add_*` 方法） |
| `channel` | `session.channel`（transient） |
| `reply_target` / `sender` / 输入文本 / 图片 | `session.last_message`（持久化整条 ChannelMessage） |
| `stream` / `cancel` | Channel trait 内化（`push_event` / `cancel_signal`，target 用 `session.last_message.reply_target`） |
| `ask_router` | `AskUserTool` 内部 Arc 字段（Agent 不感知） |
| `delegator` | `DelegateTool` 内部 Arc 字段（Agent 不感知） |
| `tool_executor` / `loop_breaker` / `context_engine` / `tool_registry` | `AgentRuntime` 字段，run() 参数 |
| `tool_timeout` / `max_tool_calls` 等 limit | 各执行器内部 |
| `user_profile` / `AttachmentManager` | process_turn 边界消化（profile 拼进 system_prompt，attachment 算成 reminder） |
| `session_key` / `routing_key` | `session.id`；routing_key 只在 SessionManager 层流转，不进 session |

### Session 数据 + 自负责持久化

```rust
struct Session {
    // ── 持久化字段（12 个）──
    id: String,
    owner: String,                              // routing_key（永远是 rk，phase 4 不改）
    history: Vec<ChatMessage>,
    message_ids: Vec<i64>,
    compact_version: u32,
    summary_metadata: Option<SummaryMetadata>,
    session_override: SessionOverride,
    token_tracker: TokenTracker,
    incomplete_turn: bool,
    last_message: Option<ChannelMessage>,       // 整条进来的消息，含 sender/reply_target/content/images
    parent_session_id: Option<String>,          // Some = sub-session，None = user session
    agent_name: Option<String>,                 // sub-session 用哪个 agent（user session 默认 "main"）

    // ── transient 运行时 backing（2 个，不持久化）──
    #[serde(skip)]
    persist: Option<Arc<dyn PersistHook>>,      // None = ephemeral（CLI 模式）
    #[serde(skip)]
    channel: Option<Arc<dyn Channel>>,          // process_turn 每次写入
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
    
    /// 启动加载时调
    pub fn restore_token_count(&mut self, total: u64);
    /// 首次跑 turn 时如果 tracker fresh 调
    pub fn estimate_tokens_from_history(&mut self, system_prompt: &str);
}
```

**`last_message: ChannelMessage` 的作用**：
- 工具读取：ask_user 拿 `session.last_message.reply_target` 作为目标
- 流式 push：Agent.run 用 `session.last_message.reply_target` 作为 `channel.push_event` 的 target
- 启动恢复：完整 ChannelMessage 已持久化，恢复时直接读出，无需重建/拼接
- Sub-agent 继承：DelegationCoordinator 构造 synthetic ChannelMessage（content = task 内容，sender / reply_target 取自 parent.last_message），sub_ctx.process_turn 写入 sub_session.last_message；cancel/push 用同一个 reply_target，自然继承父 turn 的 stream

`ChannelMessage` 需要 `#[derive(Serialize, Deserialize)]`。

**收益**：
- Agent.run() 内不可能写了 history 忘记 persist（编译保证）
- TurnContext 不需要 `persist` 字段
- 持久化关注点封装在 Session
- CLI / 测试场景 `persist = None` 零成本退化
- ask_user 反射式接收 ChannelMessage 时整条信息自然流入

### SessionManager

单表 `sessions: routing_key → Arc<SessionContext>`。**每个 rk 自己的 session 池，跨 rk 不共享**（暂不支持跨 channel 接管 session）——这个约束让 1:1 关系自然成立，不需要主动校验。

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
    
    /// rk → session_id 反查（Orchestrator 处理 ask 反馈时用）
    fn session_id_for_routing_key(&self, rk: &str) -> Option<String>;
    
    /// session_id → ctx 反查（DelegationEvent 回填等场景用）
    /// 扫描 sessions.values() O(n)，仅 in-memory 的 ctx
    /// （sub-session 不在 sessions 表，调用方持 Arc 不需要反查）
    fn get_by_id(&self, session_id: &str) -> Option<Arc<SessionContext>>;

    /// 切换到本 rk 拥有的 session
    /// 不属于本 rk 时返回 SessionNotOwned 错误
    fn switch_session(&self, routing_key: &str, target_sid: &str) -> Result<(), SessionNotOwned>;

    /// 显式创建并切换到新 session
    fn create_session(&self, routing_key: &str, agent_name: &str, name: Option<&str>) -> Result<Arc<SessionContext>>;

    /// 子代理 session（不进 sessions 表，调用方持 Arc 管生命周期）
    /// 内部走同一个 backend.create_session，meta 设置 parent_session_id + agent_name
    /// 存储路径与 user session 同位：sessions/{sid}/（扁平）
    fn create_sub_session(&self, parent_sid: &str, agent_name: &str) -> Result<Arc<SessionContext>>;

    /// 删除
    fn delete_session(&self, routing_key: &str, sid: &str) -> Result<()>;

    /// 列出本 rk 的 session（仅 user session，filter parent_session_id IS NULL）
    fn list_sessions(&self, routing_key: &str) -> Vec<SessionInfo>;
    
    /// 列出某 user 的所有 session（phase 4 后启用，只读视图，不能 switch 进去）
    fn list_sessions_for_user(&self, user_id: &str) -> Vec<SessionInfo>;
    
    /// 删除一个 user session 时，级联删该 session 的所有 sub-session
    /// （扫 backend 找 parent_session_id == sid 的）
    fn delete_session_cascade(&self, sid: &str) -> Result<()>;
}

#[derive(Debug)]
pub struct SessionNotOwned {
    pub session_id: String,
    pub routing_key: String,
}
```

**为什么单表够用**：
- DelegationCoordinator 拿 parent 信息从 `&Session` 直接取（channel / last_message）
- AskRouter 内部用 session_id 索引 pending oneshot，跟 SessionManager 无关
- 启动恢复从 backend 直接扫，临时构造 ctx 跑完即弃
- `session_id_for_routing_key` 反查只在 Orchestrator 处理用户回复时使用

**跨 channel 不共享 session 带来的简化**：
- 没有"接管"语义，没有 force_takeover API
- 没有 1:1 不变量的主动校验（自然成立）
- 没有 channel.send 的"广播 fallback"
- ask_user / push_event 知道发哪：本 turn 的 session.channel + session.last_message.reply_target 唯一

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
    provider_registry: Arc<dyn ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    // ...
}

impl ContextEngine {
    fn should_compact(&self, session: &Session, context_window: u64) -> bool;
    
    /// 调 LLM 做 summary，需要保持 prefix cache 一致：
    /// system_prompt 和 tool_specs 跟主调 LLM 一样
    async fn compact(
        &self,
        session: &mut Session,
        system_prompt: &str,
        tool_specs: &[ToolSpec],
        model_id: &str,
    ) -> Result<()>;
}

// DefaultToolExecutor 重命名 + 退化为 timeout 包装器
// 不持 ToolRegistry — 工具池由 Agent.run() turn 起手过滤后传入
struct ToolExecutor {
    timeout: Duration,                  // from [tool_executor].timeout_secs
}

impl ToolExecutor {
    /// allowed 是 Agent 按其 filter 从全局 ToolRegistry 过滤后的子集
    async fn execute(
        &self,
        call: &ToolCall,
        session: &Session,
        allowed: &[Arc<dyn Tool>],
    ) -> Result<ToolResult>;
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

### Channel trait（内化流式与取消）

流式输出与取消能力**作为 Channel 自身能力**，不再用单独的 TurnStream 类型。非流式 channel 用默认实现（no-op / None）。

```rust
#[async_trait]
trait Channel: Send + Sync {
    async fn listen(&self) -> ChannelMessage;
    async fn send(&self, msg: SendMessage) -> Result<()>;
    
    /// 推流式事件给 target 对应的监听者
    /// 默认 no-op：非流式 channel（Telegram/QQBot/cron/webhook）
    async fn push_event(&self, target: &str, event: TurnEvent) { }
    
    /// 取本轮取消信号
    /// 默认 None
    fn cancel_signal(&self, target: &str) -> Option<CancellationToken> { None }
}
```

`target` 来自 `session.last_message.reply_target`——对 ClientChannel 就是 WS auth token。

ClientChannel 实现：

```rust
struct ClientChannel {
    connections: DashMap<u64, ClientConnection>,
    streams: DashMap<String, ClientStream>,    // target (reply_target) → (event_tx, cancel)
    // ...
}

#[async_trait]
impl Channel for ClientChannel {
    async fn push_event(&self, target: &str, event: TurnEvent) {
        if let Some(s) = self.streams.get(target) {
            let _ = s.event_tx.try_send(event);
        }
    }
    fn cancel_signal(&self, target: &str) -> Option<CancellationToken> {
        self.streams.get(target).map(|s| s.cancel.clone())
    }
}
```

WS 收到用户消息时注册 streams[target]，turn 结束时清除。`TurnStream` 类型整体消失。

### AskRouter

```rust
struct AskRouter {
    pending: DashMap<String, oneshot::Sender<ChannelMessage>>,    // session_id → sender
}

impl AskRouter {
    /// AskUserTool 调用，注册并等待用户的下一条 ChannelMessage
    async fn wait_for_reply(&self, session_id: &str) -> Result<ChannelMessage>;
    
    /// Orchestrator 收到用户消息时先调，命中则消费这条消息
    fn fulfill(&self, session_id: &str, msg: ChannelMessage) -> bool;
}
```

`wait_for_reply` 返回完整 ChannelMessage（不是裸 String）——LLM 拿到的 tool_result 可以包含用户回复的图片附件。

### AgentDelegator trait

```rust
#[async_trait]
trait AgentDelegator: Send + Sync {
    /// 委派任务给指定 sub-agent，返回结果摘要
    async fn delegate(
        &self,
        agent_name: &str,
        task: &str,
        parent_session: &Session,
    ) -> Result<String>;
    
    /// 返回可用 sub-agent 列表
    /// DelegateTool.spec() 调此构造工具 schema（agent 参数的枚举值）
    fn list_available(&self) -> Vec<(String, Option<String>)>;
}
```

`DelegationCoordinator` 实现这个 trait（含 worktree 编排 + create_sub_session + sub_ctx.process_turn）。`DelegateTool` 持 `Arc<dyn AgentDelegator>`，`execute()` 时调 `delegate`，`spec()` 时调 `list_available`。

Agent 启动后 hot-reload 增减 AGENT.md → AgentRegistry 更新 → 下轮 turn 的 DelegateTool.spec() 自动反映最新可委派列表。

### DelegationEvent

子代理完成后发回 Orchestrator 的事件类型：

```rust
pub enum DelegationEvent {
    Completed {
        parent_session_id: String,
        task_id: String,
        summary: String,
        duration_secs: u64,
    },
    Failed {
        parent_session_id: String,
        task_id: String,
        error: String,
    },
}
```

Orchestrator 处理后通过合成 ChannelMessage 调父 `ctx.process_turn`，详见 §五 启动恢复流程 / Sub-agent 完成回填到父 session。

### AgentRegistry（可变，hot-reload）

```rust
struct AgentRegistry {
    inner: RwLock<HashMap<String, Arc<Agent>>>,
}

impl AgentRegistry {
    fn get(&self, name: &str) -> Option<Arc<Agent>>;
    fn list(&self) -> Vec<(String, Option<String>)>;   // (name, description)
    fn reload_from_dir(&self, agents_dir: &Path);      // WorkspaceWatcher 调用
}
```

WorkspaceWatcher 文件变更时直接 `reload_from_dir`——重建 Arc<Agent> 并 swap 进 inner。

**Hot-reload 影响范围**：只影响**新建** SessionContext。已经持着 `Arc<Agent>` 的活跃 SessionContext 继续用旧版本，下次 SessionContext 重新创建时拿到新版本。**在跑的 turn 不会被打断**。

### WorkspaceWatcher（自维护）

```rust
struct WorkspaceWatcher {
    skills: Arc<RwLock<SkillManager>>,
    agents: Arc<AgentRegistry>,
    // 内部 spawn task 监听 workspace/skills/ 和 workspace/agents/ 变化，
    // 直接调 manager / registry 的 reload 方法
}
```

不再让每个 AgentLoop 各持一份 `change_rx`，也不再有 `Vec<AgentConfig>` 中间层。文件变化时 WorkspaceWatcher 直接更新对应的全局结构。

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

**UserResolver 的边界**：**只是只读视图层**——同一 user 的 rk_telegram 和 rk_webui 各有各的 active session，互不打通。user_id 只用于：
- 列 session 列表（`list_sessions_for_user(uid)` 返回 owner = uid 的全部 session，跨 rk 可见）
- Memory 路径（`workspace/users/{uid}/memory/`）
- UserProfile 加载
- 不用于 SessionManager.sessions 表的 key——仍然按 rk

**暂不支持跨 channel 接管 session**：在 webui 看到 telegram 创建的 session 时只能浏览历史，不能 switch 进去继续聊。`switch_session(rk_webui, sid)` 仅接受本 rk 拥有的 session，否则返回 `SessionNotOwned`。

未来若引入"接管"功能，作为独立的显式 API（force_takeover）补，不混入 switch_session。

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

实现层面 `ClientChannel` 内部维护：
- `connections: DashMap<conn_id, ws_sender>`——具体 WS 投递通道
- `streams: DashMap<String, ClientStream>`——按 `reply_target` (= sender = token) 索引当前流式 turn 的 event_tx + cancel

多 tab 同 sender → 多个 conn_id → channel.send 找最近活跃 conn 投递；turn 流式事件由发起 turn 的 conn 接收（最近一次注册的覆盖前者，参考"单 listener 胜出"原则——同 sender 多 tab 同时发 turn 不可能，turn_lock 保证串行）。

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
    Some(SessionOverride.model) → ProviderRegistry.get_chat_provider_by_model(m)
    None → ProviderRegistry.get_chat_provider(Capability::Chat) — 即 providers 注册顺序的第一个 chat-capable
    （TurnContext.model_id 是 Option<&str>，None 时 Agent.run 内部 fallback）

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

- Agent 不预算 prompt——AGENT.md body 已经是字符串字段 `agent.config.system_prompt`，每轮 turn 在 `build_prompt()` 里跟其他段拼装
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
    → Channel/Scheduler/WebhookHandler （仅做协议解码 → ChannelMessage）
    → mpsc::send(OrchestratorEvent)
    → Orchestrator.run() 主循环
        ├─ 解析 routing_key
        ├─ session_id = session_manager.session_id_for_routing_key(rk)
        ├─ 先 ask_router.fulfill(session_id, msg.clone())：命中则消费消息，return
        ├─ session_manager.get_context(routing_key)
        └─ ctx.process_turn(msg, channel, rt)
              → turn_lock.lock()
              → session.channel = channel; session.last_message = msg
              → build system_prompt; compute attachment reminder
              → agent.run(&mut session, &turn_ctx, rt)

子代理委派（特例）：
  agent.run() 内 → DelegateTool.execute(args, &session)
    → DelegationCoordinator.delegate(name, task, &session)
      ├─ 从 &session 拿 channel / last_message
      ├─ session_manager.create_sub_session(parent_sid, agent_name) → Arc<SessionContext>
      │   （sub ctx 不进 sessions 表，由调用方持 Arc 管生命周期）
      └─ sub_ctx.process_turn(synthetic_msg, channel, rt)
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
| 子代理 | （不进 Orchestrator） | `sub:{parent_sid}:{agent}:{uuid}` | 父 session 同 channel | Interactive |

**WebhookHandler 退化为协议适配器**：跟 TelegramChannel 同位，只负责签名校验 + 模板渲染 + 把 `WebhookEvent` 发给 Orchestrator。原来直接持 `Arc<SessionManager>` 的旁路删除。

**Orchestrator 持有 `Arc<AgentRuntime>`**，调 process_turn 时透传。
**DelegationCoordinator 也持有 `Arc<AgentRuntime>`**，sub_ctx.process_turn 同样需要。

### 启动恢复流程

**Sub-session 改为扁平存储**（跟 user session 同位 `sessions/{sid}/`），通过 `Session.parent_session_id: Option<String>` 字段区分类型。启动恢复**一套统一路径**，删除原有的 marker 文件机制。

```rust
for sinfo in backend.list_all_sessions() {     // 扫所有 session，含 sub-session
    let s = backend.load(&sinfo.id);
    if !s.incomplete_turn { continue; }
    
    let channel = match s.parent_session_id {
        None => {
            // user session：从 owner (routing_key) 解析 channel
            let (ch_type, account, _) = parse_routing_key(&s.owner)?;
            channels.get(&(ch_type, account))?.clone()
        }
        Some(ref parent_sid) => {
            // sub-session：通过 parent session 拿 channel
            let parent_s = backend.load(parent_sid);
            let (ch_type, account, _) = parse_routing_key(&parent_s.owner)?;
            channels.get(&(ch_type, account))?.clone()
        }
    };
    
    // s.last_message 已经持久化了完整 ChannelMessage，直接读
    let ctx = build_temp_context(s, agent_runtime);
    tokio::spawn(async move {
        let result = ctx.recover_pending_turn(channel, &runtime).await;
        // sub-session 恢复完成后 emit DelegationEvent 给父 session
        if let Some(parent_sid) = ctx.session.lock().parent_session_id.clone() {
            if let Ok(text) = result {
                orchestrator.send_delegation_event(DelegationEvent::Completed {
                    parent_session_id: parent_sid,
                    summary: text,
                    task_id: ctx.session.lock().id.clone(),
                    duration_secs: 0,
                }).await;
            }
        }
        // ctx Arc drop
    });
}
```

不再需要 `subagent_running_*.json` marker 文件——sub-session 的存在性由 backend 持久化保证，incomplete 状态由 `session.incomplete_turn` 标记。

### Sub-agent 完成回填到父 session

子代理跑完后，DelegationCoordinator 把结果作为 `DelegationEvent` 发给 Orchestrator。Orchestrator 构造"合成 ChannelMessage"调父 session 的 `process_turn`，让 LLM 看到一条系统通知并决定如何回应：

```rust
// Orchestrator.handle_delegation_event
async fn handle_delegation_event(&self, ev: DelegationEvent) {
    let (parent_sid, summary_text, task_id) = match ev {
        DelegationEvent::Completed { parent_session_id, summary, task_id, duration_secs } => (
            parent_session_id,
            format!("[系统通知] 子代理已完成 (task_id: {}, {}s):\n{}", task_id, duration_secs, summary),
            task_id,
        ),
        DelegationEvent::Failed { parent_session_id, error, task_id, .. } => (
            parent_session_id,
            format!("[系统通知] 子代理失败 (task_id: {}): {}", task_id, error),
            task_id,
        ),
    };
    
    let parent_ctx = self.session_manager.get_by_id(&parent_sid)?;
    let parent_session = parent_ctx.session.lock();
    let parent_msg = parent_session.last_message.clone()?;
    let channel = parent_session.channel.clone()?;
    drop(parent_session);
    
    // 合成 ChannelMessage：sender/reply_target 沿用父 session 的最近 incoming
    let synthetic = ChannelMessage {
        sender: parent_msg.sender,
        content: summary_text,
        reply_target: parent_msg.reply_target,
        image_urls: None,
        image_base64: None,
        id: format!("delegation:{}", task_id),
    };
    
    tokio::spawn(async move {
        parent_ctx.process_turn(synthetic, channel, &self.agent_runtime).await
    });
}
```

`SessionManager.get_by_id(sid)` 是给这种"我有 session_id 想找 ctx"场景用的辅助方法（sub-session 不在 sessions 表，要从那里找。如果是用户 session，可以扫 sessions.values() 找匹配的）。

---

## 六、关键改动点

| 组件 | 现在 | 重构后 |
|------|------|--------|
| `Agent` | AgentLoop 工厂 + 大量 Arc 字段 + 预算的 system_prompt 字段 | 纯身份，仅 config 一个字段；prompt 每轮在 process_turn 内拼装 |
| `AgentLoop` | 持有 Session ownership | 删除 |
| `AgentRuntime` | （不存在） | 新增，bundle 全局基础设施（registry/context_engine/tool_executor/loop_breaker），Agent.run() 参数 |
| `RequestBuilder` | 五职责混杂 struct | 删除（system_prompt 入 process_turn，AttachmentManager 入 SessionContext，change_rx 入 WorkspaceWatcher，images 入 TurnInput，build() 改无状态函数） |
| `LoopRegistry` | DashMap 平行缓存 | 删除 |
| `SessionHandle` | Arc<Mutex<AgentLoop>> + Sender | 删除 |
| `run_session_actor` | channel + actor | 删除（turn_lock 替代） |
| `ask_user` handler | per-session 闭包 | `AskUserTool` 内部持 `Arc<AskRouter>`，Agent 不感知 |
| `delegate` handler | per-session 闭包 | `DelegateTool` 内部持 `Arc<dyn AgentDelegator>`，Agent 不感知 |
| `persist_hook` | per-AgentLoop 创建，Agent.run() 手动调用 | Session 内嵌（transient `Option<Arc<dyn PersistHook>>`），通过 `Session::add_*` 方法自动持久化 |
| `Session.channel` | （不存在） | 新增 transient 字段（`Option<Arc<dyn Channel>>`），process_turn 写入 |
| `Session.last_message` | （不存在） | 新增持久化字段（`Option<ChannelMessage>`），替代 last_reply_target；含 sender/reply_target/content/images，供工具/恢复/sub-agent 继承使用 |
| `Session.last_reply_target` | 持久化字段 | 删除（合进 last_message） |
| `Session.routing_key` | （不存在） | **不加**——routing_key 只在 SessionManager 层流转 |
| `SessionManager.cache` + `LoopRegistry.sessions` | 双层缓存（rk→sid + sid→AgentLoop），需 evict 同步 | 单表 `sessions: HashMap<rk, Arc<SessionContext>>` |
| 跨 channel session 共享 | 暗中支持 | 不支持（暂时）；switch_session 仅接受本 rk 拥有的 session，返回 SessionNotOwned 错误 |
| `WebhookContext` | 独立持 SessionManager + sessions 等 | 删除；`WebhookHandler` 退化为协议适配器 |
| WebUI sender | 可选 client_id 或 per-conn id | 必须 = auth token |
| `Channel` trait | listen / send | 加 `push_event(target, event)` + `cancel_signal(target)` 两个默认 no-op 方法；流式与取消的能力内化 |
| `TurnStream` | 独立类型 | 删除（Channel trait 内化） |
| `TurnInput` | 独立类型（content + images） | 删除（替代为 `ChannelMessage`，整条进 session.last_message） |
| `Agent.run()` 入参 | session + input + ctx + rt | session + ctx + rt（input 来自 session.last_message / history） |
| `process_turn` 入参 | input/channel/reply_target/rt | msg(ChannelMessage) / channel / rt |
| `recover_pending_turn` | （新增） | session + channel + rt — channel 必须由调用方注入（transient 字段丢失） |
| `SessionManager.get_by_id` | （新增） | session_id → Arc<SessionContext>，给 DelegationEvent 回填等场景用 |
| `AskRouter.wait_for_reply` | 返回 `String`（仅文本） | 返回 `ChannelMessage`（含图片等完整信息） |
| `AgentRegistry` | （现在散落） | `Arc<RwLock<HashMap<name, Arc<Agent>>>>`，WorkspaceWatcher 直接调 reload，hot-reload 只影响新 session |
| `Vec<AgentConfig>` 中间结构 | RequestBuilder.resources 持有 | 删除，AgentRegistry 直接持 Arc<Agent> |
| `CompactionPolicy` + `CompactionExecutor` | per-AgentLoop | 合并为全局 `ContextEngine` |
| `AttachmentManager` | RequestBuilder 字段 | `SessionContext` 字段 |
| `change_rx` | 传给每个 AgentLoop | `WorkspaceWatcher` 自维护 RwLock |
| `IDENTITY.md` / `SOUL.md` / `RULES.md` | workspace 根目录 | 合并进 main/AGENT.md body |
| `USER.md` | workspace 根目录全局 | per-user UserProfile（阶段 4） |
| `SubAgentConfig` | 与主 agent 配置不同 | 统一为 `AgentConfig` |
| `SubAgentDelegator` | 持 agent 配置 + 临时构建 AgentLoop | `DelegationCoordinator` 编排 worktree，agent 通过 SessionManager 统一执行 |
| Sub-session 存储 | `sessions/{parent}/subagents/{sub}/` 嵌套 + `subagent_running_*.json` marker | 扁平 `sessions/{sid}/` + `Session.parent_session_id` 字段；删除 marker 文件 |
| 启动恢复路径 | user 走 list_all + marker 走子代理 | 一套统一路径：list_all + parent_session_id 区分 |
| `max_tool_calls` 等 limits | AgentConfig + SessionOverride | 仅 `GlobalConfig.limits` |
| Tool trait | 仅 `name` / `spec` / `execute(args)` | 新增 `source() -> ToolSource`（Builtin / McpServer / Skill），`execute(args, &Session)` |
| Tool 过滤维度 | `tools: Vec<String>` 一维白名单 | 三维独立：`tools` (内置) / `mcp` (server) / `skills`；ToolFilter 支持 All/Allow/Deny |
| Tool 过滤应用点 | SubAgentDelegator 临时 build_filtered_tools | Agent.run() turn 起手 snapshot allowed_tools；spec 过滤 + executor 查找双层防御 |
| `ToolExecutor` | 持 ToolRegistry + ask_handler/delegate_handler/sub_delegator | 退化为 timeout 包装器，工具池每次执行由 Agent 传入 |
| `tool_timeout_secs` | `AgentConfig.tool_timeout_secs` | `ToolExecutor` 字段（来自 `[tool_executor]`） |
| `max_tool_calls` / `loop_breaker_threshold` | `AgentConfig` + SessionOverride | `LoopBreaker` 字段（来自 `[loop_breaker]`） |
| `compact_threshold` / `retain_work_units` | `AgentConfig.context` + SessionOverride | `ContextEngine` 字段（来自 `[context_engine]`） |
| `stream_first_chunk_timeout_secs` / `max_output_bytes` | `AgentConfig` | **硬编码常量**（`llm_stream` 模块） |
| `max_history` | `AgentConfig` | **删除**（死代码） |
| `permission_mode` | `AgentConfig` + SessionOverride | `[agent].permission_mode` + SessionOverride 覆盖 |
| `model` | `AgentConfig` + SessionOverride | TurnContext.model_id: `Option<&str>`；None 时 Agent.run fallback 到 `ProviderRegistry.get_chat_provider(Chat)`（providers 注册顺序的第一个） |
| `ServiceRegistry` | trait 名 | 改名为 `ProviderRegistry`（更准确） |
| `thinking` | `AgentConfig` + SessionOverride | 仅 `SessionOverride` |
| `run_mode` | SessionOverride | 仅 `SessionOverride`（触发源决定） |
| `isolation` | SessionOverride | 仅 `AgentConfig`（agent 身份） |

---

## 七、实施策略

### 阶段 1：AgentLoop 解耦 Session

1. `Agent.run()` / `AgentLoop.run()` 接收 `&mut Session` 参数，不再从 `self.session` 读取
2. ~57 处 `self.session` → `session` 机械替换（run.rs 22 处，compaction.rs 32 处，tools.rs 3 处）
3. `persist_hook` 暂时改为 `run()` 参数（阶段 2 再挪到 Session 内部）
4. `AgentLoop.session` 字段删除

可独立部署，顺带消除 `evict_loop` workaround 的根因。

### 阶段 2：SessionContext + 去间接层 + 拆解 RequestBuilder

1. 定义 `TurnContext`（5 字段）/ `TurnResult`；删除 `TurnInput` 类型（信息全在 `ChannelMessage`）
2. Session 改造：
   - 内嵌 transient `persist: Option<Arc<dyn PersistHook>>` 和 `channel: Option<Arc<dyn Channel>>`
   - 持久化字段加 `last_message: Option<ChannelMessage>`，替代 last_reply_target
   - `Session.token_tracker` 替代 `last_total_tokens` 单字段；加 `restore_token_count` / `estimate_tokens_from_history` 方法
   - 暴露 `add_user_text` / `add_assistant_text` / `add_tool_*` / `save_summary` / `update_token_count` 方法（内部调 persist）
3. `ChannelMessage` 加 `#[derive(Serialize, Deserialize)]`
4. Channel trait 加默认 no-op 方法 `push_event(target, event)` 和 `cancel_signal(target)`；TurnStream 类型删除
5. ClientChannel：
   - `streams: DashMap<String, ClientStream>` 用 reply_target 做 key
   - 实现 push_event / cancel_signal
   - 删 `loop_registry`、`evict_loop`
   - sender = auth token，匿名连接拒绝
6. `AskRouter`：`pending: DashMap<sid, oneshot::Sender<ChannelMessage>>`；`wait_for_reply` 返回 ChannelMessage
7. 新建 `SessionContext`（不存 channel；持 `AttachmentManager` 和 `pending_retry`）
8. `SessionManager` 单表 `sessions: HashMap<rk, Arc<SessionContext>>`；删 `cache`；加 `session_id_for_routing_key` 方法；`switch_session` 仅接受本 rk 拥有的 session
9. `SessionContext.process_turn(msg, channel, rt)` 承担"配置解析层"职责：写 session.channel / session.last_message → 组装 system_prompt（含 attachment reminder）→ Agent.run
10. 拆解 `RequestBuilder`：
    - `system_prompt` 由 `process_turn` 每轮组装
    - `AttachmentManager` 挪进 `SessionContext.attachments`
    - `change_rx` 删除（WorkspaceWatcher 自维护）
    - pending images 改为 ChannelMessage 自带字段
    - `build()` / `merge_attachments()` 改为无状态函数
11. 拆 `CompactionPolicy` + `CompactionExecutor` → 全局 `ContextEngine`（持 compact_threshold/retain_work_units，方法签名 `compact(session, system_prompt, tool_specs, model_id)`）+ `Session.token_tracker`（per-session 状态）
12. `DefaultToolExecutor` 重命名为 `ToolExecutor`，timeout 内化为字段，不持 ToolRegistry
13. 拆 `LoopBreaker` 为全局 policy + per-turn counter，配置从 `[loop_breaker]` 读
14. `LlmResponseReader` 退化为 `llm_stream::read` 模块函数 + 硬编码常量
15. `ServiceRegistry` 重命名为 `ProviderRegistry`（trait + 文件 + 引用全替换）；同时为 trait 加 `default_chat_model_id() -> String` 之类的辅助方法（如已存在 get_chat_provider(Capability) 路径可复用）
16. 引入 `AgentRuntime` bundle（8 字段：provider_registry / tool_registry / skills / agents / context_engine / tool_executor / loop_breaker / defaults）+ `RuntimeDefaults` struct（含 permission_mode + PromptConfig）；Orchestrator / DelegationCoordinator 各持一份
17. 删 `AgentLoop`、`SessionHandle`、`LoopRegistry`、`run_session_actor`、`TurnStream`
18. 引入 `AskUserTool` / `DelegateTool`：各持自己的 Arc 依赖，注册进 ToolRegistry
19. `DelegationCoordinator` 接管 worktree / merge / cleanup；`SubAgentDelegator` 删除；实现 AgentDelegator trait（含 `delegate` + `list_available`）；引入 `DelegationEvent` enum
20. Orchestrator 主循环：
    - 解析 routing_key
    - `let sid = session_manager.session_id_for_routing_key(rk);`
    - 若 sid 存在且 `ask_router.fulfill(sid, msg)` 命中 → 消费 return
    - 否则 `session_manager.get_context(rk).process_turn(msg, channel, rt).await`
21. WorkspaceWatcher 改为自维护（own SkillManager + AgentRegistry，文件变更直接调 reload）
22. Scheduler / Webhook 路径收编：删 `SchedulerContext` / `WebhookContext`，全走 OrchestratorEvent 进 Orchestrator
23. Sub-session 存储改为扁平：跟 user session 同位 `sessions/{sid}/`；Session 加 `parent_session_id: Option<String>` 和 `agent_name: Option<String>` 字段；`list_sessions` 时 filter parent IS NULL；删 user session 时 cascade 删其 sub
24. 启动恢复统一路径：`backend.list_all_sessions()` 扫所有 incomplete_turn；`parent_session_id` 区分 user/sub；user → channel.send 结果；sub → emit DelegationEvent 给父；删除 marker 文件机制
25. 删 `AgentConfig.max_history`（死代码）

### 阶段 3：Agent 配置统一

1. `AgentConfig` 简化：仅 `name` / `description` / `tools` / `skills` / `mcp` / `isolation` / `system_prompt`
2. 实现 `ToolFilter` / `SkillFilter` / `McpFilter` enum (All/Allow/Deny)
3. `Tool` trait 加 `source() -> ToolSource`（Builtin / McpServer / Skill），`execute` 加 `&Session` 参数
4. MCP / skill tool 注册时填正确的 source
5. `AgentConfig::allows_tool(&dyn Tool) -> bool` 按 source 分支三维独立过滤
6. `Agent.run()` turn 起手 snapshot allowed_tools 子集，转 spec 传 LLM；`ToolExecutor.execute(call, session, &allowed_tools)` 在子集内查找
7. `GlobalConfig` 按模块拆段：`[locale]` / `[prompt]` / `[agent]` / `[tool_executor]` / `[loop_breaker]` / `[context_engine]` / `[providers]` / `[channels]` / `[mcp_servers]` / `[scheduler]` / `[users]`
8. `AgentRegistry` 实际为 `Arc<RwLock<HashMap<String, Arc<Agent>>>>`，启动时加载所有 `workspace/agents/*/AGENT.md`（含 main）；WorkspaceWatcher 文件变更时通过 `reload_from_dir(agents_dir)` 重建 Arc<Agent> 并 swap 进 inner（hot-reload 只影响新 SessionContext）
9. `prompt.rs` 删 IDENTITY/SOUL/USER.md 读取；prompt 组合改为参数化（`build_prompt(...)` 函数）
10. 启动校验：`workspace/agents/main/AGENT.md` 缺失则报错退出
11. 提供 `scripts/migrate_main_agent.sh` 一次性脚本

### 阶段 4：用户信息 per-user

1. `UserResolver`：routing_key → user_id 映射（默认透传）
2. `UserProfile` 加载/序列化/`to_prompt_section`
3. `SessionContext` 加 `user_profile: Arc<UserProfile>`
4. `build_prompt` 补齐 profile section
5. Memory tools 读写 `workspace/users/{id}/memory/`
6. `SessionManager.list_sessions_for_user(uid)` 实现：通过 UserResolver **反向** map 找出 uid 对应的所有 routing_keys，再 filter backend.list_all_sessions() 中 owner ∈ rks 的（**Session.owner 保持 routing_key 不变**，不做数据迁移）
7. 提供 `scripts/migrate_memory.sh`（默认目标 `users/_default/memory/`）
8. 提供 `scripts/migrate_user_profile.sh`（USER.md → `users/_default/profile.md`）

**Session.owner 字段语义不变**：始终是 routing_key（"消息从哪来"的端点标识）。user_id 是查询层衍生概念，不污染存储 schema，也不需要 owner 字段迁移脚本。

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
