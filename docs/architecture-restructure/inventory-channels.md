# src/channels/ 模块盘点（子代理 C，2026-08-24）

总 15597 行 / 15 文件（`wc -l` 实测）。仓库只读分析，无 cargo。

## 1. 模块一句话职责 + 名实判断

**职责：四渠道（Telegram/QQ Bot/微信/本地 Client）消息适配层——实现 `Channel` trait，把各平台协议翻译为统一的 `ChannelInboundMessage`/`ChannelOutboundMessage`，并提供消息模型、安全策略、流式输出 trait。**

**名实判断：大体相符，两处偏差。**
- ✅ telegram/qqbot/wechat 纯适配器，与模块名相符。
- ⚠️ `client.rs`（2852 行）名为 WebSocketChannel，实为 **WebSocket 本地 HTTP+WebSocket JSON-RPC 服务器**：管理 sessions/memory/skills/config/daemon 等约 30 个 API 方法，反向深度依赖 `crate::agents`（SessionManager/SkillManager/UserResolver）——它不是"又一个聊天渠道"，是嵌在渠道层的 API 后端，是 channels→agents 依赖的主要源头。
- ⚠️ `message.rs` 名为消息模型，实际一半以上（~700 行）是**通用 markdown 感知文本分块算法**（`split_message_chunk` + 围栏/表格/标记配对扫描），与"消息模型"职责已分叉。

## 2. 全文件清单表（15 文件）

| 路径 | 行数 | 职责一句话 | pub 符号数 | Channel trait 关系 |
|---|---|---|---|---|
| channels/mod.rs | 32 | 模块根，re-export 门面 | 7 (pub use) | — |
| channels/client.rs | 2852 | WebSocket 本地 WS/HTTP JSON-RPC 渠道（feature=client） | 11 | `impl Channel for WebSocketChannel` :1075 |
| channels/message.rs | 1950 | Channel trait + 消息模型 + DedupState + 通用分块算法 | 48 | **trait `Channel` 定义处** :512 |
| channels/security.rs | 334 | AllowList/GroupAuthMode/ChannelSecurityPolicy 安全策略 | 13 | 被 trait `security_policy()` 返回 |
| channels/turn_stream.rs | 121 | TurnStream trait（流式输出三态投递） | 3 | trait 定义，被 Channel::create_stream 返回 |
| channels/wechat.rs | 2245 | 微信 ilink 非官方协议渠道（feature=wechat） | 2 | `impl Channel` :1577 |
| channels/telegram/mod.rs | 22 | telegram 子模块根 | 1 | — |
| channels/telegram/channel.rs | 3589 | Telegram Bot API 长轮询渠道 + TelegramTurnStream | 3 | `impl Channel` :1815 |
| channels/telegram/types.rs | 162 | Bot API JSON 反序列化类型 | 15 | 被渠道使用 |
| channels/qqbot/mod.rs | 15 | qqbot 子模块根 | 2 | — |
| channels/qqbot/channel.rs | 3195 | QQ Bot WebSocket 网关 + REST 渠道（feature=qqbot） | 18 | `impl Channel` :1750 |
| channels/qqbot/keyboard.rs | 94 | QQ 按钮 keyboard JSON 构建 | 8 | 被渠道使用 |
| channels/qqbot/markdown_sanitize.rs | 730 | QQ markdown 净化（$ 转义/CJK 粗体填充） | 3 | 被渠道使用 |
| channels/qqbot/token.rs | 220 | AppAccessToken 后台刷新管理 | 7 | 被渠道使用 |
| channels/qqbot/types.rs | 36 | WS 会话状态/GatewayPayload | 3 | 被渠道使用 |

测试行占比（`#[cfg(test)]` 起）：telegram/channel.rs ≈628 行(17.5%)、qqbot/channel.rs ≈443 行、client.rs ≈454 行、message.rs ≈332 行、wechat.rs ≈138 行。三个巨型 channel.rs 的业务代码实际约 2961/2752/2107 行。

## 3. 三渠道共性/差异剖面（telegram 3589 / qqbot 3195 / wechat 2245）

### 3.1 函数级对照表（行数均为起止实测）

| 职责 | telegram/channel.rs | qqbot/channel.rs | wechat.rs | 差异说明 |
|---|---|---|---|---|
| **连接/事件循环** | `poll_loop` :1298–1803 (**505 行**，HTTP getUpdates 长轮询+offset 持久化) | `ws_loop` :2429–2475 + `ws_connect` :2475–2587（WS 网关+Identify/Resume/心跳，~159 行） | `listen` 内联轮询循环 :1791–2033（**~242 行**，getUpdates 轮询+QR 登录+退避） | **协议真差异**：轮询 vs WS vs 轮询+扫码。但「循环骨架+退避+连续错误计数」结构雷同，可提取共享 reconnect/backoff 骨架 |
| **入站解析** | `parse_message_content` :838–849（薄，types.rs 强类型） | `parse_c2c_message` :761–789 + `parse_group_message` :789–820 + `extract_quote_content` :134–157 | `parse_inbound` :1179–1278（100 行，item_type 分派） | 各自协议结构，真差异；产出统一 ChannelInboundMessage ✓ |
| **鉴权/安全策略** | `try_authorize` :250–284 + `security_policy` :1828–1848 + mention/reply 检测 :303–410 | `handle_dispatch` 内嵌 `apply_auth` :554–761（**207 行**含嵌套 fn）+ `build_security_policy` :466–481 | `build_security_policy` :1363–1379 | policy 结构已共享（security.rs），但构造+群鉴权判定三份各写；telegram 多 mention/reply_to_bot，qqbot 多群白名单+per-group 配置 |
| **去重** | `DedupState` 共享，用例 :1397/:1513 | 共享，字段 :352 | 共享，用例 :1808 | ✅ 已上提共享层（message.rs :663） |
| **typing 指示** | `start_internal_typing` :1021–1108（87 行）+ `stop` :1108–1118 | `start_internal_typing` :1478–1519（41 行）+ `stop` :1519–1527 | `start_typing_keepalive` :1520–1568（48 行）+ `stop` :1568–1577 | **三份近似复制**：都是 HashMap<recipient, JoinHandle> + 定时循环 sendChatAction/send_typing。仅 API 端点不同，骨架可上提（~180 行） |
| **入站 debounce** | `debounce_send` :1125–1205（80 行） | `DeliverDebouncer` struct :2356–2424 + send_message 内联接入 :1796–1865 | `debounce_send` :1449–1520（71 行） | **三份三样实现**：tg/wechat 是自由函数+HashMap buffer，qqbot 是 struct 化。同一职责（合并窗口内快速连发）三处独立实现（~290 行），应统一上提 |
| **出站分块** | `chunk_for_telegram` :1803–1811（薄壳→`split_message_chunk`） | `send_message` 内联：`sanitize_qq_markdown`→`split_by_visual_lines` :157–231→`split_message_chunk` :1875–1890 | 无分块（`filter_markdown` :2039–2108 降级语法） | 核心 `split_message_chunk` 已共享 ✓；qqbot 多一层视觉行预分（QQ 客户端布局 bug 规避），属渠道真差异但可搬入共享层参数化 |
| **markdown 方言转换** | `escape_html` :2331、`normalize_markdown_tables` :434–468、`escape_digit_heading_lookalikes` :429 | markdown_sanitize.rs 全文件 730 行 + `strip_internal_tags` :306–348 | `filter_markdown` :2039–2108 | 各平台语法限制不同，**真差异**；但均属「输入 markdown → 渠道安全 markdown」管线，结构可共享（转换器 trait） |
| **媒体收发** | 下载 `download_file_bytes` :849–875、`convert_to_opus_ogg` :875–920、上传 multipart :2030–2098 | `upload_file_to_qq` :1351–1421、`ingest_voice/image/video_file_attachments` :889–1227（**~338 行三段近似**） | `download_cdn_media`+AES-ECB :891–957、`upload_to_cdn`+`upload_media` :989–1119 | 各自 CDN/上传协议，真差异；qqbot 三个 ingest_* 内部结构高度雷同可参数化 |
| **消息发送主路径** | `send_message` :1848–2098（**250 行**） | `send_message` :1790–2118（**328 行**） | `send_message` :1659–1790（**131 行**） | 三边都含：debounce 接入→文本转换分块→媒体分支→限流。骨架同构度高，仅传输细节不同 |
| **状态/ack 呈现** | ack 反应 :920–1006、`on_status` 表情 :2168–2226 | `on_status` :2141–2156（仅 typing）、`on_tool_event` :2156–2176 | `on_status` :1590–1615（typing）、`on_tool_event` :1615–1655、`send_tool_progress` :792–876 | 渠道能力真差异（tg 有 reaction API） |
| **流式输出** | `TelegramTurnStream` :2461–2958（**497 行**：preview 编辑/折叠/摘要） | 无（无流式） | 无 | tg 独有；client.rs 有简化版 ClientTurnStream :1242–1282 |
| **token/登录** | `fetch_bot_username` :284–298 | token.rs 独立 TokenManager（✓ 已拆出） | `login` :1379–1444（QR 扫码 60 次×1s） | 真差异 |
| **群历史** | 无 | group_history 三函数 :820–889 | 无 | qqbot 独有 |
| **限流** | 429 重试内嵌 `send_rich_message` :490–560 | `ReplyLimiter` :2245–2304 + `RateLimiter` :2305–2356 | `classify_backoff` :1310–1322 + RATE_LIMIT_PAUSE | 策略不同（被动回复限额 vs 全局限流 vs 退避），部分可共享 |

### 3.2 重复度量化（相同职责代码估算）

| 重复职责 | tg | qqbot | wechat | 合计可上提 |
|---|---|---|---|---|
| typing keepalive 骨架 | 87 | 41 | 48 | ~170 行 → 共享 ~60 行 |
| 入站 debounce | 80 | 69(struct) | 71 | ~220 行 → 共享 ~80 行 |
| security_policy 构造+群鉴权 | ~50 | ~60(含 apply_auth 部分) | ~16 | ~126 行 → 大部分可共享 |
| 连续错误退避/重连骨架 | 内嵌 poll_loop | ReconnectManager :2176–2245（✓ struct 化但仅 qqbot 用） | ErrorClass+classify_backoff :1278–1322 | ~90 行可共享 |
| 发送主路径同构骨架（debounce→转换→分块→媒体→限流管线） | ~250 | ~328 | ~131 | 骨架 ~200 行可提为模板方法 |

**合计：约 700–900 行同职责代码可上提到共享层**（现 client.rs 不适合做宿主——它是 API server；建议新建 `channels/shared/` 或扩 message.rs 旁的 `common.rs`）。

**必须留在子类的真差异**：协议连接（HTTP 轮询/WS/QR 登录）、平台 markdown 方言、CDN/AES 媒体编解码、被动回复限额、reaction 能力、Telegram 流式编辑。

## 4. client.rs（2852 行）剖面

**内部结构（按行号）：**
- `Subscriber`/`SessionOutputBus` :43–174 —— 多订阅者事件总线（按 bus_key 多路复用 session 输出到各 WS 连接）
- `ClientConnection` :175、`WebSocketChannel` :189（字段含 OnceLock 注入：session_manager/tool_specs/skill_manager/provider_registry/user_resolver :203–221）
- `start()` :294–1015 —— **721 行巨型函数**：TCP listener（pre-bound 热切换继承）→ HTTP 路由 → WS 升级 → 连接生命周期 → 会话绑定
- `impl Channel` :1075–1242（send_message 走 SessionOutputBus 直投，listen 返回 dummy rx）
- `ClientTurnStream` :1242–1282 —— TurnStream 薄实现（事件转发到 bus）
- `handle_api_request` :1370–2231 —— **861 行巨型 match**，方法清单（grep 实测）：sessions.list/create/switch/rename/history/delete/delete_message、memory.list/read/write/delete、skills.list/read/write/delete、tools.list、models.list/set、config.get/get_raw/save、file.read、daemon.restart、commands.list
- `reconstruct_history` :2233–2393 —— 存储历史 → WebSocket 消息形状
- tests :2398–2852（454 行，memory scope 隔离/bus_key 解析等）

**它管什么：** 不是渠道生命周期调度（那在 lib.rs/agents orchestrator）；它是"第四渠道"= 本地 WebSocket 的全部后端：WS 网关 + JSON-RPC API + memory/skill/config 管理入口。
**与各 channel.rs 的调用关系：** 无直接调用——通过 `Arc<dyn …>` 注入 agents 侧服务，与 tg/qqbot/wechat 平行挂在同一 trait 下，仅共享 message.rs 模型。

## 5. message.rs（1950 行）剖面

**消息模型职责**（:1–750）：`Channel` trait(:512, 15 方法含默认实现)、`ChannelInboundMessage`/`Outbound`/`Persisted`、`MessageSender/Receiver`、`ChannelFile/ChannelFileBody/LocalFileBody`、`InlineButton/CallbackAction`、`ProcessingStatus/ToolEvent`、`ChannelCapabilities`(含四渠道 const 构造器 :50–125)、`DedupState`(:663, 容量 5 万 LRU)。
**通用算法**（:751–1610 ≈860 行）：`split_message_chunk` + 围栏扫描 `scan_fence_regions`/表格续头/内联标记配对/UTF-16 预算——被 tg/qqbot 直接调用，是隐形的共享文本引擎。
**被谁用（agents/storage/tools 侧 grep 实测）**：`ChannelInboundMessage`→agents(ask_router/skill_extract/delegation_coordinator/session_context)+storage(completion_queue/inbound_spool)；`MessageSender/Receiver`→agents/scheduler、tools(send_message/friends/ask_user)；`PersistedChannelMessage`→storage(session/json_file/inbound_spool)+agents(session backend/types)；`ChannelFile`→tools/send_message、session_context；`CallbackAction`→orchestrator/inbound；`InlineButton`→tool_executor。**结论：message.rs 是 channels 对外的真正公共 API，分层上最稳定。**

## 6. 依赖方向明细（use 行 + inline 双源，全量 grep）

**channels → crate::agents（3 文件，均为 TurnEvent/管理器注入）：**
- `client.rs`：use `agents::{Skill, TurnEvent}` :33、`agents::workspace::skill_loader` :34；inline：`SessionManager` :203/252/1320、`SkillManager` :216/277/1325、`UserResolver` :221/288/1327/1037、`agents::commands::command_catalog` :2167（tests 另有 :2394）
- `telegram/channel.rs`：use `agents::TurnEvent` :13（TurnStream push 用）
- `turn_stream.rs`：use `agents::TurnEvent` :18（trait 签名需要）
- qqbot/channel.rs、wechat.rs：**无 agents 依赖** ✓（之前统计的 qqbot 5 处全部是 providers::media）

**channels → crate::providers（3 文件，均为 media 模态判定）：**
- `telegram/channel.rs`：`providers::media::{infer_mime_from_name :1683, modality_from_mime :1962, FileModality :1970/2051–2054}`
- `qqbot/channel.rs`：`providers::media::{modality_from_mime :2005, FileModality :2009–2012}`
- `client.rs`：`providers::capability_tool::ToolSpec` :205/257/1321、`providers::ProviderRegistry` :218/282/1326、`providers::capability_chat::{ContentPart :2236, ChatMessage :2234/2247}`、`providers::media` :715–723/2247–2288

**channels → crate::storage：0 文件 ✓**（分界干净）。
**反向依赖（agents→channels::message）**：agent.rs、orchestrator/{inbound,delegation}、session_context、ask_router、skill_extract、scheduling/scheduler、tool_executor、session/{backend,types}、storage/{session,json_file,inbound_spool,completion_queue}、tools/{send_message,friends,ask_user}——message.rs 符号被全仓消费。

## 7. 异味清单（文件:行号）

1. **超长函数**：
   - `client.rs:1370` `handle_api_request` ≈861 行单函数 match（~17 个 API 命名空间全内联）
   - `client.rs:294` `start()` ≈721 行（listener+HTTP+WS+连接管理一锅）
   - `telegram/channel.rs:1298` `poll_loop` ≈505 行
   - `qqbot/channel.rs:1790` `send_message` ≈328 行
   - `wechat.rs:1791` `listen` ≈242 行（含登录+轮询+退避）
   - `qqbot/channel.rs:554` `handle_dispatch` ≈207 行（内嵌 `apply_auth` 闭包式 fn）
2. **三渠道复制粘贴**：typing keepalive（tg:1021 / qq:1478 / wx:1520，~170 行）；入站 debounce 三份三样（tg:1125 / qq:2356 DeliverDebouncer / wx:1449，~220 行）；security_policy 构造三份；退避/重连骨架三份（qqbot 的 ReconnectManager :2176 已 struct 化但仅自用）。
3. **qqbot/channel.rs:889–1227** `ingest_voice/image/video_file_attachments` 三段结构雷同（~338 行），仅 file_type 与下载端点不同，可参数化合并。
4. **telegram/channel.rs:468–815** 发送函数 6 变体并存（send_text/send_rich_message/send_plain_message/send_rich_message_simple/send_text_html/edit_message_rich/edit_message_text_html），参数正交但未组合。
5. **名实不符**：`client.rs` 是 WebSocket API 后端却顶渠道之名并驻留渠道层（反向依赖 agents 5 类符号）；`message.rs` 半数是文本分块算法引擎，建议独立 `chunking.rs`。
6. **巨型文件本身**：telegram/channel.rs 3589（渠道+TurnStream 497 行+测试 628 行三职责一文件）；qqbot/channel.rs 3195（渠道+限流器+防抖器+重连器 4 个内嵌组件，对照 telegram 已拆 types/、qqbot 已拆 keyboard/token/markdown_sanitize，channel.rs 仍可再拆 session/ws 层）。
7. `wechat.rs` 单文件含协议加密（AES-ECB/PKCS7 :78–138）、API client、渠道三职责，未目录化（对照 telegram/qqbot 均已目录化）。

## 8. 拆分建议速记（供 RFC 汇总）

- message.rs → `model.rs`（trait+类型）+ `chunking.rs`（分块算法）；对外 re-export 不变。
- 新建 `channels/shared/`：typing keepalive、inbound debounce（统一为 qqbot 的 struct 风格）、退避/重连骨架、发送管线模板。
- client.rs → 独立 `websocket/` 或 `api/` 顶层模块（它不是渠道），`handle_api_request` 按命名空间拆 sessions/memory/skills/config。
- 三个 channel.rs 内的 TurnStream/限流器/防抖器拆出同目录子文件。
