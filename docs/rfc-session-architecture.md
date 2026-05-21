# RFC: Session 架构重构

> 状态：草案
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
| `request_builder`, `compactor` | registry/tools 的包装 | Agent |
| `session` | 对话数据 | SessionContext |
| `loop_breaker`, `policy` | per-turn 计数器 | `run()` 的局部变量 |
| `persist_hook` | 持久化回调 | 全局共享 |

真正 per-turn 的 `LoopBreaker`（tool call 计数）和 `CompactionPolicy`（token 追踪）不需要持久化，作为 `Agent.run()` 的局部变量就行。AgentLoop 整个 struct 不需要存在。

### 4. 主 agent 与子代理配置不统一

- 主 agent 配置散落在 `config.yaml` 的 `agent` 段
- 子代理配置在 `workspace/agents/<name>/AGENT.md`
- 两者格式不同，字段不对等，构建路径完全不同

### 5. Prompt 构建对主 agent 特化

`SystemPromptBuilder` 硬编码了主 agent 的 prompt 结构（读 IDENTITY.md、SOUL.md、USER.md），子代理只多了个 `identity_header` workaround。实际上 prompt 的各部分应该由 agent 配置驱动，而不是代码里的 if/else。

### 6. user_id 名不副实

`user_id` 实际是路由 key（`"telegram:bot1:12345"`），不是真正的用户标识。WebUI 开两个标签页就是两个"用户"，session 互不可见。用户信息（USER.md）是全局共享的，无法 per-user。

---

## 二、目标架构

### 整体结构

```
daemon.rs (Composition Root)
  │
  ├─ create ServiceRegistry          ← 全局，所有 Agent 共享
  ├─ create ToolRegistry             ← 全局，所有 Agent 共享
  ├─ create SkillManager             ← 全局，所有 Agent 共享
  │
  ├─ load_agents_from_dir()          ← 返回 Vec<Agent>
  │   ├─ Agent("main", ...)          ← 主 agent
  │   ├─ Agent("coder", ...)         ← 子代理
  │   └─ Agent("researcher", ...)    ← 子代理
  │
  └─ create SessionManager(AgentRegistry)
       └─ create SessionContext(...)  ← per session
```

### Agent

```rust
struct Agent {
    // 身份与配置
    name: String,
    config: AgentConfig,              // max_tool_calls, compact_threshold 等
    system_prompt: String,            // AGENT.md body（可以为空，由 builder 生成）
    tool_names: Vec<String>,          // 这个 agent 能用哪些 tool
    skill_names: Vec<String>,         // 这个 agent 绑定哪些 skill

    // 共享基础设施引用（不拥有）
    registry: Arc<dyn ServiceRegistry>,
    tool_impls: Arc<ToolRegistry>,    // 全局 tool 实现池
    skills: Arc<RwLock<SkillManager>>,
}

impl Agent {
    /// 无状态执行一轮 turn。LoopBreaker、CompactionPolicy 作为局部变量。
    fn run(&self, session: &mut Session, msg: &str, ctx: &TurnContext) -> TurnResult;
}
```

Agent 是同一个类型，不同实例有不同配置。"主 agent"和"子代理"不是不同类型，是同一个类型的不同实例。

### SessionContext

```rust
struct SessionContext {
    session: Session,                 // 对话数据：history, metadata
    agent: Arc<Agent>,                // 绑定的 agent（创建时选定）
    channel: Arc<dyn Channel>,        // 通信通道
    user_profile: Arc<UserProfile>,   // 用户信息
    mutex: tokio::sync::Mutex<()>,    // turn 串行化
}

impl SessionContext {
    async fn process_turn(&self, msg: TurnMessage) {
        let _guard = self.mutex.lock().await;
        self.channel.on_status(Thinking).await;
        self.agent.run(&mut self.session, &msg, &ctx).await;
        self.channel.on_status(Done).await;
    }
}
```

**职责**：
- 持有 per-session 的所有状态
- 保证 turn 串行化（mutex，不需要 channel + actor）
- 不持有 AgentLoop，不持有 persist_hook

### SessionManager

```rust
struct SessionManager {
    backend: Arc<dyn SessionBackend>,
    agents: HashMap<String, Arc<Agent>>,   // name → Agent 实例
    active: HashMap<String, String>,        // routing_key → session_id
    contexts: HashMap<String, Arc<SessionContext>>,  // session_id → context
}

impl SessionManager {
    fn switch_session(&mut self, routing_key: &str, session_id: &str) {
        self.active.insert(routing_key, session_id);
        // 天然一致，不需要 evict
    }

    fn get_context(&self, routing_key: &str) -> Arc<SessionContext> {
        let session_id = self.active.get(routing_key)?;
        self.contexts.get(session_id)
    }
}
```

**消灭的东西**：
- `LoopRegistry`（DashMap）→ 职责回归 SessionManager
- `SessionHandle`（Arc<Mutex<AgentLoop>> + Sender）→ 不需要
- `run_session_actor`（while let + channel）→ mutex 替代
- `get_or_create()` 的工厂逻辑 → SessionContext 构造时一次性完成

### UserProfile

```rust
struct UserProfile {
    id: String,                       // 用户标识
    name: Option<String>,
    timezone: Option<String>,
    language: Option<String>,
    preferences: HashMap<String, String>,
}
```

per user，跨 session 持久化，按用户隔离存储：

```
workspace/users/
  albert/
    profile.md          ← 基本身份信息（替代当前 USER.md）
    preferences.md      ← 积累的偏好
    memory/             ← 原 memory 目录，按用户隔离
```

来源分层（高→低优先级）：

```
1. session 内对话中积累的偏好
2. channel 提供的用户信息（Telegram profile 等）
3. 全局默认（workspace/DEFAULT_USER.md）
```

在认证体系做好之前，先用 routing key 作为用户标识的 fallback——每个 channel endpoint 看作一个"用户"，和现在的行为一致，但数据结构上是 per-user 的。

---

## 三、Agent 配置统一

### 目录结构

```
workspace/agents/
├── main/
│   └── AGENT.md            ← 主 agent
├── coder/
│   └── AGENT.md            ← 子代理
└── researcher/
    └── AGENT.md            ← 子代理
```

每个 agent 一个目录，一个 `AGENT.md`。不需要 `IDENTITY.md`、`SOUL.md`、`RULES.md` 等中间文件。身份、行为准则、system prompt 全部在 AGENT.md 的 body 里。

### AGENT.md 格式

```markdown
---
name: coder
description: "Expert programmer for writing and editing code"

# 工具与技能
tools: [shell, file_read, file_write, file_edit, content_search]
skills: [code-review]
model: claude-sonnet-4-20250514

# 行为参数
max_tool_calls: 30
permission_mode: auto
compact_threshold: 0.7
isolation: shared
---

# Coder Agent

You are an expert programmer. Write clean, idiomatic code.

## Behavioral Principles

Be concise. Don't over-engineer. Three similar lines of code is better
than a premature abstraction.

## Safety

Ask before executing destructive shell commands.
```

主 agent 的 AGENT.md body 可以留空，prompt 完全由 builder 的 builtin sections 生成：

```markdown
---
name: main
tools: [all]
skills: [all]
model: null
---

（body 为空）
```

### Prompt 构建

```
最终 system prompt =
  1. builtin sections                    ← 所有 agent 都有（行为规则、安全规则等）
  2. AGENT.md body                       ← agent 自己定义的 prompt（可空）
  3. user_profile.to_prompt()            ← 这个 session 的用户信息
  4. runtime info                        ← OS 信息
  5. skill 指令                          ← 按 agent 配置的 skills 过滤
```

不再硬编码读 IDENTITY.md / SOUL.md。USER.md 替换为 per-session 的 `user_profile`。

---

## 四、消息流转对比

### 现在

```
用户消息 → Channel 收到 → orchestrator 主循环
  → get_or_create(sk) 拿到 SessionHandle
    ├─ SessionManager.get_or_create(sk)        ← 拿 Session
    ├─ agent.loop_for_with_persist(session)     ← Session move 进 AgentLoop
    ├─ 绑 ask_user / delegate handler 闭包
    ├─ spawn run_session_actor
    └─ 存进 DashMap
  → handle.tx.send(TurnMessage)
  → run_message_task
    → loop_.lock().await
    → AgentLoop.run()
```

### 重构后

```
用户消息 → Channel 收到 → orchestrator 主循环
  → session_manager.get_context(routing_key)
  → ctx.process_turn(msg)
    → mutex.lock()
    → agent.run(&mut session, msg, ctx)
```

一层调用，没有 channel / actor / factory 间接层。

---

## 五、关键改动点

| 组件 | 现在 | 重构后 |
|------|------|--------|
| `Agent` | AgentLoop 工厂 | 无状态执行器，暴露 `run()` |
| `AgentLoop` | 持有 Session ownership | 删除 |
| `LoopRegistry` | `DashMap<sk, SessionHandle>` 独立缓存 | 删除，职责回归 SessionManager |
| `SessionHandle` | `Arc<Mutex<AgentLoop>> + Sender` | 删除，SessionContext 自身保证串行化 |
| `run_session_actor` | `while let` + channel 消费 | 删除，`process_turn` 直接调用 |
| `run_message_task` | 独立函数 | SessionContext 方法 |
| `ask_user` handler | per-session 闭包 | SessionContext 方法 |
| `delegate` handler | per-session 闭包 | SessionContext 方法 |
| `persist_hook` | per-AgentLoop 创建 | 全局共享单例 |
| `IDENTITY.md` / `SOUL.md` / `RULES.md` | workspace 根目录 | 删除，内容写入 AGENT.md body |
| `USER.md` | workspace 根目录全局 | per-user profile |
| `SubAgentConfig` | 与主 agent 配置格式不同 | 统一为 AgentConfig（从 AGENT.md 解析） |

---

## 六、实施策略

### 阶段 1：AgentLoop 解耦 Session（最小改动，可独立部署）

1. `Agent.run()` / `AgentLoop.run()` 接收 `&mut Session` 参数，不再从 `self.session` 读取
2. ~60 处 `self.session` → `session` 机械替换（run.rs 22 处，compaction.rs 32 处，tools.rs 3 处）
3. `persist_hook` 改为 `run()` 参数
4. AgentLoop.session 字段删除

### 阶段 2：SessionContext + 去掉间接层

1. 新建 `SessionContext` 结构体
2. 将 `LoopRegistry.get_or_create()` 迁移到 `SessionManager`
3. 去掉 `run_session_actor`，用 `SessionContext.process_turn()` + mutex 替代
4. `ask_user` / `delegate` 从闭包改为 SessionContext 方法
5. 删除 `SessionHandle`、`LoopRegistry`、`run_session_actor`

### 阶段 3：Agent 配置统一

1. 扩展 AGENT.md front matter（加 skills、permission_mode 等）
2. 主 agent 也从 AGENT.md 加载，与子代理走同一路径
3. `SubAgentConfig` 和主 agent 配置统一为一个结构体
4. 简化 `SystemPromptBuilder`，去掉 IDENTITY.md / SOUL.md 读取

### 阶段 4：用户信息 per-user

1. 新建 `UserProfile` 结构和 per-user 存储目录
2. SessionContext 创建时加载 UserProfile
3. prompt 注入从 USER.md 改为 user_profile
4. routing key → user_id 的映射层（认证体系前置条件）

### 当前 workaround

在阶段 1 完成前，session switch/create/delete 的 API handler 通过 evict LoopRegistry 保证一致性（commit `a7a31ff`）。
