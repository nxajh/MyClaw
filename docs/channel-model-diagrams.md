# Channel 模块关系图

> 配合 `docs/channel-model-rfc.md` 阅读

## 1. Channel trait 的方法表（Phase 0 后 → Phase 1-3 后）

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Channel trait                                                          │
├──────────────────────────────────┬──────────────────────────────────────┤
│  当前（Phase 0 后）              │  Phase 1-3 后新增                    │
├──────────────────────────────────┼──────────────────────────────────────┤
│  fn name()                       │                                      │
│  fn supports_streaming()         │  fn capabilities() → &ChannelCaps    │
│                                  │  fn message_len(text) → usize        │
│  async send(&SendMessage)        │  async send_payload(&SendTarget,     │
│                                  │                     &MessagePayload) │
│                                  │     → Option<MessageId>              │
│  async listen()                  │                                      │
│     → mpsc::Receiver<ChMsg>      │                                      │
│  async health_check()            │                                      │
│  async on_status(rt, status)     │                                      │
│                                  │  async edit_message(&SendTarget,     │
│                                  │                     &MessageId,      │
│                                  │                     &MessagePayload) │
│                                  │  async delete_message(&SendTarget,   │
│                                  │                       &MessageId)    │
│  async push_event(rt, TurnEvent) │                                      │
│  fn cancel_signal(rt)            │                                      │
│     → Option<CancelToken>        │                                      │
└──────────────────────────────────┴──────────────────────────────────────┘
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
├─ allowed_users: RwLock<Vec<String>>            热重载白名单
├─ mention_only: bool                            群组只响应 @
├─ dedup: DedupState                             update_id 去重
├─ debounce_buffer / debounce_ms                 合并连续消息
├─ typing_tasks: HashMap<recipient,JoinHandle>   typing 心跳
├─ pending_acks / status_reactions /             reaction 状态
│  stall_messages: ReactionTracker
├─ stall_timeout_secs                            空跑监控
├─ bot_username: Mutex<Option<String>>           自检后填充
└─ data_dir: PathBuf                             dedup 持久化

QQBotChannel (HTTP + WebSocket)
├─ config: QQBotAccountConfig
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
│      ch.push_event(reply_target, TurnEvent::Chunk{...}).await;           │
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

---

**图例**：

- `┌── ──┐` 框：结构体或模块
- `→` / `▼`：数据流方向
- `Arc<…>` / `Mutex<…>`：实际包装类型
- ✓ / ❌：接口一致性判断
- `Phase N`：本 RFC 时间线引用
