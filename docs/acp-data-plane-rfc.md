# RFC: WebSocket 数据面切换为原生 ACP 协议

> 状态：草案（待评审）
> 日期：2026-08-28
> 前置调研：`workspace/websocket-protocol-comparison.md`（8 项目协议对比）
> 原则：bus 内核零改动；数据面纯原生 ACP（零自定义 method），管理面收敛为 `myclaw/*` 命名空间（同连接 JSON-RPC 扩展）；CI-only 验证（禁本地 cargo）

## 1. 背景与动机

现状（`src/websocket/server/`，3061 行：channel.rs 874 + bus.rs 199 + turn.rs 234 + api/ 五文件）：WS 通道用自定义 type 标签 JSON 协议，数据面（`message`/`cancel` → TurnEvent 流）与管理面（`api` 帧，25+ method）复用同一连接同一信封。对比竞品后的核心结论：

1. 协议自造、无生态：标准客户端（编辑器/ACP 生态）无法驱动 MyClaw；
2. 鉴权单薄：仅首条消息鉴权（channel.rs L381），header 鉴权弃置（L218 自注 Phase 1）；
3. 资源风险：200MiB base64 单帧上限在 954MB 主机上有 OOM 风险；
4. 管理 api 帧是"半 JSON-RPC"（有 id/method/params，缺 `jsonrpc:"2.0"` 字段）——但这恰是同连接演化的现成桥：补字段 + 加前缀即可并入标准 JSON-RPC 分发器，与 ACP method 共存。

ACP（Agent Client Protocol，Zed 主导）是当前 agent 客户端协议的事实标准：JSON-RPC 2.0、会话生命周期完整、流式 update 语义标准、Rust 官方 crate（`agent-client-protocol`）。grok-build 已验证 ACP over WS 可行（换行分隔 JSON-RPC，`xai-grok-shell/src/agent/server.rs`）；zeroclaw-gateway 亦已在 ACP 连接上叠加鉴权与多路由（`acp.rs`/`ws.rs`）。

**需求核实结论（2026-08-28）**：MyClaw 全部真实 WS 流量均落在 ACP 的 prompt 生命周期模型内——cron/friend/子代理通知实际投递走各渠道 `send_message`，不主动推送 web 端；跨渠道消息同步属于"同身份多端"问题，由 bus 广播解决。**因此数据面可以零扩展地切纯 ACP。**

## 2. 目标 / 非目标

**目标**
- G1：WS 数据面整体替换为原生 ACP（initialize / session/new / session/load / session/prompt / session/update / session/cancel / session/request）
- G2：管理面收敛为同连接 `myclaw/*` 自定义 method 命名空间（补 `jsonrpc:"2.0"`，与 ACP 共用 JSON-RPC 分发器；**不引入 HTTP 框架**，现有裸 TcpListener + tokio-tungstenite 不动）
- G3：握手期鉴权（Authorization header + `Sec-WebSocket-Protocol: bearer.<token>` 降级，参照 zeroclaw ws.rs L67/L97）
- G4：单帧上限 200MiB → 25MiB（对齐 openclaw worker-inference.ts L25）
- G5：bus 内核（Subscriber/SessionOutputBus/bus_key_candidates）与 turn-socket 解耦不变量**零改动**
- G6：热切换 listener 继承（pre_bound/SO_REUSEPORT）保持

**非目标**
- 不做 REST 化（保留为将来的逃生门：method 名即 REST 路径语义的 1:1 素材，拆分成本不随时间上升）
- 不做 codex 式 ack/游标可靠投递（单用户场景收益低，另立 issue）
- 不做多连接多路复用（ACP session 语义已够）
- 不引入 ACP unstable 的 plan/current_mode（MyClaw 无对应概念）
- 不动 TTS 合成路径

## 3. 目标架构

```
                        ┌────────────────────────────────────────┐
 127.0.0.1:18789        │  TcpListener（pre_bound 热切换继承）      │
   /ws      ──────────► │  路径路由：upgrade → JSON-RPC over WS    │
   其他路径（/myclaw）──► │  现状 type 帧协议（P2 前保留）           │
                        └──────────────┬─────────────────────────┘
                                       │
                        ┌──────────────▼─────────────────┐
                        │  JSON-RPC 分发器（共用，新增）      │
                        │  · 标准 method  → AcpAdapter     │
                        │  · myclaw/*     → 现 api/ 域 handler│
                        └──────────────┬─────────────────┘
                                       │ subscribe（现状接口不动）
                        ┌──────────────▼─────────────────┐
                        │  SessionOutputBus（身份级，多订阅者）│  ← 零改动
                        └────────────────────────────────┘
```

新增代码集中在 `src/websocket/acp/`（adapter.rs / codec.rs / mapping.rs）：codec 负责换行分隔 JSON-RPC 编解码与 method 分流；adapter 负责 ACP 会话语义 ↔ bus 桥接；`api/` 域 handler（sessions/memory/skills/system）**原样保留**，仅由分发器以 `myclaw/*` method 名调用。`channel.rs` 的巨型 match 与 `api/mod.rs` 的 WS 帧分发骨架退役。

现状注记：server 升级请求从不检查 HTTP 路径（前端连 `/myclaw`，channel.rs 无任何路径判断）——路径路由是本 RFC 新引入的维度，也因此 P1 的双轨切换对旧前端零感知。

## 4. 协议映射（数据面）

### 4.1 连接与会话

| 现状 | ACP | 说明 |
|---|---|---|
| 首条 `{"type":"auth","token":...}` | HTTP header/subprotocol 鉴权（G3）+ `initialize` 版本协商 | token 校验提前到握手；initialize 只管 protocolVersion 与 clientCapabilities |
| `{"type":"auth_ok"}` | `initialize` response + `initialized` notification | |
| 身份 rk `client:default:web-user:{u}` | adapter 内部维护：identity ↔ bus_key_candidates | 协议层不可见，多端广播语义保留在 bus |
| `sessions.switch`（api） | `myclaw/sessions.switch`（切活跃会话，daemon 态）+ `session/load`（本连接重挂订阅） | 管理调用与连接级订阅是两件事，分开表达 |
| 新会话 | `session/new` | |

### 4.2 输入

| 现状 | ACP | 说明 |
|---|---|---|
| `{"type":"message","content":...}` | `session/prompt {sessionId, prompt:[ContentBlock]}` | 文本→Text block；`files_base64` 图片→Image block（data/mimeType）；文本附件→Text block |
| `{"type":"cancel"}` | `session/cancel {sessionId}` | |
| `attachments` 内联文本 | 合并为 Text block（分块） | 现格式 `--- attached file ---` 保留为渲染约定 |

注：channel.rs L729-731 显示 WS 路径从不设置 `interruption_scope_id`/`silenced_override`/`run_mode`（全默认），无字段丢失。

### 4.3 输出（TurnEvent → SessionUpdate）

| TurnEvent | SessionUpdate | 说明 |
|---|---|---|
| `Chunk{delta}` | `agent_message_chunk {content: ContentChunk::Text}` | token 级 |
| `Thinking{delta}` | `agent_thought_chunk` | |
| `ToolCall{id,name,args}` | `tool_call {toolCallId, tool, rawInput}` | |
| `ToolResult{id,name,output,is_error}` | `tool_call_update {toolCallId, state}` | 成功→`Completed{output}`；失败→`Failed{error}` |
| `Done{text}` | prompt response `{stopReason: end_turn}` | 完整文本已由 chunk 流累积，response 不重复携带 |
| `Cancelled{partial}` | prompt response `{stopReason: cancelled}` | |
| `Error{message}` | prompt response `{stopReason: error}` | ACP stopReason 枚举含 error |
| `EmptyResponse` | `end_turn`（空流）+ 日志告警 | 无独立事件可映射 |

多轮工具循环 = 单次 `session/prompt` 的生命周期（update 流 → 单 response），与 MyClaw 一个 agent turn 的现有边界一致。

### 4.4 权限交互（新增能力）

现状 WS 无交互式审批。ACP `session/request`（agent→client 的 permission request，选项 once/always）为 MyClaw 后续工具审批（对照 `async_delegate_checkpoint_and_tool_allowlist_decisions` 的方向）预留标准通道。首版 adapter 实现最小响应（自动允许全部），接口占位。

## 5. 多设备与 bus 桥接规则

- bus 键仍是身份 rk；adapter 订阅粒度：**每连接 × 每 session**。
- 同身份两个 tab：各自 initialize + `session/load` 同一 sessionId → 两个 adapter 都 subscribe 到同一 bus → bus 现有 `subscribers: Vec` 广播（bus.rs L36/L116）自然生效，ACP 层不引入新状态。
- 断线：adapter drop → `bus.detach(conn_id)`（现状路径，channel.rs L847）；turn 继续跑完、事件进环形缓冲；重连后 `session/load` + 0→1 订阅触发 drain 重放——**重放语义与现状完全一致**。

## 6. 管理面：`myclaw/*` 自定义 method（同连接）

| 项 | 方案 |
|---|---|
| 协议形态 | 与 ACP 共用一条 WS 连接、同一 JSON-RPC 分发器；method 加 `myclaw/` 前缀（`myclaw/sessions.list`、`myclaw/memory.write`、`myclaw/daemon.restart`…），请求补 `jsonrpc:"2.0"` 字段 |
| 现有资产复用 | 现 api 帧已是"半 JSON-RPC"（有 id/method/params）——`api/mod.rs` 的路由 match 与四个域文件的 handler 全量保留，仅由新分发器按前缀调用、响应从 `api_response{result}` 信封改为 JSON-RPC result/error |
| 错误语义 | 现散落字符串错误 → JSON-RPC error object（code/message/data），code 段位沿用现 error_class 分级思想，实现 PR 给对照表 |
| 方向约束（兼容性关键） | 自定义 method **仅 client→agent 请求-响应**；server 永不主动发送任何自定义 notification。标准 ACP 客户端（Zed 等）只处理它认识的 method，不会发起 `myclaw/*`，也永远不会收到它无法理解的服务端推送——两类流量互不可见 |
| 大载荷 | `file.read` 增加 offset/limit 分页参数（帧内解决，无 HTTP 依赖） |
| 鉴权 | 与数据面同一连接，握手期鉴权（G3）天然覆盖，无第二套 token 路径 |

治理约定：`myclaw/*` method 表新增需在本 RFC 追加登记（方法名、参数、响应），防止命名空间腐化。

## 7. 前端改造（clients/web）

- `useWebSocket.ts` 重写为 JSON-RPC client：initialize 握手、session/load、prompt 发起、update 流累积渲染（chunk→文本增量；tool_call/update→工具时间线；thought 折叠区）。
- 管理调用改发 `myclaw/*` method（同连接，同一套 id 配对/超时逻辑；不引入 fetch/REST 层）。
- 兼容性：server 双轨期间（P1）旧前端零改动；前端切换集中在 P2 一个 PR，不留中间态。

## 8. 实施批次（每批一 commit 一轮 CI；禁本地 cargo）

| 批次 | 内容 | 破坏性 | 回退 |
|---|---|---|---|
| P1 | codec + 共用 JSON-RPC 分发器 + AcpAdapter（`initialize`/`session/*` 编解码、bus 桥接、TurnEvent 映射）+ `myclaw/*` method 表接线（现 api handler 复用）；路径路由——`/ws` 走新协议，其余路径（`/myclaw`）保持旧 type 帧（前端零感知） | 无（双轨） | revert；前端未动 |
| P2 | 前端切 `/ws`（JSON-RPC client）；header/subprotocol 鉴权强制；25MiB 上限；file.read 分页参数；删除旧协议路径与 channel.rs 巨型 match、api/mod.rs WS 帧分发骨架 | **有**（旧协议下线） | 保留一版热修回退点 |
| P3 | 收尾：tests.rs 旧协议用例迁移为 ACP/JSON-RPC 一致性测试（crate 类型往返 + myclaw/* 方向约束断言）；文档更新 | 无 | — |

依赖：`agent-client-protocol = "0.10"`（pin 小版本；grok-build 经验：直接用有毛边，必要时比照其 `xai-acp-lib/src/normalize.rs` 做输入归一化层）。

## 9. 风险与开放问题

| 风险 | 缓解 |
|---|---|
| ACP 0.10.x unstable 演进破坏兼容 | 只用稳定面 + session/request 最小实现；pin 版本；codec 层隔离 |
| 前端一次性重写量 | P1 双轨期间旧前端持续可用，前端改动集中在 P2 一个 PR |
| 数据/管理共用出站队列 | localhost 单用户场景写入毫秒级、25MiB 上限兜底；若实测挤占，后手是出站队列分优先级（不引入第二连接） |
| `myclaw/*` 命名空间腐化 | 治理约定（§6）：method 表 RFC 登记；P3 加"server 不发自定义 notification"的方向断言测试 |
| `EmptyResponse` 语义弱化 | 保留告警日志 + 前端空回复提示 |
| 开放：`client_id`/设备标识去留 | ACP 无此概念；bus 键已不含设备 id（channel.rs L400-411 注释确认），确认无残留后删除 |

## 10. 验收标准

1. 标准 ACP 客户端（用 agent-client-protocol crate 写的 conformance 脚本）可完成 initialize → session/new → prompt → 收全量 update → response 闭环，且全程不感知 `myclaw/*` 的存在；
2. 断连重连：prompt 进行中断开，turn 跑完，重连 session/load 后重放缓冲事件；
3. 同身份双连接同时在线，输出广播一致；
4. `myclaw/*` 管理 method 全 25+ 行为等价（对照现 api 帧响应快照），错误以 JSON-RPC error object 返回；
5. 25MiB 超限帧被拒（连接关闭 + 明确错误），daemon 内存无尖峰；
6. `myclaw update` 热切换后 WS listener 存活（pre_bound 路径回归）；
7. CI：lint/build/layering/migrate 全绿，测试计数 = 基线 + 新增（ACP 用例 + 方向约束断言）。
