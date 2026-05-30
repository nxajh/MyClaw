# RFC: Channel 模型重构

> 状态：草案
> 日期：2026-05-29
> 参考：OpenClaw `ChannelPlugin` / Hermes-agent `BasePlatformAdapter`

---

## 目录

1. [问题陈述](#1-问题陈述)
2. [目标与非目标](#2-目标与非目标)
3. [现状分析](#3-现状分析)
4. [业界参考](#4-业界参考)
5. [核心设计](#5-核心设计)
6. [新增类型](#6-新增类型)
7. [Channel trait 改动](#7-channel-trait-改动)
8. [SessionContext / Orchestrator 改动](#8-sessioncontext--orchestrator-改动)
9. [配置变更](#9-配置变更)
10. [各 Channel 实现影响](#10-各-channel-实现影响)
11. [实施阶段](#11-实施阶段)
12. [测试计划](#12-测试计划)
13. [跨模块耦合与迁移清单](#13-跨模块耦合与迁移清单)
14. [附录](#附录-a-sendmessage-删除时间线)

---

## 1. 问题陈述

### 1.1 WebUI 回复显示两次

Agent 回复通过两条互不感知的路径同时到达 WebUI：

1. **Streaming 路径**：`Agent::run` → `collect_stream` → `push_event(TurnEvent::Chunk/Done)` → WebSocket → WebUI
2. **`channel.send` 路径**：`SessionContext::process_turn` 在 `agent.run` 返回后调用
   `channel.send()`（commit `49e408a` 后此调用在 process_turn 内部，不在 orchestrator）
   → WebSocket → WebUI

Streaming 已交付完整文本后，`channel.send` 又发一次 `{"type":"message","content":"..."}`，
WebUI 检测到 `done:true` 的 assistant 消息已存在，创建一条全新的重复消息。

**根因**：`process_turn` 的 fallback send 不区分 streaming 和非 streaming channel，
对所有 channel 都执行 `send()`。

### 1.2 Channel trait 能力不足

当前 `Channel` trait 只有 6 个方法：

```rust
trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, msg: &SendMessage) -> Result<()>;
    async fn listen(&self) -> Result<mpsc::Receiver<ChannelMessage>>;
    async fn health_check(&self) -> bool;
    fn supports_streaming(&self) -> bool { false }
    async fn push_event(&self, _rt: &str, _ev: TurnEvent) {}
    fn cancel_signal(&self, _rt: &str) -> Option<CancellationToken> { None }
}
```

缺失的能力：

| 能力 | 影响 |
|------|------|
| `edit_message` | 无法编辑已发消息——流式推送（Telegram/QQBot）和按钮交互闭环的基础 |
| `delete_message` | 无法删除过期消息——重试/取消后的清理、draft→正式消息的替换 |
| `send_payload` | `SendMessage` 靠加字段扩展，已出现 `inline_buttons`，后续还要加 `poll`、`voice`、`card` 等，struct 会膨胀 |
| `send_draft` | Telegram `sendMessageDraft` 提供原生流式预览，edit-in-place 有"已编辑"标记 |
| `send_voice` / TTS 集成 | 语音回复需要平台特有的音频格式和发送 API |
| `format_message` | Telegram MarkdownV2 / QQBot markdown 子集各自处理，无统一清洗 |
| `capabilities()` 声明 | 只有 `supports_streaming()` 一个 bool，无法表达消息长度限制、计量单位、按钮/线程/媒体支持等 |
| 统一安全策略 | @过滤、白名单、DM 策略各 channel 各写一套 |

### 1.3 消息分片不准确

`split_message_chunk()` 用 `chars().count()` 计量，Telegram 按 UTF-16 code unit 计量。含 emoji 的 4096 字符消息在 Telegram 会被截断。

### 1.4 SendMessage 结构膨胀

```rust
pub struct SendMessage {
    pub content: String,
    pub recipient: String,
    pub subject: Option<String>,
    pub thread_ts: Option<String>,
    pub cancellation_token: Option<CancellationToken>,
    pub attachments: Vec<MediaAttachment>,
    pub image_urls: Option<Vec<String>>,
    pub inline_buttons: Option<Vec<InlineButton>>,  // 加的
    // pub poll: Option<PollData>,                   // 要加
    // pub voice: Option<VoiceData>,                 // 要加
    // pub card: Option<CardData>,                   // 要加
    // ... 无限膨胀
}
```

每新增一种消息形态就加一个 `Option` 字段，所有 channel 实现都要处理不相关的字段。

---

## 2. 目标与非目标

### 目标

1. 修复 WebUI 回复双显示 bug（Phase 0）
2. 引入 `ChannelCapabilities` 结构体，集中声明各 channel 的能力（Phase 1）
3. 按 platform 实际计量单位切分消息，修复 Telegram UTF-16 bug（Phase 1）
4. 引入 `MessagePayload` 枚举统一现有的文本 / 按钮 / 媒体三类发送接口，替代
   `SendMessage` 字段膨胀（Phase 2）—— Voice/Poll/Card 等到对应功能 PR 出现时
   作为新 variant 加入，不在本次范围
5. 新增 `edit_message` / `delete_message`，为非 WebUI channel 的流式推送和交互
   闭环打基础（Phase 3）
6. 预留群组安全策略抽象（Phase 4）
7. 预留按钮回调通用化（Phase 5）

### 非目标

- 不做 OpenClaw 式的 15+ adapter 插件组合（Rust trait 组合天然支持）
- 不做 Hermes 式的外挂 `GatewayStreamConsumer`（`push_event` + `TurnEvent` 已是等价设计）
- 不做完整插件系统（`before_send` / `after_send` 钩子作为 P3 预留）
- 不做跨 channel 的 presentation 抽象渲染层（`renderPresentation`），由各 channel 自行处理 payload
- **不在 trait 上暴露 `send_draft`**——Telegram `sendMessageDraft` 仍是 beta API，
  作为 TelegramChannel 内部实现细节即可（见 §7.2 / 附录 C.1）
- **不在 trait 上暴露 `format_message`**——平台格式转换是 channel 实现私事，
  调用方不应感知（见 §7.3）
- 不在本次新增 `Voice` / `Poll` / `Card` 等 payload variant——`MessagePayload` 是
  开放枚举，trait 默认 fallback 让加变体不破坏现有 channel；等真实消费者 PR
  出现时再加

---

## 3. 现状分析

### 3.1 当前 Channel trait 及实现

```
Channel (trait)
├── ClientChannel    — WebSocket server (TUI/WebUI)，支持 streaming
├── TelegramChannel  — Telegram Bot API polling
├── QQBotChannel     — QQ Bot HTTP + WebSocket
└── WeChatChannel    — 企业微信回调
```

**注意**：TelegramChannel 内部已有 `edit_message(chat_id, message_id, text)` 和 `delete_message(chat_id, message_id)` 私有方法（分别调用 `editMessageText` 和 `deleteMessage` API），但未暴露在 Channel trait 上，仅供 stall watchdog 使用。Phase 3 的工作是将已有方法提升为 trait 实现，而非从零开始。

QQBot 目前不支持 edit/delete API，send 内部自行调用 `split_message_chunk(&msg.content, QQ_MAX_MESSAGE_LENGTH)` 处理分片（`QQ_MAX_MESSAGE_LENGTH = 2000`）。

### 3.2 当前消息流（用户消息 → agent 回复 → 显示）

```
WebUI → WebSocket → ClientChannel.listen()
  → mpsc::Sender<ChannelMessage> → Orchestrator
    → session_ctx.process_turn()
      → Agent::run()
        → collect_stream() → push_event(Chunk/Done)  ─── 路径 1: streaming
      → if let Some(ch) = channel_for_send { ch.send(...) }  ── 路径 2: 重复!
      → return TurnResult { text }
```

**RFC v2 §三.B 重构（commit `49e408a`）后，fallback `channel.send` 已经移进
`SessionContext::process_turn`，不再在 orchestrator 调用方做**。这意味着 Phase 0
的修复点是 `session_context.rs:217` 附近的 `if let Some(ch) = channel_for_send`
块，而不是 orchestrator。

**`channel.send()` 当前调用点清单**：

| 文件:行 | 场景 | 走 streaming？ | Phase 0 影响 |
|------|------|---------------|-------------|
| `orchestrator.rs` L591 | 无 pending retry 的回调响应 | 否 | 无 |
| `orchestrator.rs` L597 | abort 确认 | 否 | 无 |
| `orchestrator.rs` L628 | incomplete turn 提示（retry/abort 按钮） | 否 | 无 |
| `orchestrator.rs` L678 | command 响应（如 /help、/status） | 否 | 无 |
| `orchestrator.rs` L726 | process_turn 返回 Err 的错误通知 | 始终发送 | 无（错误需保证送达） |
| `orchestrator.rs` L884 | startup recovery 恢复响应 | 否（无 push_event） | 无 |
| `orchestrator.rs` L1322 | startup recovery 发送 | 否 | 无 |
| **`session_context.rs` L223** | **process_turn 内部 fallback send** | **是（ClientChannel）** | **需加条件** |

**结论**：只有 `session_context.rs` 一处需要加 `!channel.supports_streaming()`
条件判断。其余场景都是一次性控制消息或非 streaming 路径，不受影响。

### 3.3 SendMessage 当前字段

| 字段 | 类型 | 用途 |
|------|------|------|
| `content` | `String` | 消息文本 |
| `recipient` | `String` | 发送目标 |
| `subject` | `Option<String>` | 邮件主题（预留） |
| `thread_ts` | `Option<String>` | 线程回复 |
| `cancellation_token` | `Option<CancellationToken>` | 取消令牌 |
| `attachments` | `Vec<MediaAttachment>` | 文件附件 |
| `image_urls` | `Option<Vec<String>>` | 图片 URL |
| `inline_buttons` | `Option<Vec<InlineButton>>` | 交互按钮 |

### 3.4 现有 UTF-16 计量 Bug

Telegram Bot API 的消息长度限制为 4096 UTF-16 code units，但 `chunk_for_telegram` 实际使用 `chars().count()`（Unicode codepoints）计量。含有 emoji、CJK 扩展字符等的消息，`chars().count()` 可能 ≤ 4096 但 `encode_utf16().count()` > 4096，导致 Telegram API 返回 `400 Bad Request`。

这是 Phase 1 `message_len_unit` 要顺带修复的 bug。

---

## 4. 业界参考

### 4.1 OpenClaw — 插件化组合

OpenClaw 把 channel 拆成 15+ adapter，通过 `ChannelPlugin` 组合：

```typescript
type ChannelPlugin = {
  id: ChannelId;
  capabilities: ChannelCapabilities;   // { threads, media, blockStreaming, ... }
  outbound: ChannelOutboundAdapter;    // sendText/sendMedia/sendPayload/sendPoll
  streaming: ChannelStreamingAdapter;  // blockStreaming 配置
  threading: ChannelThreadingAdapter;  // replyTo 策略
  security: ChannelSecurityAdapter;    // 白名单/DM 策略
  // + config, auth, groups, commands, lifecycle, ...
};
```

**防重复机制**：`deliveredVisibleText` 状态追踪。当 block streaming 已送达用户，跳过 final reply 的文本发送，只发 TTS 等附加内容。

**核心思路**：声明式能力 + 三种 reply kind（tool / block / final）序列化投递。

### 4.2 Hermes-agent — 继承 + 外挂 Consumer

```python
class BasePlatformAdapter(ABC):
    # 核心抽象
    async def send(chat_id, content, reply_to, metadata) -> SendResult
    async def edit_message(chat_id, message_id, content, finalize) -> SendResult
    async def delete_message(chat_id, message_id) -> bool

    # 流式能力（可选覆盖）
    def supports_draft_streaming(chat_type, metadata) -> bool  # 默认 False
    async def send_draft(chat_id, draft_id, content, metadata) -> SendResult

    # 工具
    def message_len_fn -> Callable[[str], int]  # Telegram 用 UTF-16
    def truncate_message(content, max_length, len_fn) -> List[str]
```

**防重复机制**：`GatewayStreamConsumer._final_content_delivered` 标志位 + `response["already_sent"]`。streaming 完成后 gateway 跳过正常 final send；文本被插件修改时 edit 已有消息而非重发。

**核心思路**：方法覆盖表达能力 + 外挂 `GatewayStreamConsumer` 统一流式。

### 4.3 对比

| 维度 | OpenClaw | Hermes-agent | MyClaw（目标） |
|------|----------|-------------|---------------|
| 能力声明 | `ChannelCapabilities` 结构体 | 方法覆盖 + 返回值 | `ChannelCapabilities` 结构体 |
| 发送接口 | `sendText/Media/Payload/Poll` | `send` + `edit_message` + `send_draft` | `send_payload` 统一 + `edit/delete` |
| 防重复 | `deliveredVisibleText` 状态追踪 | `already_sent` 标志位 | `supports_streaming()` 条件判断 |
| 消息计量 | 不处理 | `message_len_fn`（可插拔） | `message_len_unit` 枚举 |
| 安全策略 | `ChannelSecurityAdapter` 抽象 | 各 adapter 各写 | `ChannelSecurityAdapter` 抽象 |

---

## 5. 核心设计

### 5.1 设计原则

1. **渐进式**：每个 Phase 独立可用，不依赖后续 Phase
2. **降级友好**：不支持新能力的 channel 自动降级到文本发送，上层代码无需条件判断
3. **trait 最小**：Rust 的 trait 组合天然支持按需扩展，不需要 TypeScript 式的显式组合
4. **声明式能力**：`capabilities()` 返回不可变结构体，核心层按能力分支

### 5.2 防重复发送策略

**选择 MyClaw 路线**：`supports_streaming()` 条件判断。

理由：
- MyClaw 的 streaming 通道（ClientChannel）和非 streaming 通道（Telegram/QQBot）边界清晰
- 不需要 OpenClaw 式的三种 reply kind（tool/block/final），MyClaw 的 `TurnEvent` 已经涵盖了 chunk/tool_call/tool_result/done
- 不需要 Hermes 式的 `already_sent` 标志位追踪，因为 `process_turn` 的调用方（orchestrator）可以直接判断

```rust
// session_context.rs — process_turn 内部 fallback send
Ok(turn_result) => {
    if let Some(retry_msg) = &turn_result.pending_retry {
        *self.pending_retry.lock().await = Some(retry_msg.clone());
    }
    if let Some(ch) = channel_for_send {
        if !turn_result.text.trim().is_empty()
            && !ch.supports_streaming()  // streaming channel 已通过 TurnEvent 交付
        {
            let send_msg = SendMessage::new(turn_result.text.clone(), reply_target.clone());
            let _ = ch.send(&send_msg).await;
        }
    }
    Ok(turn_result)
}
```

未来当 Telegram/QQBot 也支持 edit-based streaming 时，它们的 `supports_streaming()` 会返回 `true`，同一个判断点自然生效。

### 5.3 流式中断时的 fallback

加上 `!supports_streaming()` 守卫后，streaming channel **完全依赖** `push_event`
交付最终文本。当 push_event 链路失败（WebSocket 断开后又重连、Chunk 丢失等）时
用户可能看不到回复。

缓解策略：

1. **首选：streaming channel 自己保证可靠性**。ClientChannel 的 WebSocket 关闭
   时会通过 `dedup_state` 在重连后重放最后 N 条事件（已存在机制）。
2. **次选**：`push_event` 在 channel 实现内部返回 `Result`，失败时退回 `send()`
   兜底——但这把"是否需要 fallback"的判断下沉到 channel，不在本 Phase 范围内。

Phase 0 接受现状："streaming 已成功完成"是 channel 的责任；orchestrator/
process_turn 信任 `supports_streaming()` 声明。后续如果出现可观测的丢失率，再
扩展 Channel trait 加 `send_event_ack` 之类的确认机制。

---

## 6. 新增类型

### 6.1 ChannelCapabilities

```rust
/// Channel 能力声明。不可变，在 channel 构造时确定。
///
/// 字段只覆盖**当前或下一个 Phase 真实会消费**的能力。`supports_voice`/
/// `supports_poll` 等等到对应 MessagePayload variant 引入时一起加（trait 默认
/// fallback 保证扩展不破坏现有 channel）。
#[derive(Debug, Clone, Copy)]
pub struct ChannelCapabilities {
    // ── 传输能力 ──
    /// 是否支持 streaming events（push_event + TurnEvent）
    pub supports_streaming: bool,
    /// 是否支持编辑已发消息（edit_message）
    pub supports_edit: bool,
    /// 是否支持删除消息（delete_message）
    pub supports_delete: bool,

    // ── 内容能力 ──
    /// 是否支持发送媒体文件（图片/文件）
    pub supports_media: bool,
    /// 是否支持交互按钮
    pub supports_buttons: bool,
    /// 是否支持线程回复（reply_to）
    pub supports_threads: bool,

    // ── 消息限制 ──
    /// 单条消息最大长度
    pub message_chunk_limit: usize,
    /// 长度计量单位
    pub message_len_unit: LenUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LenUnit {
    /// Unicode codepoint 计数（默认，大多数平台）
    Codepoints,
    /// UTF-16 code unit 计数（Telegram）
    Utf16Units,
    /// 字节计数
    Bytes,
}

impl ChannelCapabilities {
    /// 默认能力：不支持任何高级特性，4096 codepoint 限制。
    /// `const fn` 让 `static MINIMAL: ChannelCapabilities = minimal()`
    /// 可以在编译期生成，trait 默认实现直接返回 `&MINIMAL` 即可，
    /// 不需要 `thread_local!` 或 `LazyLock`。
    pub const fn minimal() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: false,
            supports_delete: false,
            supports_media: false,
            supports_poll: false,
            supports_buttons: false,
            supports_threads: false,
            message_chunk_limit: 4096,
            message_len_unit: LenUnit::Codepoints,
        }
    }

    /// WebUI/WebSocket channel 能力
    pub const fn client() -> Self {
        let mut c = Self::minimal();
        c.supports_streaming = true;
        c
    }

    /// Telegram Bot API 能力
    pub const fn telegram() -> Self {
        let mut c = Self::minimal();
        c.supports_edit = true;
        c.supports_delete = true;
        c.supports_media = true;
        c.supports_buttons = true;
        c.supports_threads = true;
        c.message_chunk_limit = 4096;
        c.message_len_unit = LenUnit::Utf16Units;
        c
    }

    /// QQ Bot 能力
    pub const fn qqbot() -> Self {
        let mut c = Self::minimal();
        c.supports_buttons = true;
        c.message_chunk_limit = 2000;
        c
    }
}

/// 编译期生成的默认值，trait 默认实现直接返回引用。
pub static MINIMAL_CAPABILITIES: ChannelCapabilities = ChannelCapabilities::minimal();
```

**设计选择**：用预定义的 `client()` / `telegram()` / `qqbot()` 构造函数，避免各
channel 实现手写 10+ 字段。需要微调时链式调用：
`ChannelCapabilities::telegram().with_message_chunk_limit(2048)`（Phase 2 再加
builder 方法，目前不做）。

注意删掉了 `supports_draft` 字段——见 §7.1 关于 `send_draft` 的决策说明。

### 6.2 MessagePayload

**Phase 2 范围**：只引入 `Text` / `Interactive` / `Media` 三个 variant —— 这些
是已经被 `SendMessage` 字段（`content` / `inline_buttons` / `attachments` +
`image_urls`）实际消费的能力。`Voice` / `Poll` / `Card` 等到对应的功能 PR 进来
时再加 variant；trait 默认 fallback 保证扩展不破坏现有 channel。

```rust
/// 平台无关的结构化消息 payload。
///
/// 各 channel 实现将 payload 转为平台原生格式。
/// 不支持的 variant 自动降级到 `to_fallback_text()`。
#[derive(Debug, Clone)]
pub enum MessagePayload {
    /// 纯文本消息
    Text {
        content: String,
    },

    /// 带交互按钮的消息
    Interactive {
        content: String,
        buttons: Vec<InlineButton>,
    },

    /// 媒体消息（图片/文件）
    Media {
        caption: Option<String>,
        source: MediaSource,
    },
}

/// 媒体来源
#[derive(Debug, Clone)]
pub enum MediaSource {
    /// URL 引用（可被 LLM provider 直接访问）
    Url(String),
    /// 本地文件路径
    FilePath(String),
    /// 内联二进制数据
    Inline { data: Vec<u8>, mime_type: Option<String> },
}

impl MessagePayload {
    /// 降级为纯文本（不支持该 payload 类型的 channel 使用）
    pub fn to_fallback_text(&self) -> String {
        match self {
            Self::Text { content } => content.clone(),
            Self::Interactive { content, buttons } => {
                let btn_text = buttons.iter()
                    .map(|b| format!("[{}]", b.label))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{}\n{}", content, btn_text)
            }
            Self::Media { caption, .. } => caption.clone().unwrap_or_default(),
        }
    }

    /// 提取文本内容（如果有的话）
    pub fn text_content(&self) -> Option<&str> {
        match self {
            Self::Text { content } => Some(content),
            Self::Interactive { content, .. } => Some(content),
            Self::Media { caption, .. } => caption.as_deref(),
        }
    }
}
```

**未来扩展模板**（不在本次 Phase 范围）：当真正需要语音时，加 variant
即可，trait 默认 fallback 保证旧 channel 直接降级文本——

```rust
// Phase 6+：当有 Voice TTS 集成 PR 时
Voice {
    data: Vec<u8>,
    format: AudioFormat,
    caption: Option<String>,
},
```

### 6.3 SendTarget — 取代 SendMessage 的路由信息部分

```rust
/// 发送目标。Phase 2 启用后**取代** `SendMessage` 中的路由字段
/// （`recipient` / `thread_ts` / `cancellation_token`）。
///
/// 新接口形态：`channel.send_payload(target: &SendTarget, payload: &MessagePayload)`
/// 不再有 `SendMessage` 把路由和内容混在一起，避免 §1.4 的字段膨胀。
#[derive(Debug, Clone)]
pub struct SendTarget {
    /// 目标路由 key / chat_id
    pub recipient: String,
    /// 线程回复目标（平台消息 ID）
    pub reply_to: Option<String>,
    /// 线程 ID（Telegram topic / Slack thread）
    pub thread_id: Option<String>,
    /// 取消令牌（移自 SendMessage）
    pub cancellation_token: Option<CancellationToken>,
}

impl SendTarget {
    pub fn new(recipient: impl Into<String>) -> Self {
        Self {
            recipient: recipient.into(),
            reply_to: None,
            thread_id: None,
            cancellation_token: None,
        }
    }
}
```

### 6.4 SendResult

```rust
/// send_payload 的返回结果。`Some(id)` 表示平台返回了消息 ID
/// （可后续 `edit_message` / `delete_message` 引用），`None` 表示送达但无 ID。
pub type SendResult = Option<MessageId>;

/// 平台消息 ID 的轻量 newtype（防止与其他 String 混淆）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageId(pub String);
```

不用之前提案的 `enum SendResult { Delivered, WithId{...} }` 二元枚举——
`Option<MessageId>` 语义完全等价，少一层 match，调用方习惯。

---

## 7. Channel trait 改动

### 7.1 新 trait 定义

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    // ── 元信息 ──
    fn name(&self) -> &str;

    /// Channel 能力声明。不可变。
    /// 默认指向 `MINIMAL_CAPABILITIES`（编译期 const）。
    fn capabilities(&self) -> &ChannelCapabilities {
        &MINIMAL_CAPABILITIES
    }

    // ── 入站 ──
    /// 监听入站消息，返回 mpsc Receiver。
    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>;

    // ── 出站：结构化发送 ──
    /// 发送结构化 payload。默认降级到文本用 send() 发。
    async fn send_payload(
        &self,
        target: &SendTarget,
        payload: &MessagePayload,
    ) -> anyhow::Result<SendResult> {
        let text = payload.to_fallback_text();
        let mut msg = SendMessage::new(text, &target.recipient);
        msg.thread_ts = target.thread_id.clone();
        msg.cancellation_token = target.cancellation_token.clone();
        self.send(&msg).await?;
        Ok(None) // 没有 message_id 可返回
    }

    // ── 出站：兼容旧接口（Phase 4 后强制迁移到 send_payload，见附录 A）──
    async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>;

    // ── 出站：编辑/删除 ──
    /// 编辑已发消息。不支持时返回 Err。
    async fn edit_message(
        &self,
        target: &SendTarget,
        message_id: &MessageId,
        payload: &MessagePayload,
    ) -> anyhow::Result<()> {
        let _ = (target, message_id, payload);
        Err(anyhow::anyhow!("edit_message not supported by {}", self.name()))
    }

    /// 删除消息。不支持时返回 Err。
    async fn delete_message(
        &self,
        target: &SendTarget,
        message_id: &MessageId,
    ) -> anyhow::Result<()> {
        let _ = (target, message_id);
        Err(anyhow::anyhow!("delete_message not supported by {}", self.name()))
    }

    // ── 流式事件 ──
    /// 是否支持 streaming turn events。默认从 capabilities() 读取。
    fn supports_streaming(&self) -> bool {
        self.capabilities().supports_streaming
    }

    /// 推送 per-turn 流式事件（chunk/thinking/tool_call/done）。
    async fn push_event(&self, _reply_target: &str, _event: TurnEvent) {}

    /// 获取当前 turn 的取消令牌。
    fn cancel_signal(&self, _reply_target: &str) -> Option<CancellationToken> {
        None
    }

    /// 按平台计量单位计算消息长度。
    fn message_len(&self, text: &str) -> usize {
        match self.capabilities().message_len_unit {
            LenUnit::Codepoints => text.chars().count(),
            LenUnit::Utf16Units => text.encode_utf16().count(),
            LenUnit::Bytes => text.len(),
        }
    }

    // ── 健康检查 ──
    async fn health_check(&self) -> bool;

    // ── 状态通知 ──
    async fn on_status(&self, _recipient: &str, _status: ProcessingStatus) {}
}
```

### 7.2 关于 `send_draft` 的取舍

之前草案在 trait 上加了 `send_draft(target, draft_id, content)` 方法以利用
Telegram 的 `sendMessageDraft` API 做原生流式预览。**本 RFC 不在 trait 上暴露
这个能力**：

1. **Telegram `sendMessageDraft` 仍是 beta API**（见附录 C.1），签名/行为可能
   变动。trait 是 channel 间公约，绑定不稳定外部 API 不合适。
2. **edit-based streaming 已能覆盖同样的 UX**：发送占位消息 → 持续 `edit_message`
   更新——所有支持 `edit_message` 的平台都能用。
3. 如果 TelegramChannel 实现内部想用 `sendMessageDraft` 做优化，可以作为
   `send_payload` / `edit_message` 实现的私有细节，不需要让上层感知。

后续如果 sendMessageDraft 稳定且有其它平台也提供等价能力，再加 trait 方法不迟。

### 7.3 关于 `format_message` 的取舍

之前草案在 trait 上暴露 `format_message(&str) -> String` 做 markdown → 平台格式
转换。**本 RFC 不暴露**：

- 平台格式转换是 `send` / `send_payload` 实现内部的事，调用方不应该感知"这条
  消息现在是 Telegram MarkdownV2 还是 QQBot markdown 子集"。
- 各 channel 实现里有私有的 `markdown_to_telegram_html()` 之类的辅助函数，已经
  够用；提到 trait 上反而邀请上层在不该感知格式的地方调用。

### 7.4 向后兼容

| 现有方法 | 变化 |
|----------|------|
| `name()` | 不变 |
| `send()` | Phase 1-3 不变；Phase 4 后转 `#[deprecated]`，附录 A 定下线节奏 |
| `listen()` | 不变 |
| `health_check()` | 不变 |
| `supports_streaming()` | 不变，默认实现改为从 `capabilities()` 读取 |
| `push_event()` | 不变，默认 no-op |
| `cancel_signal()` | 不变，默认 None |
| `on_status()` | 不变，默认 no-op |
| **新增** `capabilities()` | 默认返回 `&MINIMAL_CAPABILITIES`（编译期 const） |
| **新增** `send_payload()` | 默认降级到 `send()` |
| **新增** `edit_message()` | 默认返回 Err |
| **新增** `delete_message()` | 默认返回 Err |
| **新增** `message_len()` | 默认按 `capabilities().message_len_unit` 计算 |

所有新增方法都有默认实现，现有 channel 实现无需改动即可编译。删除了之前草案
的 `send_draft()` / `format_message()`——见 §7.2 / §7.3 的取舍说明。

---

## 8. SessionContext / Orchestrator 改动

### 8.1 防重复发送（Phase 0）

修复点在 **`session_context.rs::process_turn` 的 fallback send 块**，**不是
orchestrator**——commit `49e408a` 已把 `channel.send` 从 orchestrator 移进
process_turn。

```rust
// src/agents/session_context.rs — process_turn 内的 Ok 分支
match result {
    Ok(turn_result) => {
        if let Some(retry_msg) = &turn_result.pending_retry {
            *self.pending_retry.lock().await = Some(retry_msg.clone());
        }
        if let Some(ch) = channel_for_send {
            if !turn_result.text.trim().is_empty()
                && !ch.supports_streaming()
                // ↑ streaming channel 已通过 TurnEvent::Chunk + Done 交付
            {
                let send_msg = SendMessage::new(
                    turn_result.text.clone(),
                    reply_target.clone(),
                );
                let _ = ch.send(&send_msg).await;
            }
        }
        Ok(turn_result)
    }
    Err(e) => Err(e),
}
```

**Orchestrator 的错误通知路径不变**——`Err` 分支调用方在
`orchestrator.rs:724` 仍然无条件 `channel.send(MSG_TURN_FAILED)`，因为流式不
保证错误已推送。

### 8.2 未来：非 WebUI channel 的 edit-based streaming

当 Telegram/QQBot 实现 `edit_message` 后，可以扩展 `process_turn` 中的流式推送：

```rust
// 未来扩展：collect_stream 中对 supports_edit 的 channel
// 做渐进式 edit streaming（发一条 → 持续 edit 更新）
if channel.capabilities().supports_edit {
    // edit-based streaming（类似 Hermes 的 GatewayStreamConsumer）
}
```

这部分不在本次 RFC 范围内，但 trait 设计预留了扩展空间。

---

## 9. 配置变更

### 9.1 Channel 配置新增

```toml
# config.toml — 各 channel 可配置的消息长度覆盖
[channels.telegram]
max_message_length = 4096  # UTF-16 code units

[channels.qqbot]
max_message_length = 2000  # codepoints
```

`ChannelCapabilities` 从配置 + channel 实现的硬编码合并得到。

---

## 10. 各 Channel 实现影响

### ClientChannel（WebUI/TUI）

| 改动 | 内容 |
|------|------|
| `capabilities()` | 覆盖返回 `ChannelCapabilities::client()`（`supports_streaming: true`） |
| 防重复 | orchestrator 不再 `channel.send()`，已通过 TurnEvent 交付 |
| `send_payload` | 保持现有 `send()` 的 JSON 格式 `{type:"message", session, content}`，扩展 payload 类型映射 |

**注意**：`ClientChannel::send()` 通过 `session_owners` 映射找到 WebSocket 连接，`push_event` 通过 `stream_contexts` 映射找到 event_tx。两者用不同的 key（session_key vs reply_target），但这恰好是同一个值。

### TelegramChannel

| 改动 | 内容 |
|------|------|
| `capabilities()` | 覆盖返回 `ChannelCapabilities::telegram()` |
| `edit_message`（trait） | **提升已有内部方法**：现有 `edit_message(chat_id, message_id, text)` 已调用 `editMessageText` API，只需包装为 trait 签名（接受 `SendTarget + &MessagePayload`） |
| `delete_message`（trait） | **提升已有内部方法**：现有 `delete_message(chat_id, message_id)` 已调用 `deleteMessage` API |
| `message_len` | 新增，`encode_utf16().count()`，**修复现有 `chars().count()` bug** |
| `chunk_for_telegram` | 改用 `self.message_len()` 替代硬编码 `chars().count()` |
| 内部 `markdown_to_telegram_html()` | 保留为私有辅助，在 `send_payload` / `edit_message` 实现里调用 |

**现有安全策略**：
- `allowed_users: Arc<RwLock<Vec<String>>>`（支持热重载）+ `is_user_allowed(username, user_id)`
- `mention_only: bool`（群组中只响应 @提及）
- `contains_bot_mention()` / `is_reply_to_bot()`
- `DedupState` 防 update_id 重复
- `debounce_ms` 合并快速连续消息

这些逻辑散落在 `TelegramChannel` 的 polling 循环中，Phase 4 考虑提取到独立的安全策略 struct。

### QQBotChannel

| 改动 | 内容 |
|------|------|
| `capabilities()` | 覆盖返回 `ChannelCapabilities::qqbot()`（`supports_buttons: true, message_chunk_limit: 2000`） |
| `edit_message` | 调研 QQBot 消息编辑 API 是否可用，不可用则保持默认 Err |
| 内部 markdown 子集清洗 | 现有/新增私有辅助函数，在 `send_payload` 实现里调用，**不暴露到 trait** |

**现有安全策略**：
- `allow_from: Option<Vec<String>>`（C2C 白名单）+ `is_user_allowed(openid)`
- `group_allow_from: Option<Vec<String>>`（群组白名单）+ `is_group_allowed(group_openid)`
- 按钮点击通过 `INTERACTION_CREATE` 事件接收，`button_data` 转为文本消息

QQBot 的 `send()` 内部自行调用 `split_message_chunk(&msg.content, QQ_MAX_MESSAGE_LENGTH)` 处理分片。Phase 1 改造 `split_message_chunk` 时需同步更新此处。

### WeChatChannel

| 改动 | 内容 |
|------|------|
| `capabilities()` | 基本保持 `minimal()`（默认） |
| 内部 markdown → 纯文本降级 | 私有辅助函数，在 `send_payload` 内调用 |

**现有安全策略**：`allowed_users` 白名单检查。

---

## 11. 实施阶段

### Phase 0：修复 WebUI 双显示 bug（P0）

**范围**：仅改 `src/agents/session_context.rs` 的 process_turn 内 fallback send 块

```rust
// session_context.rs:223 附近，给 fallback send 加 supports_streaming 守卫
if let Some(ch) = channel_for_send {
    if !turn_result.text.trim().is_empty()
        && !ch.supports_streaming()   // ← 新增条件
    {
        let send_msg = SendMessage::new(turn_result.text.clone(), reply_target.clone());
        let _ = ch.send(&send_msg).await;
    }
}
```

**风险**：极低。`supports_streaming()` 已有，ClientChannel 返回 `true`，其他 channel 返回 `false`。

### Phase 1：引入 ChannelCapabilities + message_len_unit（P1）

**范围**：
- 新增 `ChannelCapabilities` / `LenUnit` 类型（含 `const fn client()` / `telegram()` /
  `qqbot()` 预定义构造）
- `pub static MINIMAL_CAPABILITIES: ChannelCapabilities` 编译期常量
- Channel trait 新增 `capabilities()` / `message_len()` 默认方法（**不**包含
  `format_message`——见 §7.3）
- ClientChannel / TelegramChannel / QQBotChannel 覆盖 `capabilities()` 返回各自常量
- 改造 `split_message_chunk()` 接受 `LenUnit` 参数（**同时修复 Telegram UTF-16
  计量 bug**）
- TelegramChannel 的 `chunk_for_telegram` 改用 `self.message_len()` 替代
  `chars().count()`
- QQBotChannel 的 `send()` 内部 `split_message_chunk` 调用改用
  `self.capabilities().message_chunk_limit` + `self.capabilities().message_len_unit`

**风险**：低。所有新方法有默认实现。`split_message_chunk` 签名变化会影响所有
调用方（Telegram + QQBot + 测试），但改动是机械的；可保留旧名作为
`split_message_chunk_chars` 兼容包装减少改动面。

### Phase 2：引入 MessagePayload + send_payload（P1，缩范围版）

**范围**：
- 新增类型：`MessagePayload`（只含 `Text` / `Interactive` / `Media` 三个 variant）、
  `MediaSource`、`SendTarget`、`MessageId`、`SendResult = Option<MessageId>`
- Channel trait 新增 `send_payload()` 默认方法（降级到 `send`）
- **重构 inline_buttons**：所有走 `SendMessage.inline_buttons` 的调用点改为
  `send_payload(target, MessagePayload::Interactive{...})`
- **重构 attachments / image_urls**：所有走 `SendMessage.attachments` 或
  `image_urls` 的调用点改为 `send_payload(target, MessagePayload::Media{...})`
- 各 channel 实现覆盖 `send_payload()`：Telegram 完整支持三个 variant；QQBot
  完整支持 `Text` + `Interactive`，`Media` 降级；ClientChannel 全部支持
- Phase 2 结束时，所有 callsite 不再读 `SendMessage.inline_buttons` /
  `attachments` / `image_urls`，为附录 A 的 Phase 4 删除 `SendMessage` 做铺垫

**不在 Phase 2 范围内**（推迟到对应功能 PR 出现时再加）：
- `Voice` variant（需要 TTS 集成 PR）
- `Poll` variant（需要投票 UI PR）
- `Card` variant（需要富文本卡片 PR）

**风险**：中。需要逐一改造 channel 实现，但默认降级保证不影响现有功能。

### Phase 3：edit_message / delete_message（P1）

**范围**：
- Channel trait 新增 `edit_message()` / `delete_message()` 默认方法（默认返回 Err）
- TelegramChannel：**将已有私有 `edit_message(chat_id, msg_id, text)` 提升为
  trait 实现**（接受 `SendTarget + MessageId + MessagePayload`，内部复用现有
  `editMessageText` API 调用）
- TelegramChannel：**将已有私有 `delete_message(chat_id, msg_id)` 提升为 trait
  实现**
- QQBotChannel：调研并实现 `edit_message`（如 API 支持），否则保持默认 Err

**不在 Phase 3 范围内**：`send_draft`——见 §7.2 决策。如果将来需要 Telegram
`sendMessageDraft` 的优势，作为 TelegramChannel 内部细节实现，不暴露到 trait。

**风险**：低。默认实现返回 Err，不影响不支持的 channel。Telegram 的 edit/delete
已有成熟实现，只是签名包装。

### Phase 4：群组安全策略抽象（P2）

**范围**：
- 新增 `ChannelSecurityPolicy` trait（白名单/@过滤/DM 策略）
- 各 channel 的安全逻辑提取到独立 struct
- Orchestrator 在 dispatch 前统一检查安全策略

**风险**：中。需要重构各 channel 的鉴权代码。

### Phase 5：按钮回调通用化（P2）

**范围**：
- 新增 `InteractionCallback` 类型（替代 `__retry:` / `__abort:` 硬编码前缀）
- Channel trait 新增 `parse_callback()` 方法
- 重构 orchestrator 的 callback 分发逻辑

**现有 callback 机制**：
- `retry_abort_prompt()` 构建 `InlineButton { callback_data: "__retry:{sk_prefix}" }` / `"__abort:{sk_prefix}"`
- Telegram 通过 `callback_query.data` 返回 callback_data
- QQBot 通过 `INTERACTION_CREATE.button_data` 返回
- 两者都作为普通文本消息（`content = "__retry:..."`）进入 orchestrator 的 `handle_channel_event`，在 L560 解析前缀
- Telegram callback 有 64 字节限制（所以用 32 字符 session key prefix）
- QQBot callback 通过 `Keyboard` 结构体传递，button_data 也有长度限制

**改进方向**：引入结构化 `CallbackAction` 枚举（Retry / Abort / Custom），由 channel trait 的 `format_callback()` / `parse_callback()` 负责序列化/反序列化，orchestrator 不再解析字符串前缀。

**风险**：低。

---

## 12. 测试计划

### Phase 0

- [ ] WebUI 发消息后，agent 回复只出现一次（不出现重复）
- [ ] Telegram/QQBot 回复正常发送（不受 `supports_streaming()` 判断影响）
- [ ] 错误消息（`MSG_TURN_FAILED`）仍然正常发送到 WebUI（走 orchestrator 的 Err 分支）
- [ ] 流式中断场景：ClientChannel 断连后重连，仍能看到完整回复（验证现有
  `dedup_state` 重放机制不被守卫破坏）

### Phase 1

- [ ] 各 channel 的 `capabilities()` 返回正确值（且为 `&'static`，不分配）
- [ ] `message_len()` 对 UTF-16 计量准确（emoji 算 2 单位）
- [ ] `split_message_chunk` 按 UTF-16 切分 Telegram 消息不超限
- [ ] `chars().count() ≤ 4096 但 encode_utf16().count() > 4096` 的 emoji 消息不再触发 Telegram `400 Bad Request`（回归测试）

### Phase 2

- [ ] `send_payload(Text)` 等价于现有 `send(SendMessage::new(text, target))`
- [ ] `send_payload(Interactive)` 在 Telegram 渲染 inline_keyboard，在 QQBot 渲染 Keyboard
- [ ] `send_payload(Media{Url})` 在 Telegram 发送图片，在 QQBot 降级为 caption 文本
- [ ] `send_payload(Media{Inline})` 在 Telegram 发送二进制媒体
- [ ] 不支持 payload 类型的 channel 降级到 `to_fallback_text()`，输出可读
- [ ] `send_payload` 返回 `Some(MessageId)` 当平台返回 ID，否则 `None`
- [ ] 所有 callsite 不再读 `SendMessage.{inline_buttons, attachments, image_urls}`

### Phase 3

- [ ] Telegram `edit_message` 更新已发消息内容
- [ ] Telegram `delete_message` 删除指定消息
- [ ] QQBot `edit_message` 正常工作（或返回 Err，由实际 API 决定）
- [ ] WebUI 不受影响（不调用 edit/delete）

### Phase 4

- [ ] Telegram 群组只响应 @提及（配置启用时）
- [ ] 白名单过滤正确生效
- [ ] DM 策略按配置执行

### Phase 5

- [ ] 按钮点击产生结构化 callback（非 `__retry:` 前缀）
- [ ] orchestrator 正确路由 callback 到目标处理逻辑
- [ ] 现有 retry/abort 按钮平滑迁移

---

## 13. 跨模块耦合与迁移清单

Channel 改动不只影响 `channels::` 模块本身。下表列出所有跨模块依赖点，
按 Phase 时间线说明每处的迁移路径。

### 13.1 所有 `channel.send(SendMessage)` callsite 清单

Phase 0 不影响这些；Phase 2 大部分迁移到 `send_payload`；Phase 4 强制全部迁移。

| 文件:行 | 场景 | Phase 2 改造 | Phase 4 删 SendMessage |
|---|---|---|---|
| `orchestrator.rs:592` | callback 命中无 retry 时的回应 | text → `MessagePayload::Text` | 必改 |
| `orchestrator.rs:598` | `MSG_ABORT_ACK` 确认 | text → `MessagePayload::Text` | 必改 |
| `orchestrator.rs:629` | incomplete turn 提示（含 retry/abort 按钮） | **典型 `MessagePayload::Interactive` 目标** | 必改 |
| `orchestrator.rs:679` | command 响应（`/help`、`/status` 等） | text → `MessagePayload::Text` | 必改 |
| `orchestrator.rs:726` | `MSG_TURN_FAILED` 错误通知 | text → `MessagePayload::Text` | 必改 |
| `orchestrator.rs:878` | startup recovery 恢复后的响应 | text → `MessagePayload::Text` | 必改 |
| `orchestrator.rs:1318` | `send_to_target_internal`（cron/heartbeat 投递） | text → `MessagePayload::Text` | 必改 |
| `scheduler.rs:1081` | Webhook `send_to_target` 投递 | text → `MessagePayload::Text` | 必改 |
| `session_context.rs:223` | `process_turn` 内 fallback send（**Phase 0 加 guard 的位置**） | text → `MessagePayload::Text` | 必改 |
| `tools/ask_user.rs:184` | AskUserTool 发送问题 | text → `MessagePayload::Text` | 必改 |

共 **10 个 callsite**。其中 1 个（`orchestrator.rs:629` retry/abort 按钮构造，
即 `retry_abort_prompt()` 辅助函数）是 `MessagePayload::Interactive` 的天然
迁移目标——Phase 2 应该把它一起改了。

### 13.2 AskUserTool：两个独立的清理点

`tools/ask_user.rs` 有**两个**和本 RFC 相关的问题，应在 Phase 2 一起做完：

1. **`channel.send(SendMessage)` → `channel.send_payload(SendTarget, MessagePayload::Text)`**
   （和其他 9 个 callsite 同节奏）
2. **不再自持 `channels: ChannelMap`，改读 `session.channel`**
   （和其他工具的官方约定对齐，见 §13.5）

完整迁移代码在 §13.5 给出。这样 Phase 4 删除 `SendMessage` 时工具层不再有任何
连带改动；同时清理了 AskUserTool 与其他工具的接口不一致点。

### 13.3 TurnEvent vs MessagePayload 的边界

两套平行的出站路径，**用途互补不冲突**：

| 路径 | 触发场景 | 内容形态 | 调用方 |
|------|----------|----------|--------|
| `push_event(reply_target, TurnEvent)` | streaming channel 的流式增量推送 | `Chunk` / `Thinking` / `ToolCall` / `ToolResult` / `Done` | `Agent::run` 内部的 `collect_stream` |
| `send_payload(SendTarget, MessagePayload)` | 最终消息 / 控制消息 / 工具发送 | `Text` / `Interactive` / `Media` | process_turn fallback、orchestrator 各路径、AskUserTool 等 |

**边界规则**（Phase 0 起执行）：

1. **streaming channel**（`supports_streaming = true`）：
   - 通过 `push_event(TurnEvent::Chunk)` 增量推送 token，`push_event(TurnEvent::Done)` 标记结束
   - **不再**通过 `send_payload` 发送最终文本（Phase 0 的守卫负责跳过）
   - 控制消息（错误、命令响应）仍走 `send_payload`

2. **非 streaming channel**（`supports_streaming = false`）：
   - `push_event` 是 no-op（trait 默认实现）
   - 所有出站消息走 `send_payload`

不在本 RFC 范围内的扩展：**edit-based streaming**（非 streaming channel 用
`send_payload` 发占位 + 持续 `edit_message` 更新）是 Phase 3 之后的可能演进，
届时需要在 Channel trait 上再加一个 `supports_edit_streaming` capability，
和 `push_event` 形成分支。当前 RFC 不预留 trait 方法。

### 13.4 ChannelCapabilities 与 TOML 配置的合并

§9 引入 `[channels.telegram] max_message_length = 4096` 这类配置。§6.1 把
`ChannelCapabilities::telegram()` 写成 `const fn` 硬编码默认值。两者通过
**channel 构造时**合并，**不**通过运行时查询合并：

```rust
// TelegramChannel::new(cfg: TelegramConfig) — Phase 1 实施时
let capabilities = {
    let mut base = ChannelCapabilities::telegram();  // const default
    if let Some(limit) = cfg.max_message_length {
        base.message_chunk_limit = limit;            // config override
    }
    base
};
Self { capabilities, ... }
```

`Channel::capabilities()` 返回 `&self.capabilities`（结构体存在 channel 实例
里）。**不要**在 `capabilities()` 默认实现里读 config——会引入运行时分支和
锁，违背 §6.1 "不可变在构造时确定" 的设计。

Phase 1 channel 实现的字段从原本的几个 `max_message_length / unit` 散字段，
合并为单个 `capabilities: ChannelCapabilities` 字段。

### 13.5 Session.channel transient 字段：当前读取方与不一致点

`Session.channel: Option<Arc<dyn Channel>>` 由 `process_turn` 写入。读取方：

| 读取方 | 现状 | 一致性 |
|---|---|---|
| `Agent::run` 的 `collect_stream` | `session.channel.as_ref()` 调 `push_event(reply_target, ev)` | ✅ 走官方约定 |
| `Agent::run` 的 cancel checkpoint | `session.channel.as_ref()` 调 `cancel_signal(reply_target)` | ✅ 走官方约定 |
| `AskUserTool` | **解析 `session.owner` 字符串 → 自持 `channels: ChannelMap` 反查** | ❌ **绕开 session.channel，自持 channels map** |
| 其他工具 | 不读 | — |

`AskUserTool` 自持 `Option<ChannelMap>` 是历史遗留——`session.channel`
transient 字段晚于 AskUserTool 出现。**应清理为统一约定**：所有需要 channel
访问的工具都应读 `session.channel`，不自持 channels map。

**Phase 2 AskUserTool 改造完整内容**（§13.2 的扩展）：

```rust
// 之前
pub struct AskUserTool {
    router: Option<Arc<AskRouter>>,
    channels: Option<ChannelMap>,   // ← 自持 channels map
}

// 之后（接口一致性后）
pub struct AskUserTool {
    router: Option<Arc<AskRouter>>,
    // 不再自持 channels — 改读 session.channel
}

// execute() 内部
let channel = match &session.channel {
    Some(ch) => ch.clone(),
    None => return Ok(ToolResult::error("ask_user: no channel on session")),
};
let target = SendTarget::new(session.reply_target().unwrap_or(""));
channel.send_payload(&target, &MessagePayload::Text { content: question }).await?;
```

`daemon.rs` 构造 AskUserTool 时不再传 channels map（少一个参数）。

**Phase 4 删 SendMessage 时，唯一在工具层受影响的是 AskUserTool**——
完成 Phase 2 改造后，删除 SendMessage 在工具层零影响。

### 13.6 ChannelMessage（入站侧）的演进

本 RFC 主要谈出站。入站侧 `ChannelMessage` 的字段：

```rust
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub timestamp: u64,
    pub thread_ts: Option<String>,
    pub interruption_scope_id: Option<String>,
    pub attachments: Vec<MediaAttachment>,
    pub image_urls: Option<Vec<String>>,
    pub image_base64: Option<Vec<String>>,
}
```

**Phase 1-4 不动入站侧**——只动出站。但 Phase 5（callback 通用化）需要
入站侧支持结构化按钮回调：

```rust
// Phase 5 候选扩展
pub struct ChannelMessage {
    // ... 现有字段 ...
    /// 用户点击按钮产生的结构化回调（替代当前 "__retry:..." 文本前缀）
    pub callback: Option<InteractionCallback>,
}

pub enum InteractionCallback {
    Retry { session_key_prefix: String },
    Abort { session_key_prefix: String },
    Custom { action: String, payload: serde_json::Value },
}
```

各 channel 在 `listen()` 内构造 ChannelMessage 时填充：Telegram 从
`callback_query.data` 解析，QQBot 从 `INTERACTION_CREATE.button_data` 解析。

Phase 5 时再细化；本 RFC 仅声明入站侧"Phase 1-4 保持现状，Phase 5 加结构化
callback 字段"。

### 13.7 WebhookContext + Recovery：透传，无独立改造

| 模块 | 涉及 Channel 的方式 | Phase 影响 |
|---|---|---|
| `WebhookContext`（axum app state） | 持 `Arc<Orchestrator>`，通过 `orchestrator.channels()` 访问 channel 集合；调 `send_to_target_internal` 派发 | Phase 4 跟随 `send_to_target_internal` 一起改 |
| `Orchestrator::startup_recover_sessions` | 调 `channel.send(MSG_INCOMPLETE_TURN)` 等控制消息 | Phase 4 必改（13.1 表格里已列） |
| `DelegationCoordinator` | 调 `sub_ctx.process_turn(synthetic, None, runtime)` —— **channel 传 None** | **不直接调用 Channel；Phase 4 无影响** |

### 13.8 子代理的 channel 继承：决策点

`DelegationCoordinator::delegate_with_parent` 当前给子代理传 `channel: None`
（见 commit `b26bd61`）。结果：

- 子代理走 streaming（push_event）的能力 = ❌（session.channel = None）
- 子代理调 ask_user 的能力 = ❌（AskUserTool 检查 channel.is_none() 时 fallback
  到"返回 question 让 LLM 自答"模式）

**RFC 决策**：保持现状——子代理是后台 background 任务，不应抢占用户对话
通道。如果将来要让子代理能主动 ask_user（例如 git worktree 子代理需要 push
权限确认），单独 RFC 讨论，**不在 Channel 模型重构范围**。

### 13.9 跨 Phase 迁移依赖图

```
Phase 0  ── 仅改 session_context.rs:223 一处 ────── 无下游依赖
            │
Phase 1  ── ChannelCapabilities + LenUnit ──────── 必须先于 Phase 2
            │     fix Telegram UTF-16 bug
            │
Phase 2  ── MessagePayload + send_payload ──────── 必须先于 Phase 4
            │     ↓ callsite 迁移：
            │       - orchestrator.rs 7 处（含 retry_abort_prompt）
            │       - scheduler.rs 1 处
            │       - tools/ask_user.rs 1 处
            │       - session_context.rs 1 处
            │
Phase 3  ── edit_message / delete_message ──────── 独立，可与 Phase 2 并行
            │
Phase 4  ── 删除 SendMessage ────────────────────── 依赖 Phase 2 callsite 全迁完
            │
Phase 5  ── 结构化 callback ─────────────────────── 独立，依赖 Phase 3 提供 edit
            │     ↓ 同时改造：
            │       - 出站：MessagePayload::Interactive 已有
            │       - 入站：ChannelMessage 加 callback 字段（13.6）
            │
Phase 6+ ── Voice / Poll / Card / 安全策略抽象 ──── 等真实需求 PR
```

**关键依赖**：Phase 2 必须把 §13.1 表格列出的 10 个 callsite 全部迁移，否则
Phase 4 无法删除 `SendMessage`。各 Phase 的"完成定义"必须包含 callsite 验收：
`grep -rn 'SendMessage::new\|channel.send(' src/` 返回空才算 Phase 2 完成。

### 13.10 接口一致性原则（确立约定）

复盘 §13.1–13.9 后，可以总结出 Channel 与其他模块的**官方接口约定**。本 RFC
执行期间和之后，新增 channel 用户必须遵守这些约定，避免再出现像 AskUserTool
那样的孤立模式：

| 调用方类型 | 获取 channel 的官方途径 | ❌ 反模式 |
|---|---|---|
| **工具（`impl Tool`）** | 读 `session.channel.as_ref()` | 自持 `channels: ChannelMap` 字段，再解析 `session.owner` 反查 |
| **运行时核心**（`Agent::run` / process_turn） | 读 `session.channel` 或接受 `Option<Arc<dyn Channel>>` 参数 | 反查 channels map |
| **调度/Webhook**（已有外部入口） | 通过 `Orchestrator::channels()` accessor 或 `send_to_target_internal` | 自持 channels map |
| **Orchestrator 自己** | 自持 `channels: Arc<DashMap>` ✓（它就是所有者） | — |

**为什么不让工具直接接收 channel 参数？**——`Tool::execute(args, session)` 签名
不该为了少数需要 channel 的工具加 `Option<&dyn Channel>` 参数；让全部工具承担
不相关的签名负担。`session.channel` transient 字段已经是 architecture-target.md
line 92-93 定义的标准约定，工具按需读取即可。

**Channel trait 自身保持 Session 无关**：

- `push_event(reply_target: &str, event: TurnEvent)` 以**字符串 key** 寻址，
  不是 `&Session`。让 Channel 不感知 Session 结构，保持解耦。
- `cancel_signal(reply_target: &str) -> Option<CancellationToken>` 同理。
- `send_payload(target: &SendTarget, payload: &MessagePayload)` 同理 ——
  `SendTarget.recipient` 是 routing 字符串，channel 不知道它对应哪个 Session。

**结论**：Channel 与其他模块的接口形态**总体合理，无需重新设计**。唯一的偏差
是 AskUserTool 自持 channels map 这一历史遗留，按本 RFC §13.5 的方案在 Phase 2
随手清理即可。

---

## 附录 A：SendMessage 删除时间线

之前草案是"标记 deprecated 但不删除"——这会创造永久的双 API 状态（新代码用
`send_payload`，旧代码用 `send`），永远不收敛。本 RFC 定下线时间线：

| 时点 | 状态 |
|------|------|
| Phase 2 完成 | 所有 callsite 改用 `send_payload`；`SendMessage` 字段
（`inline_buttons` / `attachments` / `image_urls` / `subject` 等）不再被读 |
| Phase 3 完成 | `Channel::send(&SendMessage)` 加 `#[deprecated]` 标注 |
| Phase 4 完成 | **删除 `Channel::send`、删除 `SendMessage` 结构**。所有 channel 实现只暴露 `send_payload`；调用方只用 `send_payload(target, payload)` |

过渡期（Phase 2 → 4）两个 API 共存：`send_payload` 默认实现内部调用 `send`
作为降级路径；`send` 仍是 trait 上的核心方法。Phase 4 时翻转——`send_payload`
变为 trait 上的核心方法，`SendMessage` 类型整体删除。

理由：保留双 API 的运维成本（文档、新人理解、误用风险）比一次性迁移高得多。
MyClaw 当前没有外部 channel 实现需要考虑兼容（仅 4 个内置 channel）。

## 附录 B：与 RFC Session 架构的关系

本 RFC 不依赖 Session 架构重构（`rfc-session-architecture.md`），也不被其依赖。两者独立实施。唯一交集点是 `SessionContext.process_turn()` 中的 `channel.send()` 调用——Phase 0 的防重复修复在 `orchestrator.rs` 的 spawn block 中，不影响 `process_turn()` 本身。

## 附录 C：已知风险和待决事项

### C.1 Telegram `sendMessageDraft` API 不在 trait 范围

之前草案担心 `sendMessageDraft` API 稳定性。本 RFC §7.2 决策已经把 `send_draft`
从 trait 移除——`sendMessageDraft` 即使将来用，也作为 TelegramChannel 内部实现
细节（在 `send_payload` 或 `edit_message` 内部决定用 sendMessageDraft 还是
sendMessage + editMessageText），调用方完全不感知。API 变动时只影响一个文件，
不影响 trait 公约。

### C.2 `split_message_chunk` 签名变更影响面

当前 `split_message_chunk(message: &str, limit: usize)` 被 3 处调用：
1. `TelegramChannel::chunk_for_telegram`（通过 `raw_limit` 参数）
2. `QQBotChannel::send`（直接调用 `split_message_chunk(&msg.content, QQ_MAX_MESSAGE_LENGTH)`）
3. `QQBotChannel` 的 bot command 回复

改为 `split_message_chunk(message: &str, limit: usize, unit: LenUnit)` 后，所有
调用方都需要加 `LenUnit` 参数。Phase 1 实施时提供兼容包装
`split_message_chunk_chars(message, limit)` = `split_message_chunk(message,
limit, LenUnit::Codepoints)`，让 QQBot 改动可以分批做。

### C.3 默认 `capabilities()` 性能

默认实现返回 `&MINIMAL_CAPABILITIES`（编译期 `static`），只是一次引用复制。
不需要 thread_local 或 LazyLock。各 channel 实现一般会覆盖该方法返回各自的
`&'static ChannelCapabilities`，开销同样为零。

### C.4 流式中断时的兜底

Phase 0 加 `!supports_streaming()` 守卫后，streaming channel 完全依赖
`push_event` 链路交付最终文本。如果 push_event 失败（WebSocket 断开等），用户
可能看不到回复。

当前依赖 ClientChannel 自身的 `dedup_state` 重连重放机制（已存在）。后续如果
出现可观测的丢失率，可以扩展 Channel trait 加 `acknowledge_event(reply_target,
event_id)` 之类的确认机制，让上层在 ack 超时时退回 `send()` 兜底。本 RFC 不
强求此扩展——属于"出现问题再做"的范畴。
