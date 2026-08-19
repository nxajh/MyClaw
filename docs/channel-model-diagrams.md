# Channel 模块关系图

> 配合 `docs/channel-model-rfc.md` 阅读

## 1. Channel trait 的方法表（Phase 0-5 全部落地后）

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Channel trait（已实现状态）                                            │
├──────────────────────────────────────────────────────────────────────────┤
│  ── 元信息 ──                                                            │
│  fn name() → &str                                                        │
│  fn capabilities() → &ChannelCapabilities       (Phase 1)               │
│  fn message_len(text) → usize                   (Phase 1, by len_unit)  │
│                                                                          │
│  ── 入站 ──                                                              │
│  async listen() → mpsc::Receiver<ChMsg>                                  │
│                                                                          │
│  ── 出站：结构化 ──                                                       │
│  async send_payload(&SendTarget, &MessagePayload)                       │
│     → Option<MessageId>                          (Phase 2, default ↓)   │
│  async send(&SendMessage)                        (legacy, Phase 4 删)   │
│                                                                          │
│  ── 编辑/删除 ──                                                          │
│  async edit_message(&SendTarget, &MessageId,    (Phase 3, Telegram impl)│
│                     &MessagePayload)                                     │
│  async delete_message(&SendTarget, &MessageId)  (Phase 3, Telegram impl)│
│                                                                          │
│  ── 流式（Phase 1.5 删 push_event/cancel_signal，统一为 TurnStream）─    │
│  fn supports_streaming() → bool                 (default: from caps)    │
│  fn create_stream(reply_target)                 (Phase 1.5)             │
│     → Option<Box<dyn TurnStream>>                                        │
│                                                                          │
│  ── 安全策略（Phase 4 新增）──                                            │
│  fn security_policy() → ChannelSecurityPolicy   (default: open())       │
│  fn check_authorization(sender, scope)          (default: evaluate(…))  │
│     → AuthDecision                                                       │
│                                                                          │
│  ── 状态通知 ──                                                          │
│  async health_check() → bool                                             │
│  async on_status(recipient, status)                                      │
└──────────────────────────────────────────────────────────────────────────┘
```

## 2. 各 Channel 实现持有的数据

```
ClientChannel (WebSocket，supports_streaming=true)
├─ config: ClientConfig
├─ message_tx/rx: mpsc<ChannelMessage>           入站消息通道
├─ pre_bound: Option<TcpListener>                热切换继承的 socket
├─ stream_contexts: RwLock<HashMap<rt,           per-reply_target 流上下文
│      { event_tx, cancel: CancelToken,
│        ws_sender }>>
├─ connections: RwLock<HashMap<conn_id,          WebSocket 连接表
│      { ws_sender, active_session,
│        sessions: HashSet }>>
├─ session_owners: RwLock<HashMap<sid, owner>>   session → routing_key 反查
├─ session_manager: Arc<RwLock<Option<           会话管理 API 调用
│      Arc<SessionManager>>>>
└─ tool_specs / workspace_dir / skill_manager    WebUI 管理面板需要

TelegramChannel (HTTP polling，supports_edit=true)
├─ bot_token / api_base / http: reqwest::Client  Telegram API 客户端
├─ allowed_users: RwLock<Vec<String>>            热重载 DM 白名单
├─ allowed_groups: Option<Vec<String>>           Phase 4 新增；None=拒绝群
├─ mention_only: bool                            群消息要求 @bot
├─ dedup: DedupState                             update_id 去重
├─ debounce_buffer / debounce_ms                 合并连续消息
├─ typing_tasks: HashMap<recipient,JoinHandle>   typing 心跳
├─ pending_acks / status_reactions /             reaction 状态
│  stall_messages: ReactionTracker
├─ stall_timeout_secs                            空跑监控
├─ bot_username: Mutex<Option<String>>           自检后填充
└─ base_dir: PathBuf                             dedup 持久化

QQBotChannel (HTTP + WebSocket)
├─ config: QQBotAccountConfig                    含 allowed_users/allowed_groups
│                                                (Phase 4 重命名自 allow_from/
│                                                 group_allow_from，serde alias)
├─ token_manager: Arc<TokenManager>              OAuth token 刷新
├─ dedup: DedupState
├─ http_client: reqwest::Client
├─ session: Arc<Mutex<Option<SessionState>>>     WS resume 支持
├─ last_seq: Arc<Mutex<Option<u64>>>             心跳序号
├─ typing_tasks: HashMap                         typing 心跳
└─ msg_seq_counter: AtomicU32                    主动消息序号

WechatChannel (HTTP webhook)
├─ api: ApiClient
├─ config: WechatAccountConfig (含 allowed_users)
└─ dedup: DedupState
```

**共性**：
- 全部持有 `dedup: DedupState`（防止 update_id / msg_id 重复）
- 全部持有一个 HTTP client 或自维护的连接

**差异**：
- 只有 ClientChannel 持有 `stream_contexts`（流式）和 `session_owners`（管理 API）
- Telegram 有最多的群组语义状态（mention_only/typing/stall）
- WeChat 最简（只有 dedup + API client）

## 3. 数据流转 — 入站

```
                            外部平台 (WebUI / Telegram / QQ / 企微)
                                         │
                                         ▼
                          ┌──────────────────────────────┐
                          │  各 Channel 内的 listener     │
                          │  · WebSocket accept loop      │
                          │  · HTTP polling loop          │
                          │  · webhook handler            │
                          │                               │
                          │  写入 channel.message_tx      │
                          └──────────────┬───────────────┘
                                         │
                          channel.listen() 返回 rx
                                         │
                                         ▼
                          ┌──────────────────────────────┐
                          │  Orchestrator::spawn_listener  │
                          │  (per channel 一个 task)       │
                          │                                │
                          │  rx → orchestrator.msg_rx      │
                          │       (mpsc<((ct,acc), ChMsg)>)│
                          └──────────────┬───────────────┘
                                         │
                                         ▼
                          ┌──────────────────────────────┐
                          │  Orchestrator::run() 主循环    │
                          │  ① ask_router.fulfill()?  →   │
                          │     命中则消费 不进 process_turn│
                          │  ② 解析命令 / 回调 / 控制消息  │
                          │  ③ session_ctx.process_turn() │
                          └──────────────────────────────┘
```

## 4. 数据流转 — 出站（Phase 0 后的边界）

```
                          ┌──────────────────────────────────────────────┐
                          │  Streaming 路径（仅 supports_streaming=true）│
                          │                                              │
                          │  Agent::run / collect_stream                 │
                          │     │                                        │
                          │     ▼                                        │
                          │  session.channel.push_event(rt, TurnEvent)   │
                          │     │   · Chunk / Thinking / ToolCall        │
                          │     │   · ToolResult / Done                  │
                          │     ▼                                        │
                          │  ClientChannel.stream_contexts[rt].event_tx  │
                          │     │                                        │
                          │     ▼                                        │
                          │  WebSocket frame → WebUI                     │
                          └──────────────────────────────────────────────┘

                          ┌──────────────────────────────────────────────┐
                          │  非 streaming 路径（含 control messages）    │
                          │                                              │
                          │  callers ─┐                                  │
                          │           │                                  │
                          │           ▼                                  │
                          │  channel.send(SendMessage)        (现在)     │
                          │       ↓ Phase 2                              │
                          │  channel.send_payload(             (目标)    │
                          │      &SendTarget,                            │
                          │      &MessagePayload)                        │
                          │           │                                  │
                          │           ▼                                  │
                          │  各 channel 实现转平台 API：                 │
                          │     · ClientChannel: WebSocket JSON          │
                          │     · TelegramChannel: sendMessage HTTP      │
                          │     · QQBotChannel: HTTP + Keyboard          │
                          │     · WechatChannel: ApiClient 推送          │
                          └──────────────────────────────────────────────┘
```

## 5. 哪些模块调用 Channel？以什么方式拿到？

```
                        ┌─────────────────────────────────┐
                        │  Channel 集合的唯一所有者       │
                        │  Orchestrator.channels:         │
                        │  Arc<DashMap<(type,acc),        │
                        │              Arc<dyn Channel>>> │
                        └────┬────────────────────────────┘
                             │  对外通过 Arc::clone 分发
            ┌────────────────┼──────────────────────────────────────┐
            │                │                                      │
            ▼                ▼                                      ▼
┌──────────────────┐  ┌──────────────────┐         ┌────────────────────────┐
│ SessionContext   │  │ Orchestrator      │         │ Scheduler / Webhook    │
│ ::process_turn   │  │ 自己              │         │                        │
│                  │  │                   │         │ ctx.orchestrator       │
│ 入参             │  │ 7 处控制消息：    │         │   .channels()          │
│ channel:         │  │ · /cmd 响应       │         │   .get(&key)           │
│ Option<          │  │ · retry/abort     │         │   ↓                    │
│  Arc<dyn         │  │ · incomplete turn │         │ send_to_target_internal│
│  Channel>>       │  │ · ABORT_ACK       │         │ (Phase 4 改 send_paylo)│
│                  │  │ · MSG_TURN_FAILED │         │                        │
│ 写：             │  │ · recovery        │         │                        │
│ session.channel  │  │ · send_to_target  │         │                        │
└────────┬─────────┘  └──────────────────┘         └────────────────────────┘
         │
         │  session.channel.as_ref() 由 Agent.run 读取
         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Agent::run（Session 内部读取，从不直接持有 channels map）              │
│                                                                          │
│  if let Some(ch) = session.channel.as_ref() {                            │
│      let _ = ch.push_event(reply_target, TurnEvent::Chunk{...}).await;   │
│  }                                                                       │
│  if let Some(token) = session.channel.as_ref()                           │
│        .and_then(|ch| ch.cancel_signal(reply_target)) {                  │
│      if token.is_cancelled() { ... }                                     │
│  }                                                                       │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│  AskUserTool（当前的不一致点 — Phase 2 待清理）                         │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│  ❌ 现在：                                                               │
│      pub struct AskUserTool {                                            │
│          router: Option<Arc<AskRouter>>,                                 │
│          channels: Option<Arc<DashMap<...>>>,  ← 自持 channels map      │
│      }                                                                   │
│      // execute() 内：                                                   │
│      let (ct, acc, _) = split_routing_key(&session.owner);               │
│      let channel = channels.get(&(ct, acc));   ← 字符串解析反查          │
│                                                                          │
│  ✅ Phase 2 后（接口一致性）：                                           │
│      pub struct AskUserTool {                                            │
│          router: Option<Arc<AskRouter>>,                                 │
│          // 不再自持 channels                                            │
│      }                                                                   │
│      // execute() 内：                                                   │
│      let channel = session.channel.as_ref()                              │
│          .ok_or_else(|| "no channel on session")?;                       │
│      channel.send_payload(&target, &MessagePayload::Text {...}).await?;  │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

## 6. AskRouter 与 Channel 的间接关系

```
        Channel.listen() 出 ChMsg
              │
              ▼
        Orchestrator::handle_channel_event(ChMsg)
              │
              │  ① 检查是否有 session 在等 ask_user 回复
              │
              ▼
        ask_router.fulfill(session_id, ChMsg) ── true ──► 消息被消费
              │                                          异步 oneshot 唤醒
              │ false                                    阻塞中的 AskUserTool
              ▼
        正常进入 process_turn / 命令解析

                                ┌─────────────────────────┐
                                │  AskUserTool 在等待     │
                                │  AskRouter.wait_for_    │
                                │  reply(session_id,      │
                                │        timeout)         │
                                │     ↑                   │
                                │     ↓ 收到 ChMsg        │
                                │  解析文本 + 图片附件    │
                                │  返回给 Agent           │
                                └─────────────────────────┘

AskRouter 不直接接触 Channel — 是 Orchestrator 帮它桥接 inbound ChMsg
```

## 7. 接口约定（§13.10 的可视化）

```
                       获取 Channel 的官方途径

┌───────────────────────────────────────────────────────────────────────┐
│ Tool (impl Tool)        │  读 session.channel              ✓          │
│ Agent::run              │  读 session.channel              ✓          │
│ SessionContext          │  接受 channel: Option<…> 入参    ✓          │
│ Scheduler / Webhook     │  orch.channels() accessor 透传   ✓          │
│ Orchestrator 自己       │  自持 channels: Arc<DashMap>     ✓ (唯一)   │
└───────────────────────────────────────────────────────────────────────┘
                                  ❌ 反模式

  自持 channels: ChannelMap + 解析 session.owner → 反查
                          ↑
                  当前 AskUserTool 的形态
                  Phase 2 清理
```

## 8. 流式出口的形态演进（push_event → TurnStream，§7.6）

```
┌────────────────────────────────────────────────────────────────────────┐
│  当前 / Phase 0-1（push_event 形态）                                   │
│                                                                        │
│  Agent::run                                                            │
│     │                                                                  │
│     ▼                                                                  │
│  let _ = session.channel.as_ref()                                      │
│      .map(|ch| ch.push_event(reply_target, ev))?  ← &str 索引          │
│                                                                        │
│  ClientChannel.stream_contexts: HashMap<String, StreamContext>         │
│      ├─ insert on first chunk (隐式)                                   │
│      └─ remove on Done    (隐式，cancel 路径易漏)                      │
│                                                                        │
│  cancel: 单独 Channel::cancel_signal(rt) → Option<CancelToken>         │
└────────────────────────────────────────────────────────────────────────┘

                                ↓ Phase 1.5

┌────────────────────────────────────────────────────────────────────────┐
│  Phase 1.5 后（TurnStream 形态）                                       │
│                                                                        │
│  SessionContext::process_turn                                          │
│     │                                                                  │
│     │  ① 入口：session.turn_stream = ch.create_stream(rt);             │
│     ▼                                                                  │
│  Agent::run                                                            │
│     │                                                                  │
│     │  ② 推送：if let Some(s) = &mut session.turn_stream {             │
│     │              s.push(ev).await?   →  Result<StreamDelivery>       │
│     │          }                          (Pending/Visible/Final)      │
│     │                                                                  │
│     │  ③ cancel：s.cancel_token() — 与 stream 同生命周期               │
│     ▼                                                                  │
│  SessionContext::process_turn                                          │
│     │                                                                  │
│     │  ④ 收尾：正常 → turn_stream.finish().await  (await ack)          │
│     │          错误 → turn_stream.abort().await                        │
│     │          panic → Drop impl 触发 best-effort abort                │
│     ▼                                                                  │
│                                                                        │
│  ClientTurnStream  (owned, per-turn)                                   │
│      ├─ event_tx: mpsc<TurnEvent>                                      │
│      ├─ cancel: CancelToken                                            │
│      ├─ ws_sender: WsSender                                            │
│      ├─ finished: bool                                                 │
│      └─ impl Drop { if !finished { spawn(cancel.cancel()) } }          │
│                                                                        │
│  Channel trait 上少了 push_event / cancel_signal,                      │
│  多了 create_stream(rt) → Option<Box<dyn TurnStream>>                  │
└────────────────────────────────────────────────────────────────────────┘
```

**为什么不留双形态**：see RFC §7.6.6 — `push_event` 和 `TurnStream` 表达同一
能力，并存只增加心智负担；Phase 1.5 强制翻转。

## 9. Security policy 数据流（Phase 4，RFC §14）

```
┌────────────────────────────────────────────────────────────────────────┐
│  Channel 配置                                                          │
│                                                                        │
│  TelegramAccountConfig          QQBotAccountConfig                     │
│  ├─ allowed_users: Vec<String>  ├─ allowed_users: Option<Vec<String>>  │
│  ├─ allowed_groups:             │  (alias: allow_from)                 │
│  │     Option<Vec<String>>      ├─ allowed_groups:                     │
│  └─ mention_only: bool          │     Option<Vec<String>>              │
│                                 │  (alias: group_allow_from)           │
│  WechatAccountConfig                                                   │
│  └─ allowed_users: Vec<String>  ClientConfig                           │
│     (空集 = reject all)         └─ (无 channel-level allowlist；        │
│                                     依赖 WS 连接层 token)              │
└────────────────────────────────────────────────────────────────────────┘
                              │
                              │  Channel::new() 调用 build_security_policy()
                              ▼
┌────────────────────────────────────────────────────────────────────────┐
│  ChannelSecurityPolicy { allowed_users, group_mode, group_allowlist }  │
│                                                                        │
│  AllowList::from_config(Option<Vec<String>>)                           │
│   ├─ None      → All                                                   │
│   ├─ ["*"]     → All                                                   │
│   ├─ []        → Whitelist(empty)  →  reject all (触发 warn_if_locked) │
│   └─ [list]    → Whitelist(list)                                       │
│                                                                        │
│  group_mode 派生：                                                     │
│   Telegram: (allowed_groups, mention_only) →                           │
│             (None, _)      → Reject                                    │
│             (Some, true)   → MentionOnly                               │
│             (Some, false)  → Open                                      │
│   QQBot:    allowed_groups None=Reject, Some=Open（无 @ 概念）         │
│   Wechat:   永远 Reject（无群）                                        │
│   Client:   Open（用 open() 默认）                                     │
└────────────────────────────────────────────────────────────────────────┘
                              │
        Channel::new() 还调用 warn_if_locked_down(&ch) — 启动期一次性
        提醒：allowed_users 空白名单 / 群默认拒绝
                              │
   ─── 运行时 ─────────────────┴──────────────────────────────────────────
                              ▼
┌────────────────────────────────────────────────────────────────────────┐
│  poll_loop / receive_loop 收到入站 inbound                              │
│     │                                                                  │
│     │  ① 计算 sender (username/openid/wxid) 与 scope                   │
│     │     scope = Direct                                               │
│     │           | Group { id, has_mention }                            │
│     ▼                                                                  │
│  channel.check_authorization(sender, scope) → AuthDecision             │
│     │                                                                  │
│     │  默认实现 = security::evaluate(&self.security_policy(), …)       │
│     │  Telegram 重写为 try_authorize：先按 username 试，再按 user_id    │
│     │  试，任一 Allow 即 Allow                                          │
│     ▼                                                                  │
│  match decision {                                                      │
│      Allow                  → forward 给 orchestrator                  │
│      Ignore                 → continue（静默；不 warn）                │
│      Reject { reason }      → warn!(reason); continue                 │
│  }                                                                     │
└────────────────────────────────────────────────────────────────────────┘
```

**关键设计点**：

- `evaluate()` 是**纯函数**——测试 / admin UI / 模拟器 可以脱离 Channel 调用
- `security_policy()` **返回值（不是引用）**，channel 内部 `Arc<RwLock<...>>`
  支持热重载且不暴露锁
- `AuthDecision::Ignore` ≠ `Reject` —— MentionOnly 群没 @ 是预期路径，
  不该 warn 刷屏
- `warn_if_locked_down` 仅在构造期触发一次，避免热路径加噪声

---

**图例**：

- `┌── ──┐` 框：结构体或模块
- `→` / `▼`：数据流方向
- `Arc<…>` / `Mutex<…>`：实际包装类型
- ✓ / ❌：接口一致性判断
- `Phase N`：本 RFC 时间线引用
