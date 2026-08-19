# Agent 间消息 RFC（Agent Messaging）

> 状态:草案(2026-08-08 讨论收敛,待实现)
> 范围:`tools/send_message.rs` + `agents/delegation.rs` + `agents/known_users.rs` + `agents/commands/` 新增 friends 模块 + `agents/attachment.rs`
> 原则:**消息必须进入接收方 agent 上下文,不允许旁路插入**;协调/授权决策留在框架层,agent 层只做业务。

## 0. 背景

Claude Code v2.1.224 引入 cross-session messaging(SendMessage/ListAgents)。对其源码做缺陷评估(`memory/claude_code_cross_session_messaging.md`)后,导出 6 条缺陷与 4 条借鉴要点:

| 缺陷 | 本设计的回应 |
|---|---|
| 信任域与介质错配(控制面消息放无认证文件) | 授权前置:好友机制,框架层关系表校验 |
| 身份不可区分(跨 session 消息混入 user prompt) | 消息统一以 `<system-reminder>` 注入,来源显式标注 |
| 无 Ack、投递路径三套 | P0 内存直连 + 队列注入;P1 起补 Ack/持久化 |
| 接收侧权限模型缺失(crossSessionInbound 未落地) | contacts 关系表 + 每轮注入 pending 列表,接收侧有真实策略 |
| auto-resume 耦合唤醒与指挥 | 消息注入与任务唤醒分开设计(见 §3.4) |
| 可观测性缺失 | 消息带 msg_id,日志贯穿(§3.6) |

## 1. 目标与非目标

**目标**
- `send_message` 扩展可选 `recipient` 参数:主 agent ↔ 子 agent 通信(P0),跨用户好友通信(P1)。
- 好友授权机制:slash command + 工具双通道,写同一张 contacts 表。
- 每个用户唯一 `user_id`;别名可重复,ID 唯一。

**非目标**
- 不做子 agent ↔ 子 agent 通信(用户明确:暂不必要)。
- 不引入"超级 agent"/LLM 仲裁者(路由与协调是框架的确定性职能)。
- 不做陌生人发现/搜索(好友机制管授权,不管发现)。
- 不改 `SessionKey`/`SubAgentKey` 值类型语义(`agents/orchestrator/key.rs`)。

## 2. 身份模型

```
别名(@昵称)  ──关系表消歧──→  user_id(唯一,机器寻址)  ──→  用户级 mailbox(投递目标,§3.5)
   可重复,人看的                contacts 表键;跨渠道不失效            任意 session 下一条消息触发注入
```

- `user_id` 体系已存在(`agents/user_profile.rs`,RFC v2 §三.D):`UserResolver` 默认 `user_id == routing_key`,operator 可折叠多 routing_key 到同一 user_id。
- 好友关系**建在 user_id 上**(非 session_key):bob 信任的是"alice 这个人",换设备/渠道不失效。
- 别名消歧规则:只可能在**已建立关系的人**中消歧——你无法 @ 一个陌生人;昵称按关系存储,**不同好友可重名**,关系中记录对方完整 user_id,无歧义。
- 附带收益:好友关系建立时双方 user_id 自然沉淀,身份折叠从"operator 手动配置"走向"用户行为驱动"。

### 2.1 跨渠道身份绑定(P3,方案 B:用户主动)

```
新渠道 /link @昵称 ──→ 旧渠道推送一次性验证码(框架模板,零 LLM token)
        │                                       │
        │      ┌────────────────────────────────┘
        ▼      ▼
新渠道 /link_confirm 验证码 ──验证通过──→ resolver.set(rk, uid) + migrate_identity
                                            │
                                            ▼
                         好友 / 消息 / 记忆按"人"共享(折叠身份)
```

- 绑定由**用户行为驱动**:在新渠道 `/link @昵称` 认领身份,框架向**被认领账号**推送 6 位一次性验证码(框架模板直发,零 LLM token),新渠道回 `/link_confirm 验证码` 证明同时掌握两渠道,验证通过后 `resolver.set(rk, uid)` + `migrate_identity`。
- 验证码安全边界:6 位数字、10 分钟 TTL、3 次错误作废、不能绑自己、已绑定渠道拒绝重复绑定(暂无解绑,报错提示);目标渠道不可达时回滚 pending。
- 绑定仅**命令通道**(同 block/unblock 哲学),不进工具集——安全敏感操作,防 LLM 误触发。
- `UserResolver` 持久化(`{base_dir}/user_resolver.json`,version 1,`set()` 即写盘):绑定关系跨重启保留;待确认验证码状态仅内存(重启后重新 `/link` 即可)。
- 折叠语义:contacts / mailbox / sender 身份 / last_seen 一律经 `resolve_uid()` 按"人"折叠;`users` 表保持 routing_key 键(登记簿语义,渠道级活跃度各自记录)。
- `migrate_identity(old_rk, new_uid)`:绑定成功时把旧 rk 的 mailbox 并入新身份、owner 维度联系人合并、其他好友侧指向旧 rk 的关系重指新身份(昵称跟随折叠身份)。

### 2.2 用户实体层(P4:邮箱 + uid 双标识 + 用户自助)

> **已被 `identity-id-rfc.md` 取代(2026-08-09)**:身份标识部分——uid 改系统分配 uuidv7、username 取代 nickname、@username 全局提及、统一 `<ns>/<type>/<uuidv7>` ID 层、`[system] namespace` 配置、5 项数据迁移——全部以 `docs/identity-id-rfc.md` 为准。
>
> 本节保留仍有效的内容:
> - 邮箱唯一性、`/email set` 更换、SMTP 混合验证、gate 拦截引导(白名单微调:`/nickname` 移除、`/username` 新增)
> - `/link` `/link_confirm` 绑定流程(P3 验证码机制复用,target 为 user.id 或 email)
> - contacts / mailbox / sender 身份一律以 user.id 为键;Session.owner 保持 routing_key;known_users.json 保持登记簿语义
> - 输出渲染 `<ref id="…"/>` 标签 + 双层白名单机制(标签层只认 `<ref>`,id 层验 namespace 前缀);类型段与实例格式按新 RFC
## 3. 消息通道

> **统一入口**:所有 agent 间消息(主↔子、跨用户)发送侧统一走 `send_message` 工具,不引入第二工具。§3.4 的 `DelegationEvent::Message` 与 AttachmentManager 队列是**接收侧框架内部机制**——它们把 `send_message` 投递的消息送入接收方 agent 的 turn,不是独立通信通道。

### 3.1 send_message 扩展

现有 schema 只有 `text` + `files`(隐含目标 = 当前会话用户,`tools/send_message.rs`)。扩展一个可选参数:

```json
send_message(text, files?, recipient?)
```

| recipient | 发送者 | 语义 |
|---|---|---|
| 省略 | 主 agent | 发给当前用户(channel 输出,现状零改动) |
| 省略 | 子 agent | 发给 parent 主 agent |
| task_id | 主 agent | 发给运行中的 async 子 agent |
| parent | 子 agent | 发给父主 agent(唯一合法目标) |
| @昵称 | 主 agent | 发给已建立好友关系的用户(P1) |

### 3.2 路由与注入

目标解析三条链,按 recipient 值类型分派:
- `recipient` 省略 → 目标由**上下文绑定**:主 agent 上下文 = 当前用户(channel,现状零改动);子 agent 上下文 = parent(唯一合法目标,见 §3.3)。
- `recipient = task_id` → 解析 SubAgentKey,发给运行中的 async 子 agent(P0)。
- `recipient = parent` → 当前子 agent 的父主 agent(P0)。
- `recipient = @昵称` → contacts 消歧 → user_id → 用户级 mailbox(P1,§3.5)。

注入格式:
  - 主 agent 收到:`<system-reminder> 来自子代理 {name} 的消息:{text}`
  - 子 agent 收到:`<system-reminder> 来自主代理的消息:{text}`
  - 跨用户:`<system-reminder> 来自 @{nick} 的消息:{text}`(P1)

### 3.3 子 agent 权限:信息不可见

不设矩阵、不默认关工具——**子 agent 上下文只注入 parent 一个合法目标**,它不知道任何其他地址。工具可见性 = 权限边界,与 `send_message` 现状"非 channel 上下文不可用"同一哲学。

### 3.4 接收侧语义

| 场景 | 机制 | 是否唤醒 |
|---|---|---|
| 子 agent 运行中收到主 agent 消息 | AttachmentManager 队列注入,下一轮 tool round 看到(不打断运行) | 否 |
| 主 agent 收到子 agent 消息 | `DelegationEvent::Message` → orchestrator wake(不伪造 ChannelMessage) | **是** |
| 主 agent 收到跨用户好友消息(P1) | 写入接收方**用户级 mailbox**(键=user_id,非 session);不唤醒;任意 session 下一条用户消息到来时注入,注入即消费 | **否** |

- **wake 语义(不抢占)**:主 agent 正在处理用户 turn 时子消息到达 → 排队,当前 turn 结束后下一轮注入;主 agent 空闲/等待 → 立即唤醒。wake 只负责"从空闲拉起来",不打断进行中的 turn。
- **子 agent 收消息窗口**:队列注入仅在子 agent 仍有下一轮 tool round 时可见;若消息到达时子 agent 已收尾(无后续 round),消息不丢——附加到 `DelegationEvent::Completed` 随结果带回主 agent。
- **仅 async 子 agent 可收**:sync 调用阻塞等待结果,无接收窗口,不支持(显式报错)。
- 唤醒与指挥分离:消息注入只提供信息;任务继续/终止仍由主 agent 通过 `agent_delegate`/`agent_kill` 决策。

### 3.5 跨用户投递链路(P1 时序)

```
alice 的 bot(turn 中)
  └─ send_message(recipient=@bob, text)
       → 框架:contacts 校验(alice→bob 关系 == accepted)
       → 解析 @bob → user_id → 写入 bob 的**用户级 mailbox**(键=user_id,持久化,msg_id)
       → 返回 alice 的 bot:已投递(Ack)
bob 任意 session 下一条用户消息(用户驱动,不唤醒)
  └─ AttachmentManager 注入:<system-reminder> 来自 @alice 的消息:{text}(注入即消费,其他 session 不再显示)
       → bob 的 agent 有上下文,bob 回复可被引用
bob 回复(需转发时)
  └─ bob 的 bot 调 send_message(recipient=@alice, ...) → 同链反向 → P2 闭环
```

> 用户级 mailbox 消解了"多 routing_key 投哪个"的选择问题:不主动挑 session,任何 session 的下一条用户消息都触发注入,谁先到谁消费(注入即标记已读)。

与 P0 的差异:**跨用户消息不唤醒接收方 agent**(接收方 agent 由用户驱动),只进用户级 mailbox,等任意 session 下一条用户消息触发注入;P0 的子 agent 汇报必须唤醒主 agent(DelegationEvent::Message)。两条路径实现不同,不可混用。

### 3.6 消息可观测性

- 每条消息带 `msg_id`(UUID)。
- `DelegationEvent::Message { msg_id, sender_name, task_id, session_id, text }` 与 `Completed/Failed` 并存,日志可追溯完整消息图。
- 字段语义:子→主方向 `task_id` = 子 agent 自己的 task_id(身份),`sender_name` = 子 agent 的 label(delegate 时记录);主→子方向 `task_id` = 目标子 agent,`sender_name` = "主 agent"。

### 3.7 消息长度与分片(已定)

三层模型,**分片只发生在渲染层**:

| 层 | 机制 | 参照 |
|---|---|---|
| 发送侧(send_message 调用) | 单条 text 硬上限 **32K chars**,超限返回明确错误(不截断、不分片;超长内容建议走 files 通道) | Claude Code `maxResultSizeChars: 100_000` 的"明确上限"思路;取 32K 因单条约 8K tokens,过大挤占接收方 context(用户拍板) |
| 注入侧(接收方 context) | 每轮待收消息**总量预算**(≤2K tokens):超预算注入最近 N 条完整消息 + "还有 M 条未读"提示;不截断单条 | **已实现**(P0):`INJECTION_BUDGET_TOKENS=2048`,`select_within_injection_budget` 保留最近预算内完整消息,旧消息经 `SubAgentMailbox.tx` 放回队列等下一轮 tool round(不丢不截断);超预算时 `Agent::run` 记 warn 日志。与 §4.3"每轮注入保持极简"同一哲学 |
| 渲染侧(P2 转发到 channel) | 复用 `split_message_chunk`(B+ fence-aware,按目标 channel limit/unit,`src/channels/message.rs:676`) | 现有实现,不新设计 |

原则:**单条消息内容永不静默截断**——agent 间消息是内容不是 tool 输出,`truncation.rs` 的 head/tail 策略不适用(截断=丢信息且接收方不知情)。明确错误优于静默降级(与 P0 验收"发给不存在 task_id 返回明确错误"同哲学)。

## 4. 好友机制(P1)

### 4.1 状态机与数据模型

contacts 表,键 = `user_id`,持久化于 `known_users.json` 扩展(现有 60s flush 机制):

```json
{
  "contacts": {
    "<bob_user_id>": {
      "<alice_user_id>": {
        "status": "pending" | "accepted" | "declined" | "blocked",
        "direction": "in" | "out",
        "nickname": "@alice",
        "requested_at": "...",
        "accepted_at": "..."
      }
    }
  }
}
```

双边记录示例(同一关系,两侧各一条,direction 为各自视角):
```json
alice 侧: contacts["<alice_user_id>"]["<bob_user_id>"] = { "status": "accepted", "direction": "out", "nickname": "@bob", ... }
bob 侧:   contacts["<bob_user_id>"]["<alice_user_id>"] = { "status": "accepted", "direction": "in",  "nickname": "@alice", ... }
```

发送校验:alice 发消息给 bob 时,查 **bob 名下 alice 条目** == `accepted`(即 bob 接受过 alice 的请求)。

状态机:`pending → accepted / declined / blocked`;`accepted → removed`(用户操作);`declined → pending`(24h 后重新申请);`blocked → removed`(用户 unblock 解除拉黑,回到无关系,需重新走请求流程)。

规则:
- **双向好友**:alice 加 bob 且 bob 加 alice 才能互发。
- **拒绝后 24h 限流**:同对关系 24h 内只能发一次请求(限流对齐 per-routing_key 30/min 体系)。
- **pending 重发幂等**:请求挂起期间重复发起 → 不重复通知,响应提示"对方尚未处理,请耐心等待"。
- **拉黑/解除拉黑仅用户操作**:`/friend_block` / `/friend_unblock`,无自动路径。
- 授权主体是**用户级**(bob 信任 alice 这个人),非 bot 实例。

### 4.2 双通道(同一张表,同一状态机)

**slash command 通道**(用户直接操作,确定性,绕过 LLM):
```
/friends                 查看 pending / 已建立列表
/friend_request @alice   发起请求(显式;send_message 未建立关系时也会走请求流程)
/friend_accept @alice    接受
/friend_decline @alice   拒绝
/friend_block @alice     拉黑(仅用户操作)
/friend_unblock @alice   解除拉黑(回到无关系,需重新请求)
/friend_remove @alice    解除已建立关系
```
实现:`agents/commands/` 新模块 `friends.rs`,`dispatch` match 加分支(拦截点 `agents/orchestrator/inbound.rs:294` 机制现成)。命令按 session 归属天然隔离(bob 的命令只改 bob 的表)。

**工具通道**(bot 替用户操作,LLM 理解意图):
- `friend_accept` / `friend_decline` / `friend_list` / `friend_request` 注册进主 agent 工具集(block/unblock 仅用户操作,不进工具集)。
- `send_message(recipient=@alice)` 未建立关系 → 返回"发送好友请求?"→ bot 确认后调 `friend_request(@alice)`。

### 4.3 通知与注入

- **通知:框架模板 proactive 推送一次**(不走 LLM,零 LLM token;复用 `known_users.rs` 的 `users_for` 能力):
  `📩 @alice 请求与你建立联系。用 /friends 查看,或直接告诉我处理。`
- **不重复提醒**:请求挂起期间只通知一次,用户可用 `/friends` 查看。
- **每轮注入**(有 token 成本,故保持极简):pending 请求以 `<system-reminder>` 注入接收方 agent(`你有 1 条待处理好友请求:@alice,发送于 xx:xx`),保证用户随时回复"接受"时 agent 有上下文(回应"上下文一致性"问题)。
- **回执**:接受/拒绝后,框架模板通知发起方(`@bob 已接受你的好友请求` + 注入上下文)。

## 5. 无超级 agent

协调职能在框架层(Orchestrator 总线),agent 层只做业务。路由是 `match` 语句(确定性、可单测),不是 LLM 决策。跨用户消息经 Orchestrator 的 contacts 校验后转发,不存在全局 LLM 仲裁者。若未来出现全局任务池/能力发现需求,优先用框架(Scheduler 队列/注册表匹配),不引入 LLM 中介。

## 6. 阶段划分与验收

### P0:主 ↔ 子通信

改动:
- `tools/send_message.rs`:加 `recipient` 参数 + 目标解析(task_id / parent);子 agent 上下文启用该工具,target 仅 parent(现状"非 channel 上下文不可用"需放开,并收紧到单一合法目标)
- `agents/delegation.rs`:加 `DelegationEvent::Message`
- `agents/orchestrator/delegation.rs`:wake 处理 Message
- `agents/attachment.rs`:子 agent 队列注入(日期 + 待收消息)

验收:
- [x] 主 agent 可向运行中 async 子 agent 发消息,子 agent 下一轮 tool round 看到（`send_message recipient=task_id` → mailboxes → `Agent::run` 预算内注入）
- [x] 子 agent 可向 parent 发消息,主 agent 被唤醒且收到注入（`DelegationEvent::Message` → orchestrator wake,不抢占 turn_lock）
- [x] 子 agent 上下文无任何其他 agent 地址(信息不可见,单测断言 `sub_agent_rejects_foreign_recipient`)
- [x] 发给不存在 task_id 返回明确错误(不静默丢)
- [x] 全量测试 + clippy `-D warnings` 通过（run 31252364288 全绿,688 passed）

### P1:好友机制 + 跨用户通信

改动:
- `agents/known_users.rs`:contacts 表 + 状态机 + 限流
- `agents/commands/friends.rs`:7 个命令
- `tools/friend_*.rs`:4 个工具(accept / decline / list / request)
- `send_message` recipient=@昵称 解析链
- 通知模板 + 每轮注入 + 回执

验收:
- [x] 请求全流程:发起 → 通知一次 → 接受 → 双方可互发
- [x] 拒绝后 24h 内同对重发被拒
- [x] 拉黑后不可再投递,仅用户可解除
- [x] 未建立关系时任意投递被框架拦截
- [x] 别名消歧:同名不同 user_id 解析正确

### P2:回复转发闭环 + 会话发现

- bob 回复好友消息 → 转发回 alice 的 agent(闭环)
- 会话发现(已知好友的在线/活跃状态)

验收:
- [x] bob 回复 → 同链反向（send_message @alice → 用户级 mailbox）→ alice 下次交互注入,双向闭环（单测 `cross_user_reply_loop_back_to_sender`）
- [x] 注入文本带回复引导（「如需回复，使用 send_message 工具（recipient=@昵称）」）
- [x] 好友活跃状态可查:`/friends` 命令与 `friend_list` 工具显示 🟢 在线(<5min)/🟡 最近活跃(<24h)/⚪ 离线 + 相对时间（数据源 `KnownUser.last_seen_ms`,每次互动更新）
- [x] 全量测试 + clippy `-D warnings` 通过（run 31256080368 全绿）

### P3:跨渠道身份绑定(方案 B:用户主动)

改动:
- `agents/user_profile.rs`:`UserResolver` 持久化(`persistent(base_dir)` + `user_resolver.json`,version 1,`set()` 即写盘)
- `agents/known_users.rs`:`with_resolver()` / `resolve_uid()` 折叠接入 + `migrate_identity()`(mailbox 合并、owner 维度合并、peer 键重指)
- `agents/commands/link.rs`(新):`/link` + `/link_confirm` 命令(验证码绑定流程,仅命令通道)
- `tools/send_message.rs` + `commands/friends.rs`:sender 身份折叠(`resolve_uid`)
- `daemon.rs`:user_resolver 与 known_users 共享 base_dir 接线

验收:
- [x] 新渠道 `/link @昵称` → 被认领账号所在渠道收到 6 位一次性验证码(框架模板直发,零 LLM token)
- [x] `/link_confirm 验证码` 验证通过 → `resolver.set` + `migrate_identity`,两渠道共享好友/消息/记忆(单测 `migrate_identity_merges_mailbox_and_contacts` / `cross_user_folds_linked_identity`)
- [x] 安全边界:10 分钟过期 / 3 次错误作废 / 不能绑自己 / 已绑定渠道拒绝重复绑定 / 目标渠道不可达回滚 pending
- [x] 绑定持久化:重启后 `resolve(rk)` 仍返回折叠身份(`user_resolver.json` roundtrip 单测)
- [x] 全量测试 + clippy `-D warnings` 通过（run 31257540524 全绿,718 passed）

### P4:用户实体层(邮箱 + uid 双标识 + 用户自助)

改动(第一波):
- `agents/user_registry.rs`(新):UserRegistry + `users.json`(version 1)持久化(`persistent(base_dir)`,每次变更写盘);`User{uid,email,nickname,active,created_ms}`;register / set_email(旧邮箱释放+唯一性回滚) / set_nickname / find_by_uid / find_by_email(大小写归一);validate_uid(`[a-z0-9_]+` 3–32 位 + 保留字 root/admin/system/bot/help/register)/ validate_email / validate_nickname(不允许 `/`);ensure_root
- `agents/commands/register.rs`(新):`/register <邮箱> <uid>`(创建 User + 当前 rk 经 resolver 绑定 FQID + migrate_identity 折叠;已注册拒绝)/ `/email set <邮箱>` / `/nickname set <昵称>`;`pub(crate) parse_target`(u/uid / 完整 FQID / 邮箱;`@昵称` → 第二波明确报错)命令/工具层共用
- `agents/orchestrator/inbound.rs`:Gate 拦截器挂 chain 第 4 位(AskReply→Callback→CrashRecovery→Gate→SlashCommand→DispatchTurn);白名单 register/email/link/link_confirm/help/whoami;未注册 rk 的其余入站直接框架模板回复(GATE_PROMPT,零 LLM token),注册判定 = `user_registry.is_user_id(resolve_uid(rk))`
- `agents/known_users.rs`:删 ContactEntry.nickname 快照(显示实时化)+ `rk_keys`/`rekey_legacy_to`(迁移用);提醒文案 recipient 渲染为 u/uid 或邮箱
- `tools/friends.rs` + `tools/send_message.rs`:入参 nick→target 全走 parse_target(只见 id/email);显示经 UserRegistry.display() 实时渲染 `@昵称(u/uid)`(无昵称 → `u/uid`);`UserMail.sender_nickname` 降级纯显示字段
- `agents/commands/info.rs`:`/whoami` 重写(注册态判定;显示 u/uid + email/nickname(无则 —)+ Messages/First seen/Last seen/Scope;未注册附 /register、/link 引导)
- `daemon.rs`:UserRegistry::persistent + `migrate_legacy_to_root`(启动一次性迁移:存量全部 rk + user_resolver 已有绑定归 root User `myclaw/u/root`;幂等,root 已存在即跳过)
- `agents/commands/link.rs`:目标解析改 parse_target(绑同身份幂等/异身份拒绝);用户文案 `/link @昵称` → `/link u/uid`

验收:
- [x] 新 rk `/register <邮箱> <uid>` 创建身份并绑定当前渠道,`users.json` 持久化;uid 先到先得不可变(注册后改绑拒绝,防孤儿用户)
- [x] uid/email/nickname 校验:uid `[a-z0-9_]+` 3–32 位 + 保留字拒绝;email 小写归一全局唯一、占用即拒绝;昵称可重复、不允许 `/`(可含空格)(单测 `parse_target_*` / `register_roundtrip_email_nickname_updates`)
- [x] Gate 拦截:未注册 rk 白名单命令放行、其余返回框架引导文案(零 LLM token,不进 agent loop);注册后放行(chain 测试含 gate)
- [x] 存量迁移幂等:启动时全部 known_users rk + resolver 绑定归 root(`myclaw/u/root`),存量渠道无需注册直接可用;迁移后新 rk 才走拦截引导
- [x] 建关系/投递目标仅唯一标识:friend_*/link/send_message.recipient 接受 u/uid / 完整 FQID / 邮箱;`@昵称` 明确报错(第二波)
- [x] 显示实时化:删 ContactEntry.nickname 快照,改昵称后提醒/列表实时显示 `@昵称(u/uid)`;`/whoami` 显示 u/uid + email/nickname + 统计与注册引导
- [x] 全量测试 + clippy `-D warnings` 通过（run 31264854208 全绿,719 passed）

改动(第二波,已实现):
- `agents/mention.rs`(新):`render_refs`(整段输出渲染,白名单 `<ref id="…"/>` → `@昵称(u/uid)`,查不到/无昵称 → `@u/uid`,非本 namespace 或非白名单标签原样保留)+ `RefRenderer`(流式缓冲:尾部未闭合 `<ref…` 前缀暂存等补全,其余立即渲染流出,flush 原样)+ `resolve_mentions`(入站 `@昵称`/`@u/uid` 自由文本扫描:`u/` 前缀 → UserRegistry 精确解析不限关系;否则 → 关系内 Accepted 实时昵称比对,0 命中/多命中 → Failed 模板;邮箱防御 `a@b.com` 跳过;token 终止符空白/标点,`/` 保留支持 `u/uid`)
- `agents/orchestrator/inbound.rs`:`MentionPreParse` 拦截器挂 chain 第 6 位(SlashCommand 之后、DispatchTurn 之前;chain 6→7 元素,chain_order_is_pinned 同步更新);解析成功原位替换 `<ref id="…"/>` 进 agent 上下文,失败框架模板回复(零 token)
- `agents/runtime.rs`:`AgentRuntime.user_registry: Option<Arc<UserRegistry>>` + `with_user_registry`(不影响 new 调用点)
- `agents/agent.rs`:`collect_stream` 加 `user_registry` 参数;chunk 过 RefRenderer、返回 text 整段 render_refs(Done push / fallback send / history 同源,已渲染)
- `config/mod.rs`:`MessagingConfig`(`namespace` 默认 `myclaw`)+ `SmtpConfig`(host/port/username/password/from 全 Option,支持 `${ENV}` 展开);`AppConfig.messaging`
- `daemon.rs`:UserRegistry 改 `with_namespace(&base_dir, &config.messaging.namespace)`(默认 myclaw 存量零影响)+ agent_runtime `.with_user_registry(...)`

验收(第二波):
- [x] 入站 `@u/uid` 精确解析不限关系;`@昵称` 仅关系内实时昵称比对;未找到/重名多命中 → 框架模板拦截(零 token);邮箱 `a@b.com` 不误伤(单测 `resolve_*` 9 项)
- [x] 输出渲染:流式 chunk 跨 chunk 切割 `<ref id=…/>` 不残留标签(RefRenderer 缓冲);Done/fallback 整段渲染 `@昵称(u/uid)`;查不到 → `@u/uid`;非白名单标签/非本 namespace 原样保留(单测 `render_refs_*` / `ref_renderer_*` 6 项)
- [x] chain 顺序测试同步更新(6→7 元素,mention_preparse 在 slash_command 与 dispatch_turn 之间)
- [x] `[messaging] namespace` 配置项生效(默认 myclaw,存量 users.json/resolver 绑定零影响);`[messaging.smtp]` 仅解析(发送验证码流程后续,无 SMTP 声明即生效保持不变)
- [x] 全量测试 + clippy `-D warnings` 通过(run 31266339000 全绿,731 passed)

仍未做(如实区分):**SMTP 发送验证码流程**(配置项已解析,发送/验证后续波);**uid 变更**(P4 定不可变,不做——第三波起 uid 改系统分配 uuidv7,见 `identity-id-rfc.md`);**解绑/注销 User**(不做);**改 messaging.namespace 的存量迁移**(已规划 `migrate-namespace` 命令,见 `identity-id-rfc.md` §6.7;默认值下零影响)。

## 7. 待决(不阻塞 P0)

**当前无待决项**,历史决策留痕:
- ~~消息内容长度上限与分片~~ **已定**(§3.7):单条 32K chars 硬上限+拒绝;注入侧 ≤2K tokens 总量预算;渲染侧复用 `split_message_chunk`。
- ~~好友消息归档~~ **已定**:用户级 mailbox(未读)→ 注入即消费 → 并入触发注入的 session 聊天历史(天然归档,不独立存储)。

## 8. 相关文档

- `memory/claude_code_cross_session_messaging.md` — 竞品缺陷评估(设计来源)
- `memory/myclaw_agent_messaging_design.md` — 讨论收敛记录(本 RFC 的上游)
- `memory/send_message_tool_visibility.md` — 现状"非 channel 上下文不可用"逻辑,本 RFC 扩展之
- `docs/orchestrator-refactor-rfc.md` — SessionKey/SubAgentKey 值类型来源
