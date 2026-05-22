# MyClaw 模块关联关系总图

> 基于 RFC: Session 架构重构 v2
> 日期：2026-05-21

下图覆盖目标架构的**全部模块**和它们之间的关系。三种箭头：

- **`━━▶`**  Ownership（拥有，drop 时级联）
- **`╌╌▶`**  Arc 引用（共享，不拥有）
- **`···▶`**  调用/数据流（消息流向）

---

## 全图：模块关系

```
═════════════════════════════════════════════════════════════════════════════════
            持久化层（文件系统，daemon 启动时读 / 运行时读写）
═════════════════════════════════════════════════════════════════════════════════

  config.toml              workspace/
   ┌────────────────┐       ┌──────────────────────────────────────┐
   │ [locale]       │       │  agents/{name}/AGENT.md              │
   │ [prompt]       │       │  skills/{name}/SKILL.md              │
   │ [agent]        │       │  sessions/{sid}/history.jsonl, ...   │
   │ [tool_executor]│       │  users/{uid}/profile.md              │
   │ [loop_breaker] │       │  users/{uid}/preferences.md          │
   │ [context_eng]  │       │  users/{uid}/memory/{type}/          │
   │ [providers]    │       │  cron/jobs.json                      │
   │ [channels]     │       │  DEFAULT_USER.md                     │
   │ [mcp_servers]  │       └──────────────────────────────────────┘
   │ [scheduler]    │                 │
   │ [users]        │                 │
   └─────┬──────────┘                 │
         │ 加载                        │ load/save/watch
         ▼                            ▼

═════════════════════════════════════════════════════════════════════════════════
                       daemon.rs (Composition Root)
═════════════════════════════════════════════════════════════════════════════════
  │
  │ 构造下列所有组件，按依赖顺序 wire 起来
  │
  ┣━━▶ GlobalConfig (load from config.toml, 值类型，Arc 包装后共享)
  │
  ┣━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
  ┃                   全局基础设施单例（Arc 共享）                       ┃
  ┃                                                                       ┃
  ┃  ┌─────────────────┐   ┌──────────────┐   ┌─────────────────┐       ┃
  ┃  │ ServiceRegistry │   │ ToolRegistry │   │  SkillManager   │       ┃
  ┃  │ (LLM providers) │   │ (tool 实现池) │   │ Arc<RwLock<...>>│       ┃
  ┃  └─────────────────┘   └──────▲───────┘   └────────▲────────┘       ┃
  ┃                                ┃                    ┃                ┃
  ┃                                ┃ 注册 MCP/skill tool┃ 自动 reload    ┃
  ┃  ┌─────────────────┐           ┃                    ┃                ┃
  ┃  │   McpManager    │━━━━━━━━━━━┛                    ┃                ┃
  ┃  └─────────────────┘                                ┃                ┃
  ┃                                                     ┃                ┃
  ┃  ┌─────────────────────────────────────────┐        ┃                ┃
  ┃  │           WorkspaceWatcher              │━━━━━━━━┛                ┃
  ┃  │  own: Arc<RwLock<SkillManager>>         │                          ┃
  ┃  │  own: Arc<RwLock<Vec<AgentConfig>>>     │━━━━━━━━┓                ┃
  ┃  │  内部 task 监听文件系统变化              │        ┃                ┃
  ┃  └─────────────────────────────────────────┘        ┃                ┃
  ┃                                                     ┃                ┃
  ┃  ┌─────────────────┐   ┌──────────────┐   ┌─────────────────┐       ┃
  ┃  │  ContextEngine  │   │SessionBackend│   │  ToolExecutor   │       ┃
  ┃  │ (compact_thresh,│   │ (JSONL 持久化)│   │ (timeout)       │       ┃
  ┃  │  retain_units,  │   │              │   │ 不持 Registry   │       ┃
  ┃  │  registry/tools)│   │              │   │ 工具池由 Agent  │       ┃
  ┃  │ from [context_  │   │              │   │ turn 起手过滤   │       ┃
  ┃  │   engine]       │   │              │   │ 后传入          │       ┃
  ┃  │ compact(session,│   │              │   │ from [tool_     │       ┃
  ┃  │  prompt, specs, │   │              │   │   executor]     │       ┃
  ┃  │  model_id)      │   │              │   │                 │       ┃
  ┃  └─────────────────┘   └──────────────┘   └─────────────────┘       ┃
  ┃                                                                       ┃
  ┃  ┌─────────────────┐   ┌──────────────────────────────────────┐      ┃
  ┃  │  LoopBreaker    │   │  llm_stream (模块，非 struct)        │      ┃
  ┃  │ (max_tool_calls,│   │   const FIRST_CHUNK_TIMEOUT = 600s   │      ┃
  ┃  │  threshold)     │   │   const MAX_OUTPUT_BYTES   = 100KB   │      ┃
  ┃  │ from [loop_     │   │   pub fn read(stream) -> Response    │      ┃
  ┃  │   breaker]      │   │   pub fn read_streamed(...)          │      ┃
  ┃  └─────────────────┘   └──────────────────────────────────────┘      ┃
  ┃                                                                       ┃
  ┃  ┌─────────────────┐   ┌──────────────┐   ┌─────────────────┐       ┃
  ┃  │  AgentRegistry  │   │  AskRouter   │   │ UserResolver    │       ┃
  ┃  │ HashMap<name,   │   │ (pending_    │   │ (rk → user_id)  │       ┃
  ┃  │   Arc<Agent>>   │   │  oneshot map)│   │                 │       ┃
  ┃  └──┬───┬──────┬───┘   └───┬──────────┘   └─────────────────┘       ┃
  ┃     ┃   ┃      ┃           │ 仅注入到 AskUserTool 内部              ┃
  ┃     ▼   ▼      ▼           │ Agent / AgentRuntime 不感知            ┃
  ┃   Agent Agent Agent        ▼                                        ┃
  ┃   "main" "coder" "..."   AskUserTool ─→ 进 ToolRegistry            ┃
  ┃     ┃                                                                ┃
  ┃     ┗━ Agent 退化为"纯身份"：config + cached_prompt，无 Arc 字段     ┃
  ┃                                                                       ┃
  ┃  ┌──────────────────────────────────────────────────────────┐        ┃
  ┃  │           AgentRuntime (启动时构造的全局 bundle)           │        ┃
  ┃  │   registry / tool_registry / context_engine /             │        ┃
  ┃  │   tool_executor / loop_breaker                            │        ┃
  ┃  │   Agent.run(session, ctx, rt) 时传入                       │        ┃
  ┃  │   turn 起手按 agent.config 过滤 tool_registry → allowed   │        ┃
  ┃  │   spec 进 LLM；tool_executor 在 allowed 内查找            │        ┃
  ┃  └──────────────────────────────────────────────────────────┘        ┃
  ┃                                                                       ┃
  ┃  ┌─────────────────────────────────────────────────────┐             ┃
  ┃  │            DelegationCoordinator                    │             ┃
  ┃  │  references: SessionManager + AgentRegistry         │             ┃
  ┃  │  worktree 编排 + 调 SessionManager.create_session   │             ┃
  ┃  └─────────────────────────────────────────────────────┘             ┃
  ┃                                                                       ┃
  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
  │
  │
  ┣━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
  ┃                       Session 层                                      ┃
  ┃                                                                       ┃
  ┃   ┌───────────────────────────────────────────────────────────┐      ┃
  ┃   │                    SessionManager                          │      ┃
  ┃   │                                                            │      ┃
  ┃   │   backend:   Arc<dyn SessionBackend>     ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶     ┃
  ┃   │   agents:    Arc<AgentRegistry>          ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶     ┃
  ┃   │   resolver:  Arc<UserResolver>           ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶     ┃
  ┃   │                                                            │      ┃
  ┃   │   sessions:  RwLock<HashMap<routing_key,                  │      ┃
  ┃   │                            Arc<SessionContext>>>          │      ┃
  ┃   │     ★ 1:1 不变量：表内 session.id 互不重复                │      ┃
  ┃   │     sub-session 不进此表，调用方持 Arc 管生命周期         │      ┃
  ┃   │                                       ┃                    │      ┃
  ┃   └───────────────────────────────────────╋────────────────────┘      ┃
  ┃                                           ┃ own (HashMap value)        ┃
  ┃                                           ▼                            ┃
  ┃   ┌───────────────────────────────────────────────────────────┐      ┃
  ┃   │              SessionContext (per session)                  │      ┃
  ┃   │                                                            │      ┃
  ┃   │   ┌──────────────────────────────────────┐                │      ┃
  ┃   │   │ session: Mutex<Session>              │                │      ┃
  ┃   │   │  持久化字段：                          │                │      ┃
  ┃   │   │  ├─ id, owner                        │                │      ┃
  ┃   │   │  ├─ history: Vec<ChatMessage>        │                │      ┃
  ┃   │   │  ├─ message_ids                      │                │      ┃
  ┃   │   │  ├─ compact_version, summary_meta    │                │      ┃
  ┃   │   │  ├─ session_override, token_tracker  │                │      ┃
  ┃   │   │  ├─ incomplete_turn                  │                │      ┃
  ┃   │   │  └─ last_message: Option<            │ ← 整条 incoming│      ┃
  ┃   │   │      ChannelMessage>                 │   持久化       │      ┃
  ┃   │   │  transient 字段：                     │                │      ┃
  ┃   │   │  ├─ persist: Option<Arc<PersistHook>>│ ← serde(skip)  │      ┃
  ┃   │   │  └─ channel: Option<Arc<dyn Channel>>│ ← serde(skip)  │      ┃
  ┃   │   │  methods: add_user_text(),           │                │      ┃
  ┃   │   │           add_assistant_text(),      │                │      ┃
  ┃   │   │           add_tool_*(),              │                │      ┃
  ┃   │   │           restore_token_count()      │                │      ┃
  ┃   │   └──────────────────────────────────────┘                │      ┃
  ┃   │                                                            │      ┃
  ┃   │   agent: Arc<Agent>           ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶ (Registry) │      ┃
  ┃   │   user_profile: Arc<UserProfile>  ╌╌╌╌╌╌╌╌╌╌▶ (per user)  │      ┃
  ┃   │                                                            │      ┃
  ┃   │   attachments: Mutex<AttachmentManager>  ← 内存增量通告状态│      ┃
  ┃   │   pending_retry: Mutex<Option<String>>                    │      ┃
  ┃   │   turn_lock: tokio::Mutex<()>            ← turn 串行化     │      ┃
  ┃   │                                                            │      ┃
  ┃   │   process_turn(msg: ChannelMessage, channel, rt) .......▶ Agent  │      ┃
  ┃   └────────────────────────────────────────────────────┬───────┘      ┃
  ┃                                                         ┃              ┃
  ┃                                          调用时构造 ┃              ┃
  ┃                                                         ▼              ┃
  ┃   ┌──────────────────────────────────────────────────────────┐       ┃
  ┃   │    TurnContext<'a> (5 字段，纯本轮决策)                    │       ┃
  ┃   │                                                            │       ┃
  ┃   │  system_prompt: &'a str       ← 已含 profile/runtime/skill │       ┃
  ┃   │  model_id: &'a str            ← override > defaults 已解析 │       ┃
  ┃   │  thinking: Option<&ThinkingConfig>                         │       ┃
  ┃   │  permission_mode (PermissionMode)                          │       ┃
  ┃   │  run_mode (RunMode)                                        │       ┃
  ┃   │                                                            │       ┃
  ┃   │  注意：                                                     │       ┃
  ┃   │  - channel / last_message → session 字段                   │       ┃
  ┃   │  - stream / cancel → Channel trait 内化方法                 │       ┃
  ┃   │  - ask_router / delegator → AskUserTool / DelegateTool     │       ┃
  ┃   │  - tool_executor / loop_breaker / context_engine /         │       ┃
  ┃   │    tool_registry → AgentRuntime (run() 入参)               │       ┃
  ┃   │  - limits → 各执行器内部                                    │       ┃
  ┃   │  - user_profile / attachments → process_turn 边界消化       │       ┃
  ┃   │  - routing_key → 只在 SessionManager 层流转                 │       ┃
  ┃   └──────────────────────────────────────────────────────────┘       ┃
  ┃                                                                       ┃
  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
  │
  │
  ┣━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
  ┃                       Interface 层（消息路由 & 来源）                  ┃
  ┃                                                                       ┃
  ┃   ┌──────────────────────────────────────────────────────────┐       ┃
  ┃   │                     Orchestrator                          │       ┃
  ┃   │                                                           │       ┃
  ┃   │   channels: DashMap<(type, account), Arc<dyn Channel>>    │       ┃
  ┃   │   session_manager: Arc<SessionManager>      ╌╌╌╌╌╌╌╌╌╌╌▶ │       ┃
  ┃   │   agent_runtime:   Arc<AgentRuntime>        ╌╌╌╌╌╌╌╌╌╌╌▶ │       ┃
  ┃   │   ask_router:      Arc<AskRouter>           ╌╌╌╌╌╌╌╌╌╌╌▶ │       ┃
  ┃   │   user_resolver:   Arc<UserResolver>        ╌╌╌╌╌╌╌╌╌╌╌▶ │       ┃
  ┃   │   delegation:      Arc<DelegationCoordinator> ╌╌╌╌╌╌╌╌╌▶ │       ┃
  ┃   │   scheduler:       Arc<Scheduler>           ╌╌╌╌╌╌╌╌╌╌╌▶ │       ┃
  ┃   │   webhook:         Arc<WebhookHandler>      ╌╌╌╌╌╌╌╌╌╌╌▶ │       ┃
  ┃   │                                                           │       ┃
  ┃   │   事件循环：select! {                                       │       ┃
  ┃   │     ChannelMessage  → SessionManager.get_context()        │       ┃
  ┃   │                       → SessionContext.process_turn()     │       ┃
  ┃   │     SchedulerEvent  → 同上                                 │       ┃
  ┃   │     DelegationEvent → 回填到父 session                       │       ┃
  ┃   │     UserAnswer      → AskRouter.fulfill()                  │       ┃
  ┃   │   }                                                        │       ┃
  ┃   └──┬────────────┬─────────────────┬──────────────┬────┬─────┘       ┃
  ┃      │            │                 │              │    │             ┃
  ┃      ▼            ▼                 ▼              ▼    ▼             ┃
  ┃   ┌──────┐  ┌─────────┐  ┌────────┐  ┌───────────┐  ┌──────────┐    ┃
  ┃   │Tele- │  │ Client  │  │ QQBot  │  │ Scheduler │  │  Webhook │    ┃
  ┃   │gram  │  │ Channel │  │Channel │  │(cron+hb)  │  │  Handler │    ┃
  ┃   │Chan  │  │(WebUI WS)│  │        │  │           │  │ (HTTP)   │    ┃
  ┃   └──┬───┘  └────┬────┘  └───┬────┘  └─────┬─────┘  └─────┬────┘    ┃
  ┃      │           │           │             │              │           ┃
  ┃      └───────────┴───────────┴─────────────┴──────────────┘           ┃
  ┃                            │                                          ┃
  ┃                            ▼                                          ┃
  ┃                  ChannelMessage (mpsc 给 Orchestrator)                  ┃
  ┃                                                                       ┃
  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
  │
  │
  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
                              外部（用户、LLM、MCP server）                 ┃
                                                                            ┃
   用户 ─── Telegram / WebUI / QQ ─── Channel.listen() ──────────────────────┛
   LLM ─── HTTPS ─── ServiceRegistry.chat()
   MCP ─── stdio/HTTP ─── McpManager.connect()
```

---

## 引用关系一览表

每个组件持有的 Arc 引用（不计自身字段）：

| 组件 | 持有的引用 |
|------|-----------|
| **Agent** | （无 Arc 字段，仅 config + cached_prompt） |
| **AgentRuntime** | ServiceRegistry, ToolRegistry, ContextEngine, ToolExecutor, LoopBreaker |
| **AskUserTool** | AskRouter（在 ToolRegistry 内部，Agent 不感知） |
| **DelegateTool** | AgentDelegator（DelegationCoordinator impl，Agent 不感知） |
| **ContextEngine** | ServiceRegistry, ToolRegistry |
| **ToolExecutor** | （仅 own timeout 字段；工具池 Agent 传入） |
| **LoopBreaker** | (own max_tool_calls + threshold) |
| **AgentRegistry** | HashMap<name, Arc<Agent>>（拥有） |
| **WorkspaceWatcher** | Arc<RwLock<SkillManager>>, Arc<RwLock<Vec<AgentConfig>>>（拥有 RwLock） |
| **McpManager** | ToolRegistry（注册到里面） |
| **SessionManager** | SessionBackend, AgentRegistry, UserResolver |
| **SessionContext** | Arc<Agent>, Arc<UserProfile>；own Mutex<Session> |
| **DelegationCoordinator** | SessionManager, AgentRegistry |
| **Orchestrator** | SessionManager, AgentRuntime, AskRouter, UserResolver, DelegationCoordinator, Scheduler, WebhookHandler, channels DashMap |
| **WebhookHandler** | event_tx 给 Orchestrator（协议适配器） |
| **Channel trait** | 4 方法：listen / send / push_event(target,event) / cancel_signal(target)；后两个默认 no-op |
| **Session** | Option<Arc<dyn PersistHook>>, Option<Arc<dyn Channel>>（transient） |
| **TurnContext (借用)** | &str system_prompt, &str model_id, Option<&ThinkingConfig>, PermissionMode, RunMode（5 字段，无 stream） |
| `llm_stream::read*()` 模块函数 | 硬编码常量 FIRST_CHUNK_TIMEOUT / MAX_OUTPUT_BYTES |

---

## 生命周期分层

```
                    生命周期长 ─────────────────────────────► 短

┌─────────────┐  ┌──────────────┐  ┌──────────────┐  ┌────────────┐
│ Global      │  │ Per-Agent    │  │ Per-Session  │  │ Per-Turn   │
│ (daemon)    │  │              │  │              │  │            │
├─────────────┤  ├──────────────┤  ├──────────────┤  ├────────────┤
│ GlobalConfig│  │ Agent        │  │ Session      │  │ TurnContext│
│ Service-    │  │   .config    │  │   .history   │  │ TurnResult │
│   Registry  │  │   .cached_   │  │   .token_    │  │ TurnResult │
│ ToolRegistry│  │     prompt   │  │     tracker  │  │            │
│ SkillManager│  │              │  │ Session-     │  │            │
│ McpManager  │  │              │  │   Context    │  │ LoopBreaker│
│ Context-    │  │              │  │   .attach-   │  │   (run() 内│
│   Engine    │  │              │  │     ments    │  │    局部)   │
│ Workspace-  │  │              │  │   .pending_  │  │            │
│   Watcher   │  │              │  │     retry    │  │            │
│ AskRouter   │  │              │  │ UserProfile  │  │            │
│ User-       │  │              │  │              │  │            │
│   Resolver  │  │              │  │              │  │            │
│ Session-    │  │              │  │              │  │            │
│   Manager   │  │              │  │              │  │            │
│ Delegation- │  │              │  │              │  │            │
│   Coord     │  │              │  │              │  │            │
│ Orchestrator│  │              │  │              │  │            │
│ AgentReg    │  │              │  │              │  │            │
│ Channels    │  │              │  │              │  │            │
│ Scheduler   │  │              │  │              │  │            │
│ Webhook     │  │              │  │              │  │            │
│ Session-    │  │              │  │              │  │            │
│   Backend   │  │              │  │              │  │            │
└─────────────┘  └──────────────┘  └──────────────┘  └────────────┘
   只创建一次       启动时 N 个       session 数         每条消息一个
                  （AGENT.md 数）   （可缓存淘汰）         （短暂）
```

---

## 一条用户消息的完整流程

```
① 用户在 Telegram 发消息
       │
       ▼
② TelegramChannel.listen() 收到，转为 ChannelMessage
       │ mpsc(OrchestratorEvent::UserMessage)
       ▼
③ Orchestrator.run() 主循环 select 到事件
       │
       ├── 解析 routing_key = "telegram:bot1:12345"
       ├── routing_key → session_id（查 active）
       ├── 先 AskRouter.fulfill(session_id, ...)（若 pending 有等待则被消费，return）
       │
       ▼
④ session_manager.get_context(routing_key)
       │
       ├── 查 sessions[routing_key] → Arc<SessionContext>（命中）
       │                       或：从 backend 加载 Session → 包成 SessionContext → 插入
       │
       ▼
⑤ session_ctx.process_turn(msg: ChannelMessage, channel, rt)
       │
       ├── turn_lock.lock()                  ← 串行化本 session 的 turn
       ├── session.channel = Some(channel)   ← transient
       ├── session.last_message = Some(msg)  ← 整条 ChannelMessage 持久化
       ├── attachments.diff_and_render(...)  → <system-reminder> 文本
       ├── session.add_user_text(reminder + msg.content)   ← 写 history（自动 persist）
       ├── resolve permission_mode/model/run_mode/thinking
       ├── 组装 system_prompt（agent.cached_prompt + profile + runtime + skills）
       ├── 构造 TurnContext { system_prompt, model_id, thinking, perm, run_mode }
       │
       ▼
⑥ agent.run(&mut session, &turn_ctx, rt)
       │
       │   注：不传 input — 输入信息已在 session.history.last() / session.last_message
       │
       ├── 循环：
       │     ├── rt.context_engine.should_compact(&session.token_tracker, ctx_window)?
       │     │      └─ true → rt.context_engine.execute(...) 调 LLM 做 summary
       │     ├── 构建 ChatRequest（system_prompt + history + tool_specs）
       │     ├── service_registry.chat(req) → LLM 响应
       │     ├── 更新 session.token_tracker
       │     ├── 解析响应：
       │     │     ├─ tool_call → rt.tool_executor.execute(call, &session)
       │     │     │   ├─ AskUserTool.execute(args, session)
       │     │     │   │   ├─ session.channel.send(question, target)  ← target = session.last_message.reply_target
       │     │     │   │   └─ self.ask_router.wait_for_reply(&session.id) → ChannelMessage
       │     │     │   ├─ DelegateTool.execute(args, session)
       │     │     │   │   └─ self.delegator.delegate(agent, task, &session.id)
       │     │     │   │       └─ DelegationCoordinator 编排 worktree + sub_ctx.process_turn
       │     │     │   └─ 其他 tool → session 不变或 add_tool_result
       │     │     ├─ session.add_tool_result(...)                   ← 内部 persist
       │     ├─ text → session.add_assistant_text(text)              ← 内部 persist
       │     └─ stop_reason 决定继续/退出
       │
       └── 返回 TurnResult { text, pending_retry }
       │
       ▼
⑦ SessionContext 保存 pending_retry（若有）
       │
       ▼
⑧ channel.send(response) → 用户
```

---

## 模块归属速查

```
能力定义（"agent 是个什么")
  └─ AgentConfig (AGENT.md front matter + body)

运行时参数（"系统怎么跑")
  └─ GlobalConfig.{prompt, agent, tool_executor, loop_breaker, context_engine, ...}

用户/触发源临时改（"这次这么跑")
  └─ SessionOverride

对话数据（"聊了什么")
  └─ Session (持久化)

运行时容器（"打开了这个 session")
  └─ SessionContext (Session + 绑定的 agent + 内存状态)

执行器（"怎么跑一轮")
  └─ Agent.run() + TurnContext + ContextEngine
```
