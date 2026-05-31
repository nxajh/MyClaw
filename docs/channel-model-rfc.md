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
14. [Phase 4 — 群组安全策略归一（方向 C 设计）](#14-phase-4--群组安全策略归一方向-c-设计)
15. [附录](#附录-a-sendmessage-删除时间线)

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

### 5.3 流式可靠性归属

加上 `!supports_streaming()` 守卫后，streaming channel **完全依赖**
`push_event` 链路交付最终文本。可靠性责任明确归到 channel 实现侧 —— 详细契约
见 §7.5（Streaming 可靠性契约）。

简单总结：
- ClientChannel 已经有 `dedup_state` 做 WebSocket 断连重连后的事件 replay
- `push_event` 改为返回 `Result<()>`（默认 `Ok(())`）让 channel 实现可以 log
  / metric 失败事件，但 `Agent::run` 不基于该返回值做策略决策
- 如果将来出现可观测的丢失率，再考虑扩展 trait（如 `send_event_ack`）；
  当前不预留

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
    /// 返回 `Result<()>` 用于诊断（如 channel 实现想 log "WS 断连，event
    /// 丢弃"），调用方一般 `let _ = ch.push_event(...).await;` 忽略错误。
    /// Fire-and-forget 语义不变 —— 见 §7.5 流式可靠性契约。
    async fn push_event(&self, _reply_target: &str, _event: TurnEvent) -> anyhow::Result<()> {
        Ok(())
    }

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

**`push_event` 签名变化**：从 `→ ()` 改为 `→ Result<()>`（默认 `Ok(())`）。
这是**唯一一个改动签名**的现有方法。现有调用方都是 `let _ = ch.push_event(...)`
形态，加一个 `let _ =` 即可保持原有 fire-and-forget 行为。详见 §7.5。

**Phase 1.5 端到端替代**：§7.6 提出用 `TurnStream` trait 替代 `push_event` +
`cancel_signal`。Phase 1.5 落地时，这两个 trait 方法被删除，新增
`create_stream(rt) -> Option<Box<dyn TurnStream>>`。Phase 0-1 仍保留 `push_event`。

### 7.5 流式可靠性契约

`push_event` 在表面上是 fire-and-forget（调用方不用 retry，不用回退），但
**这建立在 channel 实现对自己 streaming 通道负责的前提下**。trait 把这个契约
显式写出来：

> **Channel 实现声明 `supports_streaming() == true`（即 `capabilities()
> .supports_streaming = true`）的，承担以下契约：**
>
> 1. **有序送达**：`push_event(reply_target, e1)` 在 `push_event(reply_target, e2)`
>    之前 await 完成时，e1 必须在 e2 之前抵达消费者。
> 2. **断连重连透明**：传输层临时失败（WebSocket 断连重连等）由 channel 内部
>    解决，包括必要时的事件重放（replay）。`Agent::run` 不会重试 `push_event`，
>    也不会因为流式失败而 fallback 到 `send_payload`。
> 3. **`TurnEvent::Done` 后停止**：channel 在收到 `Done` 后视为本轮结束；
>    `Agent::run` 不会再 push 任何 event。channel 实现可以借此清理
>    per-reply_target 资源。
> 4. **Cancel 抢断**：用户在 `cancel_signal(rt)` 返回的 token 上发出 cancel
>    后，`Agent::run` 退出且**不**再 push `Done`。channel 应当能识别"看到 cancel
>    后流不再有 Done" 的情况，作为流终结信号处理。
> 5. **未知 reply_target**：`push_event(rt, ev)` 调用时 channel 内若没有 `rt`
>    对应的活跃 stream（比如已经在客户端断开后清理过），实现可以静默丢弃但
>    **建议 log**，便于排查。

**声明 `supports_streaming() == false`** 的 channel：default `push_event`
no-op 实现 + `Ok(())` 满足契约。`Agent::run` 的 §8.1 守卫保证它的最终文本走
`send_payload` 兜底，**不**依赖 push_event 送达。

**为什么不让 Agent.run 自己重试或回退？** —— 把可靠性逻辑下沉到 channel 实现
的理由：
1. 不同传输（WS / SSE / Telegram editMessageText）的重试语义差别大，统一在
   Agent.run 写会很丑
2. ClientChannel 已经有 `dedup_state` 重连重放机制；这是 channel 私有实现细节
3. Agent.run 一旦做 fallback，就回到了 Phase 0 之前的双发 bug

**实现侧建议**：channel 实现 `push_event` 时返回 `Err` 主要用于 log /
metrics / 测试 assert，不期望调用方做策略决策。

### 7.6 TurnStream — `push_event` 的强化替代方案（目标态）

#### 7.6.1 push_event 在 §7.5 契约下暴露的两个弱点

§7.5 已经把可靠性契约写清了，但 `push_event(&str, TurnEvent)` 这个签名本身仍有
两个结构性弱点：

1. **per-turn 状态的归属是 channel 私有的、按 `reply_target` 字符串索引的 map**
   （`ClientChannel.stream_contexts: HashMap<String, StreamContext>`）。
   - 新建 / 销毁的时机散落在 `push_event` 实现里靠"看见首个 chunk 才创建、看见
     Done 才清理"隐式管理；cancel 抢断、客户端断连等异常路径会留下孤立条目。
   - Agent 那侧每次 push 都做一次字符串 hash 查找，热路径上微小但持续的开销。
2. **`push_event` 只能返回 `Result<()>`**，无法告诉 Agent "这次 event 是已缓冲
   待发还是已被消费端确认"。日后想做 backpressure（如客户端处理慢就暂停
   chunk 生成）或 ack-based 重试，签名都不够。

#### 7.6.2 替代设计：把 per-turn 流抽出为 `TurnStream` trait

```rust
/// 一次 push_event 的送达态。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StreamDelivery {
    /// 已缓冲，未确认抵达消费者（默认）。
    #[default]
    Pending,
    /// 已抵达消费端（如 WS send 调用返回，HTTP 200，editMessageText 成功）。
    Visible,
    /// 消费端已确认完成（如 client ack / Telegram 最终 edit 成功）。
    FinalDelivered,
}

/// 一次 agent turn 的流式出口。由 `Channel::create_stream` 工厂创建，
/// 生命周期 = 一个 turn。
#[async_trait]
pub trait TurnStream: Send {
    /// 推一个 event。返回当前送达态；返回 `Err` 表示传输层永久失败，
    /// channel 实现已无法继续推送（调用方应停止推送但不必 fallback）。
    async fn push(&mut self, event: TurnEvent) -> anyhow::Result<StreamDelivery>;

    /// 当前累积送达态（不触发新动作，只读）。
    fn status(&self) -> StreamDelivery;

    /// 正常收尾：等待最终 event 抵达消费者。实现 SHOULD await 网络确认或
    /// best-effort timeout。返回 `FinalDelivered` 表示已确认，`Visible` 表示
    /// 已发送但未收到 ack。
    async fn finish(self: Box<Self>) -> StreamDelivery;

    /// 异常收尾：取消未完成的传输，不再 await ack。
    async fn abort(self: Box<Self>);

    /// 用户侧 cancel 抢断 token。Agent::run 监听此 token 决定是否提前退出。
    fn cancel_token(&self) -> Option<CancellationToken> { None }
}
```

Channel trait 上增加一个工厂方法：

```rust
/// 为本轮 turn 创建流式出口。
/// 不支持流式的 channel 返回 None；调用方走 send_payload 兜底。
fn create_stream(&self, reply_target: &str) -> Option<Box<dyn TurnStream>> {
    let _ = reply_target;
    None
}
```

Session 新增一个 transient 字段（不持久化，不进 snapshot）：

```rust
// src/agents/session.rs — Session struct 新增
pub turn_stream: Option<Box<dyn TurnStream>>,  // transient: per-turn 重置
```

`Agent::run` 的 streaming 路径从 `channel.push_event(rt, ev).await` 改为
`if let Some(s) = &mut session.turn_stream { s.push(ev).await }`。

#### 7.6.3 比 `push_event` 强在哪

| 维度 | `push_event(&str, TurnEvent)` | `TurnStream` |
|---|---|---|
| Per-turn 状态归属 | channel 私有 HashMap，字符串索引 | `Box<dyn TurnStream>` owned by Session |
| 生命周期 | 隐式（首 chunk 起，Done 止） | 显式（`create_stream` → `push`* → `finish/abort`）|
| Cancel | 独立方法 `cancel_signal(rt)` | `stream.cancel_token()`，与 stream 同生命周期 |
| 送达反馈 | `Result<()>` 两态 | `StreamDelivery` 三态（Pending/Visible/Final）|
| 热路径开销 | 每次 push 做 string hash 查找 | 直接 `&mut self` 调用 |
| 异常清理 | 依赖 channel "看不到 Done 就漏" | 走 `abort` 或 Drop 兜底（见 §7.6.5） |
| Telegram edit-streaming | 难——`push_event` 需把 message_id 状态藏在 HashMap | 自然——`TelegramEditStream` 内部持有 message_id |
| 第三方 channel 自带 stream | 可以但 reply_target map 是黑盒 | 工厂返回 `Box<dyn TurnStream>`，完全开放 |

#### 7.6.4 不变量声明（§7.5 在 TurnStream 下的重述）

声明 `create_stream(rt) -> Some(_)` 的 channel 实现，承担：

1. **有序送达**：连续 `push` 的 event 必须按调用顺序抵达消费者。
2. **断连重连透明**：传输层临时失败由 stream 实现内部解决（缓冲、replay）。
3. **`Done` 后停止**：Agent 推送 `TurnEvent::Done` 后不再 push 新 event；
   随后调用 `finish` 收尾。
4. **Cancel 抢断**：`cancel_token` 被 cancel 后，Agent 退出且**不**再 push `Done`，
   走 `abort` 收尾。stream 应识别此模式。
5. **意外丢弃 = abort**：Session 析构或 turn_stream 字段被覆盖时，Drop 触发
   best-effort abort（见 §7.6.5）。

#### 7.6.5 两条来自候选方案的强化（**必须实现**）

**(a) `push` 返回 `Result<StreamDelivery>` 而非 `StreamDelivery`**

允许传输层错误上抛供 Agent 决定是否提前结束（不重试，但可短路后续 push 节省
LLM 输出）。当前态用 `StreamDelivery` 表达。

**(b) Drop 兜底 = abort 安全网**

```rust
impl Drop for ClientTurnStream {
    fn drop(&mut self) {
        if !self.finished {
            let cancel = self.cancel.clone();
            tokio::spawn(async move { cancel.cancel(); });
        }
    }
}
```

任何意外丢弃（Session 重置、panic unwind、字段被覆盖）都不会留下悬挂资源。
`finish` / `abort` 仍是首选路径——它们能 await 确认；Drop 只是兜底。

#### 7.6.6 与 `push_event` 的关系：迁移、不并存

| 时点 | 状态 |
|---|---|
| Phase 0（按 §8.1 原计划） | 仅加 `!supports_streaming()` 守卫，保持 `push_event` 接口 |
| Phase 1.5（新增 phase） | 引入 `TurnStream` / `StreamDelivery` / `Channel::create_stream`；ClientChannel 提供 `ClientTurnStream` 实现；Session 加 `turn_stream` 字段；Agent::run 切到 `session.turn_stream` 路径 |
| Phase 1.5 完成 | 删除 `Channel::push_event` 和 `Channel::cancel_signal`；删除各 channel 的 `stream_contexts` map |

§7.5 的契约在 Phase 1.5 后由 §7.6.4 重述版替代，但**语义不变**——只是从 channel
全局 map 索引改为 per-stream owned state。

#### 7.6.7 待 RFC 落地时还需确认

- `finish` 返回值能 await 到什么程度？ClientChannel 走 WS ack 可以确认 Visible→
  FinalDelivered；TelegramEditStream 走 HTTP 200 也可以；但实现 SHOULD 文档化
  "最长 await N 秒后降级为 Visible 返回"。
- `create_stream` 返回 None 后 `session.turn_stream` 保持 None；Agent::run 的
  push 路径需要 `if let Some(s) = &mut session.turn_stream` 短路——Phase 1.5
  的 §11 实施计划需把这点列入 callsite 改造清单。
- `Box<dyn TurnStream>` 必须 `Send`（已在 trait 上）——Session 跨 await，不
  Send 的实现会在编译期挡住。
- `Session::clone` 需自定义实现：`turn_stream` 不可 clone，clone 时设为 None。

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

### Phase 1.5：TurnStream 替代 push_event（P1，依赖 Phase 0）

**范围**（见 §7.6 完整设计）：

- 新增类型：`StreamDelivery`（Pending/Visible/FinalDelivered）、`TurnStream` trait
- Channel trait 新增 `create_stream(reply_target) -> Option<Box<dyn TurnStream>>`
  工厂方法
- **删除** `Channel::push_event` 和 `Channel::cancel_signal`（两者职责合并进
  `TurnStream`）
- Session struct 新增 transient 字段 `turn_stream: Option<Box<dyn TurnStream>>`，
  custom `Clone` 重置为 None；snapshot 序列化跳过
- ClientChannel：实现 `ClientTurnStream`，把现有 `stream_contexts: HashMap<...>`
  里的逐 reply_target 状态收进 stream owned state；删除 channel 上的
  HashMap 字段
- Agent::run：streaming push 路径从 `channel.push_event(rt, ev).await` 改为
  `if let Some(s) = &mut session.turn_stream { s.push(ev).await? }`；cancel
  监听从 `channel.cancel_signal(rt)` 改为 `session.turn_stream.as_ref()
  .and_then(|s| s.cancel_token())`
- SessionContext::process_turn：在 agent.run 前调用 `channel.create_stream(rt)`
  填充 `session.turn_stream`；agent.run 返回后调用 `finish`（正常）或
  `abort`（错误）
- 所有 TurnStream 实现：加 Drop 兜底（§7.6.5(b)），意外丢弃 = best-effort abort

**callsite 改造清单**：

| 文件 | 改动 |
|---|---|
| `src/agents/agent.rs` | `collect_stream` 内 `channel.push_event(rt, ev)` → `session.turn_stream.as_mut()` 路径；cancel 监听同步切换 |
| `src/agents/session.rs` | 加 `turn_stream` 字段 + custom Clone |
| `src/agents/session_context.rs` | process_turn 入口创建 stream，出口 finish/abort；Phase 0 守卫从 `!ch.supports_streaming()` 改为 `session.turn_stream.is_none()`（两者等价但更直接） |
| `src/channels/message.rs` | trait 删 `push_event` / `cancel_signal`，加 `create_stream` 默认 None |
| `src/channels/client.rs` | 实现 `ClientTurnStream`，删 `stream_contexts` |
| `src/channels/telegram/channel.rs` | 暂保持 `create_stream → None`，Phase 3 后再实现 `TelegramEditStream` |

**Phase 0 守卫语义同步**：Phase 0 的 `!ch.supports_streaming()` 守卫在 Phase 1.5
后等价于 `session.turn_stream.is_none()`——None 意味着 channel 不支持流式（或
本轮显式关闭），需走 send_payload 兜底；Some 意味着流式已交付完整文本，
不重复发。两种写法**任选其一保留**即可（推荐后者，更直接）。

**风险**：中。涉及 Session 字段、Agent::run 路径切换、ClientChannel 重构。
但全部在 Phase 0 守卫保护下进行——任何阶段回归到双发 bug 都能被 §12 Phase 0
测试用例捕获。

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

### Phase 4：群组安全策略归一（P2，方向 C — 详见 §14）

**摘要**（完整设计见 §14）：

- 新增类型：`ChannelSecurityPolicy` 数据结构 + `AllowList` / `GroupAuthMode` /
  `MessageScope` / `AuthDecision` 枚举
- Channel trait 新增 `security_policy(&self) -> &ChannelSecurityPolicy` +
  `check_authorization(sender, scope) -> AuthDecision`
- **破坏性变更**：Wechat 的 `allowed_users = []` 语义从"允许全部"统一为"拒绝
  全部"（与 Telegram 对齐）；保留 `["*"]` 作为"显式允许全部"的统一约定
- 启动期 warn log 兜底；CHANGELOG 标注 BREAKING

**风险**：中。Wechat 配置语义变更需要 release note + warn log；refactor
所有 channel 的 inline 检查到 policy 调用。

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

### Phase 1.5

- [ ] ClientChannel `create_stream(rt)` 返回 `Some(_)`，QQBot/Wechat 返回 `None`
- [ ] `ClientTurnStream::push` 在 WS 正常时返回 `Ok(Visible)`，断连重连期间
  返回 `Ok(Pending)`，最终 `finish` 返回 `FinalDelivered`
- [ ] Agent::run 不再调用 `channel.push_event` / `channel.cancel_signal`
  （grep 验证）
- [ ] `session.turn_stream` 在 turn 结束（正常 / 错误 / cancel）后被重置或
  drop；连续多 turn 不泄漏 stream
- [ ] Cancel：在 `cancel_token` 被触发后，Agent::run 退出且 `session.turn_stream`
  走 `abort` 收尾（不 finish）
- [ ] Drop 兜底：在 Agent::run panic 路径下，Session 析构触发
  `ClientTurnStream::drop`，cancel_token 被点亮（验证孤立 WS 资源被清理）
- [ ] 双发回归：WebUI 单次提问，回复仍只显示一次（Phase 0 守卫等价改写后保持
  正确）

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

## 14. Phase 4 — 群组安全策略归一（方向 C 设计）

> 本节是 §11 Phase 4 的深入设计文档。结论：在三种候选方向中选 **C — 语义归一**，
> 用一次性、可观测、可文档化的破坏性变更换长期清爽的数据模型。

### 14.1 当前安全检查的散落形态

| Channel | 检查位置 | 数据 | 空白名单语义 |
|---------|---------|------|------------|
| **Telegram** | `poll_loop` 内 inline | `Arc<RwLock<Vec<String>>>` + `mention_only: bool` | **拒绝全部** + warn log |
| **QQBot** | `is_user_allowed` / `is_group_allowed`，3 处调用 | `config.allow_from: Option<Vec<String>>` + `config.group_allow_from` | `None` = **允许全部**；`Some(empty)` 等同 `None` |
| **Wechat** | inline 一处 | `config.allowed_users: Vec<String>` | 空 = **允许全部** |
| **Client** | 连接层 token | — | 不适用（连接通过即可信） |

**三个 channel 三套语义** —— 是 Phase 4 必须解决的根本问题。Telegram 的
"empty = closed" 是安全默认；Wechat / QQBot 的 "empty = open" 是历史遗留。

### 14.2 决策：方向 C 而非 A/B

候选方向回顾：

- **A**（仅命名约定）：trait 方法包装现有 inline 检查，零语义变更。
- **B**（保留异构数据模型）：`AllowList { Open, Whitelist(Vec), WhitelistStrict(Vec) }`
  把语义差异编码到类型里。
- **C**（语义归一）：所有 channel 统一为"empty = closed"，破坏性变更 Wechat。

**为什么 C 在"有需求"前提下胜出 B**：

1. **B 的 `WhitelistStrict` 是永久负债** —— 新代码不会主动用它，只是为兼容
   存在；下一次有人加 channel 时面临"我该用 Whitelist 还是 WhitelistStrict？"
   的决策瘫痪。
2. **Admin UI 渲染 B 是 UX 灾难** —— "为什么 `Whitelist([])` 和
   `WhitelistStrict([])` 看起来一样行为不同？"用户无法理解。
3. **C 的破坏性远比纸面看起来小**：
   - Wechat 故意配置空 `allowed_users` 让所有人能聊 = 反模式，几乎无人采用
   - 默认拒绝是更安全的方向（防止"忘了配白名单 = 全开"事故）
   - `["*"]` 已经是 Telegram 的"显式允许全部"约定，迁移成本为一行 config
4. **C 的破坏可观测**：启动时 warn log + CHANGELOG 一次性消化，过渡期短

### 14.3 新增类型（`src/channels/security.rs` 新文件）

```rust
/// 哪些用户被允许向本 channel 发消息。
///
/// 跨 channel 统一语义：`Whitelist(empty)` = 拒绝全部；要"允许全部"
/// 必须显式用 `All`（config 写 `allowed_users = ["*"]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowList {
    /// 显式允许全部。Config 写 `["*"]`，或单独的 wildcard 标记。
    All,
    /// 限定白名单。空集 = 拒绝全部，会在启动期触发 warn log。
    Whitelist(Vec<String>),
}

impl AllowList {
    /// 从 config 的 `Option<Vec<String>>` 解析为本类型。
    /// `None`               → `All`（向后兼容 QQBot 历史语义）
    /// `Some(vec ["*"])`    → `All`
    /// `Some(empty)`        → `Whitelist(vec![])` 拒绝全部（与 Telegram 对齐）
    /// `Some(vec)` 否则     → `Whitelist(vec)`
    pub fn from_config(opt: Option<Vec<String>>) -> Self {
        match opt {
            None => Self::All,
            Some(v) if v.iter().any(|s| s == "*") => Self::All,
            Some(v) => Self::Whitelist(v),
        }
    }

    pub fn allows(&self, sender: &str) -> bool {
        match self {
            Self::All => true,
            Self::Whitelist(v) => v.iter().any(|u| u == sender),
        }
    }
}

/// 群消息处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupAuthMode {
    /// 拒绝所有群消息（仅响应 1:1）
    Reject,
    /// 接受所有群消息
    Open,
    /// 仅响应 @提及本 bot 的群消息（Telegram 现有 mention_only=true 行为）
    MentionOnly,
}

/// 完整的安全策略数据。每个 channel 暴露一份，admin UI / 配置工具
/// 可以读取或（未来）写入。
#[derive(Debug, Clone)]
pub struct ChannelSecurityPolicy {
    pub allowed_users: AllowList,
    /// 群消息策略 + 群白名单。`group_allowlist` 与 `allowed_users` 独立
    /// 控制：用户白名单决定"个人发消息能否被回复"，群白名单决定
    /// "在哪些群里被听到"。
    pub group_mode: GroupAuthMode,
    pub group_allowlist: AllowList,
}

impl ChannelSecurityPolicy {
    /// 安全的 "all open" 默认（用于 Client 这类无平台级鉴权概念的 channel）。
    pub fn open() -> Self {
        Self {
            allowed_users: AllowList::All,
            group_mode: GroupAuthMode::Open,
            group_allowlist: AllowList::All,
        }
    }
}

/// 入站消息的作用域（用于 `check_authorization`）。
/// `'a` 让调用方避免 String 分配，热路径调用零成本。
#[derive(Debug, Clone, Copy)]
pub enum MessageScope<'a> {
    /// 1:1 私聊。
    Direct,
    /// 群组消息。`id` 用于查 group_allowlist；
    /// `has_mention` 用于 MentionOnly 模式。
    Group { id: &'a str, has_mention: bool },
}

/// 鉴权决策。三态而非 bool —— 调用方需要区分"静默丢弃"和"显式拒绝"
/// 以决定是否 log 或回 ack。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    /// 静默丢弃。例：群消息没 @bot 且 mention_only=true ——
    /// 这是预期路径，不该 warn。
    Ignore,
    /// 显式拒绝，调用方应当 log（含 reason）。例：白名单外的用户
    /// 试图私聊 —— 安全相关，需可追溯。
    Reject { reason: &'static str },
}

impl AuthDecision {
    pub fn allowed(self) -> bool { matches!(self, Self::Allow) }
}
```

### 14.4 Channel trait 改动

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    // ── 既有方法不变 ──

    /// 本 channel 的安全策略快照。Admin UI / 配置工具的读取入口。
    /// 默认 `ChannelSecurityPolicy::open()`（Client 适用）。
    fn security_policy(&self) -> ChannelSecurityPolicy {
        ChannelSecurityPolicy::open()
    }

    /// 对单条入站消息做鉴权决策。Channel 自己的 listen/poll loop
    /// 在 forward 给 orchestrator 前调用本方法。
    /// 默认实现走 `security_policy()`：抽出快照，按字段判定。
    fn check_authorization(&self, sender: &str, scope: MessageScope<'_>) -> AuthDecision {
        let policy = self.security_policy();
        match scope {
            MessageScope::Direct => {
                if policy.allowed_users.allows(sender) {
                    AuthDecision::Allow
                } else {
                    AuthDecision::Reject { reason: "user not in allowed_users" }
                }
            }
            MessageScope::Group { id, has_mention } => {
                match policy.group_mode {
                    GroupAuthMode::Reject => AuthDecision::Ignore,
                    GroupAuthMode::MentionOnly if !has_mention => AuthDecision::Ignore,
                    _ => {
                        if !policy.group_allowlist.allows(id) {
                            AuthDecision::Reject { reason: "group not in allowlist" }
                        } else if policy.allowed_users.allows(sender) {
                            AuthDecision::Allow
                        } else {
                            AuthDecision::Reject { reason: "sender not in allowed_users" }
                        }
                    }
                }
            }
        }
    }
}
```

**热重载支持**：`security_policy()` 返回值类型（不是引用），channel 可以
内部用 `Arc<RwLock<ChannelSecurityPolicy>>`，每次调用 clone 出快照。每次
调用一次 mutex + clone，对热路径来说可接受（一个 `Vec<String>` clone 在
百微秒级，且 listen loop 本身就是 I/O bound）。

### 14.5 各 channel 的迁移

| Channel | 改动 |
|---------|------|
| **Client** | 用默认 `security_policy() = open()`；连接层 token 校验保留 |
| **Telegram** | `security_policy()` 从 `Arc<RwLock<Vec<String>>>` + `mention_only` 合成；`is_user_allowed` 删除，poll_loop 改调 `check_authorization` |
| **QQBot** | `security_policy()` 从 `config.allow_from` + `config.group_allow_from` 合成（走 `AllowList::from_config`）；`is_user_allowed` / `is_group_allowed` 删除 |
| **Wechat** | `security_policy()` 从 `config.allowed_users` 合成（走 `from_config`）；inline 检查删除 |

每个 channel 的 listen / poll / receive loop 把原有的"if 拒绝就 continue"
改成：

```rust
match self.check_authorization(&sender, scope) {
    AuthDecision::Allow => { /* forward to orchestrator */ }
    AuthDecision::Ignore => { continue; }
    AuthDecision::Reject { reason } => {
        warn!(sender = %sender, reason, "channel rejected inbound");
        continue;
    }
}
```

### 14.6 破坏性变更细则

| 平台 | Phase 4 前 | Phase 4 后 | 修复方法 |
|------|-----------|-----------|--------|
| Telegram `allowed_users = []` | 拒绝全部 | 拒绝全部 | 无变化 |
| Telegram `allowed_users = ["alice"]` | alice 允许 | alice 允许 | 无变化 |
| Telegram `allowed_users = ["*"]` | 允许全部 | 允许全部 | 无变化 |
| QQBot `allow_from` 未设 | 允许全部（None）| 允许全部（None）| 无变化 |
| QQBot `allow_from = []` | 允许全部（None 等价）| **拒绝全部** | 改为 `["*"]` 或移除字段 |
| QQBot `allow_from = ["uid"]` | uid 允许 | uid 允许 | 无变化 |
| **Wechat `allowed_users = []`** | **允许全部** | **拒绝全部** | 改为 `["*"]` |
| Wechat `allowed_users = ["uid"]` | uid 允许 | uid 允许 | 无变化 |

**唯一真正破坏**：Wechat 空数组 + QQBot 空数组（QQBot 的 `None` vs `Some([])`
原本等价，归一后 `Some([])` 变成拒绝）。

### 14.7 启动期 warn log 兜底

每个 channel 启动时（`listen()` 第一次成功后）调用一次：

```rust
fn warn_if_locked_down(&self) {
    let policy = self.security_policy();
    if matches!(policy.allowed_users, AllowList::Whitelist(ref v) if v.is_empty()) {
        warn!(
            channel = %self.name(),
            "allowed_users is empty — channel will reject all messages. \
             To allow all senders, set allowed_users = [\"*\"]. \
             (Phase 4 behavior change for wechat/qqbot, see CHANGELOG)"
        );
    }
}
```

依赖运行时观察，配上文档说明，足够引导用户自助修复。

### 14.8 CHANGELOG 草稿

```markdown
## BREAKING: Channel security policy unified (Phase 4)

`allowed_users` semantics now match across all channels:

- Empty list = reject all messages (was: allow-all on wechat/qqbot)
- ["*"] = explicitly allow all (was: telegram-only convention)
- Omitted field = allow all (QQBot's prior `None` behavior, preserved)

If your wechat or qqbot config has `allowed_users = []` and you want to
keep the prior "allow all" behavior, change it to `allowed_users = ["*"]`.
Otherwise the channel will start with all messages rejected and log a
warning at startup.

Telegram is unaffected; this brings the other channels to its behavior.
```

### 14.9 测试

```rust
#[test]
fn allow_list_from_config() {
    use AllowList::*;
    assert_eq!(AllowList::from_config(None), All);
    assert_eq!(AllowList::from_config(Some(vec![])), Whitelist(vec![]));
    assert_eq!(AllowList::from_config(Some(vec!["*".into()])), All);
    assert_eq!(
        AllowList::from_config(Some(vec!["alice".into()])),
        Whitelist(vec!["alice".into()])
    );
}

#[test]
fn telegram_default_check_authorization() {
    let ch = TelegramChannel::new(config_with_users(&["alice"]));
    assert!(ch.check_authorization("alice", MessageScope::Direct).allowed());
    assert!(!ch.check_authorization("bob", MessageScope::Direct).allowed());
}

#[test]
fn group_mention_only_ignores_unmentioned() {
    let ch = TelegramChannel::new(config_with_mention_only());
    assert_eq!(
        ch.check_authorization("alice", MessageScope::Group { id: "g1", has_mention: false }),
        AuthDecision::Ignore
    );
    assert_eq!(
        ch.check_authorization("alice", MessageScope::Group { id: "g1", has_mention: true }),
        AuthDecision::Allow
    );
}

#[test]
fn wechat_empty_allowlist_now_rejects() {
    // BREAKING regression test
    let ch = WechatChannel::new(config_with_users(&[]));
    assert!(!ch.check_authorization("anyone", MessageScope::Direct).allowed());
}
```

### 14.10 不在 Phase 4 范围内（保留给未来）

- **Orchestrator-level 中央拦截**：Phase 4 仍由 channel 内部调用
  `check_authorization`。trait 方法已经就位，未来若要在 `handle_channel_event`
  入口加一次调用做 audit log，只是一行新代码。
- **跨 channel 策略**（"alice 在 telegram 允许但 wechat 不允许"）：当前数据
  模型在 channel 内部，无法表达跨 channel 规则。需要时另起 Policy Service。
- **动态权限**（运行时通过 API 修改 policy）：`security_policy()` 返回值
  而非引用是为此预留扩展空间，但 admin API 不在 Phase 4 范围。

### 14.11 风险与回滚

**风险**：

1. Wechat / QQBot 用户配置空数组并依赖"allow all"行为 → 启动期 warn log +
   CHANGELOG 引导自助修复
2. 热路径调用 `security_policy()` clone Vec<String> → listen/poll 本就是
   I/O bound，clone 开销可忽略；如成为瓶颈再换 `Arc<...>` 共享

**回滚**：Phase 4 是纯 channel 内部重构，不动 orchestrator / session。
若发现 Wechat 用户大面积配置依赖旧行为，可在下一个 patch release 临时
加一个 `allowed_users_empty_means_open: bool = false` 配置项作为兼容
开关，下两个 release 强制移除。

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

已在正文 §7.5（Streaming 可靠性契约）和 §7.6（TurnStream 替代方案）展开。简要：

- Phase 0 守卫让 streaming channel 完全负责自己的可靠性
- `push_event` 返回 `Result<()>` 用于 log/metric，不参与策略决策
- ClientChannel 已有 `dedup_state` 重连重放机制
- Phase 1.5 切到 `TurnStream` 后：`push` 返回 `Result<StreamDelivery>` 提供
  Pending/Visible/FinalDelivered 三态；Drop 触发 best-effort abort 兜底
- 若出现实际可观测丢失率再扩展 trait（如 `acknowledge_event(rt, event_id)`），
  当前不预留
