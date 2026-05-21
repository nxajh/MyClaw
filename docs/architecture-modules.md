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
   ┌──────────┐             ┌──────────────────────────────────────┐
   │ [runtime]│             │  agents/{name}/AGENT.md              │
   │ [limits] │             │  skills/{name}/SKILL.md              │
   │ [context]│             │  sessions/{sid}/history.jsonl, ...   │
   │ [default]│             │  users/{uid}/profile.md              │
   │ [prompt] │             │  users/{uid}/preferences.md          │
   │ [provid] │             │  users/{uid}/memory/{type}/          │
   │ [channel]│             │  cron/jobs.json                      │
   │ [mcp]    │             │  DEFAULT_USER.md                     │
   │ [users]  │             └──────────────────────────────────────┘
   └─────┬────┘                       │
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
  ┃  ┌─────────────────┐   ┌──────────────┐   ┌─────────▼────────┐       ┃
  ┃  │  ContextEngine  │   │SessionBackend│   │  AgentRegistry   │       ┃
  ┃  │ (compact 逻辑+   │   │ (JSONL 持久化)│   │ HashMap<name,    │       ┃
  ┃  │  registry 引用) │╌╌╌│              │   │   Arc<Agent>>    │       ┃
  ┃  │ ★ 无状态        │   └──────────────┘   └──┬───┬──────┬────┘       ┃
  ┃  └─────────────────┘                          ┃   ┃      ┃            ┃
  ┃           ╲                                   ╋   ╋      ╋            ┃
  ┃            ╲ 每个 Agent 引用：                ▼   ▼      ▼            ┃
  ┃             ╲╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶  Agent  Agent  Agent       ┃
  ┃                          ↑                  "main" "coder" "..."     ┃
  ┃           ServiceRegistry ┘                                            ┃
  ┃           ToolRegistry ────────────────────╌╌▶ (Arc 引用，不拥有)     ┃
  ┃                                                                       ┃
  ┃  ┌─────────────────┐   ┌──────────────┐   ┌─────────────────┐       ┃
  ┃  │   AskRouter     │   │ UserResolver │   │ DelegationCoord │       ┃
  ┃  │ (pending_asks   │   │ (rk→user_id) │   │ (worktree 编排  │       ┃
  ┃  │  注册器)        │   │              │   │  +AgentRegistry)│       ┃
  ┃  └─────────────────┘   └──────────────┘   └────────┬────────┘       ┃
  ┃                                                     ┃                ┃
  ┃                                                     ╋                ┃
  ┃                                  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶  references     ┃
  ┃                                  AgentRegistry, SessionManager        ┃
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
  ┃   │   active:    RwLock<HashMap<routing_key, session_id>>     │      ┃
  ┃   │   contexts:  RwLock<HashMap<session_id,                   │      ┃
  ┃   │                            Arc<SessionContext>>>          │      ┃
  ┃   │                                       ┃                    │      ┃
  ┃   └───────────────────────────────────────╋────────────────────┘      ┃
  ┃                                           ┃ own (HashMap value)        ┃
  ┃                                           ▼                            ┃
  ┃   ┌───────────────────────────────────────────────────────────┐      ┃
  ┃   │              SessionContext (per session)                  │      ┃
  ┃   │                                                            │      ┃
  ┃   │   ┌──────────────────────────────────────┐                │      ┃
  ┃   │   │ session: Mutex<Session>              │                │      ┃
  ┃   │   │  ├─ history: Vec<ChatMessage>        │ ← 唯一 owner   │      ┃
  ┃   │   │  ├─ message_ids, compact_version    │                │      ┃
  ┃   │   │  ├─ session_override                 │                │      ┃
  ┃   │   │  ├─ token_tracker                    │                │      ┃
  ┃   │   │  ├─ incomplete_turn, last_reply_targ │                │      ┃
  ┃   │   │  └─ persist: Option<Arc<PersistHook>>│ ← 自己负责落盘 │      ┃
  ┃   │   │     methods: add_user_text(),        │                │      ┃
  ┃   │   │              add_assistant_text(),   │                │      ┃
  ┃   │   │              add_tool_*(), ...       │                │      ┃
  ┃   │   └──────────────────────────────────────┘                │      ┃
  ┃   │                                                            │      ┃
  ┃   │   agent: Arc<Agent>           ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶ (Registry) │      ┃
  ┃   │   user_profile: Arc<UserProfile>  ╌╌╌╌╌╌╌╌╌╌▶ (per user)  │      ┃
  ┃   │                                                            │      ┃
  ┃   │   attachments: Mutex<AttachmentManager>  ← 内存增量通告状态│      ┃
  ┃   │   pending_retry: Mutex<Option<String>>                    │      ┃
  ┃   │   turn_lock: tokio::Mutex<()>            ← turn 串行化     │      ┃
  ┃   │                                                            │      ┃
  ┃   │   process_turn(input, channel, reply_target, env) ........▶ Agent│      ┃
  ┃   └────────────────────────────────────────────────────┬───────┘      ┃
  ┃                                                         ┃              ┃
  ┃                                          调用时构造 ┃              ┃
  ┃                                                         ▼              ┃
  ┃   ┌──────────────────────────────────────────────────────────┐       ┃
  ┃   │    TurnContext<'a> (per turn 借用 — 已 resolve 的执行入参)  │       ┃
  ┃   │                                                            │       ┃
  ┃   │  ── 运行参数（process_turn 解析后传入标量/借用）──            │       ┃
  ┃   │  system_prompt: &'a str       ← 已含 profile/runtime/skill │       ┃
  ┃   │  model_id: &'a str            ← override > defaults 已解析 │       ┃
  ┃   │  thinking: Option<&ThinkingConfig>                         │       ┃
  ┃   │  permission_mode, run_mode (枚举值)                         │       ┃
  ┃   │  max_tool_calls, tool_timeout_secs, ... (limits 抽出)      │       ┃
  ┃   │                                                            │       ┃
  ┃   │  ── 工具回弹路径 ──                                          │       ┃
  ┃   │  channel: &'a dyn Channel     ← process_turn 参数          │       ┃
  ┃   │  reply_target: &'a str                                    │       ┃
  ┃   │  ask_router: &'a AskRouter      ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▶         │       ┃
  ┃   │  delegator: &'a dyn AgentDelegator  ╌╌╌╌╌╌╌╌╌╌╌╌▶         │       ┃
  ┃   │                                                            │       ┃
  ┃   │  ── 流式 ──                                                  │       ┃
  ┃   │  stream: Option<TurnStream<'a>>   ← 仅 Streamed 模式       │       ┃
  ┃   │                                                            │       ┃
  ┃   │  注意：不传 persist（Session 自负责），不传 user_profile     │       ┃
  ┃   │       / GlobalConfig / SessionOverride / AttachmentManager │       ┃
  ┃   │       ——这些在 process_turn 边界全部消化掉                  │       ┃
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
| **Agent** | ServiceRegistry, ToolRegistry, Arc<RwLock<SkillManager>>, ContextEngine |
| **ContextEngine** | ServiceRegistry, ToolRegistry |
| **AgentRegistry** | HashMap<name, Arc<Agent>>（拥有） |
| **WorkspaceWatcher** | Arc<RwLock<SkillManager>>, Arc<RwLock<Vec<AgentConfig>>>（拥有 RwLock） |
| **McpManager** | ToolRegistry（注册到里面） |
| **SessionManager** | SessionBackend, AgentRegistry, UserResolver |
| **SessionContext** | Arc<Agent>, Arc<UserProfile> |
| **DelegationCoordinator** | SessionManager, AgentRegistry |
| **Orchestrator** | SessionManager, AskRouter, UserResolver, DelegationCoordinator, Scheduler, WebhookHandler, channels DashMap |
| **Session** | Option<Arc<dyn PersistHook>>（自负责持久化） |
| **TurnContext (借用)** | &str system_prompt, &str model_id, &Channel, &AskRouter, &AgentDelegator, Option<TurnStream> |

---

## 生命周期分层

```
                    生命周期长 ─────────────────────────────► 短

┌─────────────┐  ┌──────────────┐  ┌──────────────┐  ┌────────────┐
│ Global      │  │ Per-Agent    │  │ Per-Session  │  │ Per-Turn   │
│ (daemon)    │  │              │  │              │  │            │
├─────────────┤  ├──────────────┤  ├──────────────┤  ├────────────┤
│ GlobalConfig│  │ Agent        │  │ Session      │  │ TurnContext│
│ Service-    │  │   .config    │  │   .history   │  │ TurnInput  │
│   Registry  │  │   .cached_   │  │   .token_    │  │ TurnResult │
│ ToolRegistry│  │     prompt   │  │     tracker  │  │ TurnStream │
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
       │ mpsc
       ▼
③ Orchestrator.run() 主循环 select 到 ChannelMessage
       │
       ├── 解析 routing_key = "telegram:bot1:12345"
       ├── 检查 AskRouter.fulfill()（若 pending_asks 有等待，oneshot 投递后跳出）
       │
       ▼
④ session_manager.get_context(routing_key)
       │
       ├── 查 active[routing_key] → session_id
       ├── 查 contexts[session_id] → Arc<SessionContext>（命中）
       │                       或：从 backend 加载 Session → 包成 SessionContext
       │
       ▼
⑤ session_ctx.process_turn(input, channel, reply_target, env)
       │
       ├── turn_lock.lock()                ← 串行化本 session 的 turn
       ├── 构造 TurnContext（borrow attachments / user_profile / global / ...）
       │
       ▼
⑥ agent.run(&mut session, input, &turn_ctx)
       │
       ├── attachments.diff_and_render(...) → <system-reminder> 文本
       ├── 拼用户消息 → session.history.push()
       ├── 持久化：turn_ctx.persist.persist_message(...)
       │
       ├── 循环：
       │     ├── context_engine.should_compact(&session.token_tracker, ctx_window)?
       │     │      └─ true → context_engine.execute(...) 调 LLM 做 summary
       │     ├── 构建 ChatRequest（system_prompt + history + tool_specs）
       │     ├── service_registry.chat(req) → LLM 响应
       │     ├── 更新 session.token_tracker
       │     ├── 解析响应：
       │     │     ├─ tool_call → tool_executor.execute()
       │     │     │   ├─ ask_user → turn_ctx.ask_router.ask(...)
       │     │     │   │     └─ channel.send(question); 挂 oneshot
       │     │     ├─ delegate → turn_ctx.delegator.delegate_async(...)
       │     │     │   └─ DelegationCoordinator 编排 worktree + sub_ctx.process_turn
       │     │     ├─ 其他 tool → tool_impls 查表执行
       │     │     └─ 写 tool result 到 session.history + persist
       │     ├─ text → session.history.push(assistant) + persist
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
  └─ GlobalConfig.{limits, context, defaults, prompt}

用户/触发源临时改（"这次这么跑")
  └─ SessionOverride

对话数据（"聊了什么")
  └─ Session (持久化)

运行时容器（"打开了这个 session")
  └─ SessionContext (Session + 绑定的 agent + 内存状态)

执行器（"怎么跑一轮")
  └─ Agent.run() + TurnContext + ContextEngine
```
