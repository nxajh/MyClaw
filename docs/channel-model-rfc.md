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
8. [Orchestrator 改动](#8-orchestrator-改动)
9. [配置变更](#9-配置变更)
10. [各 Channel 实现影响](#10-各-channel-实现影响)
11. [实施阶段](#11-实施阶段)
12. [测试计划](#12-测试计划)
13. [附录](#附录-a-sendmessage-保留-vs-淘汰)

---

## 1. 问题陈述

### 1.1 WebUI 回复显示两次

Agent 回复通过两条互不感知的路径同时到达 WebUI：

1. **Streaming 路径**：`Agent::run` → `collect_stream` → `push_event(TurnEvent::Chunk/Done)` → WebSocket → WebUI
2. **`channel.send` 路径**：`process_turn` 返回后 → orchestrator 无条件调用 `channel.send()` → WebSocket → WebUI

Streaming 已交付完整文本后，`channel.send` 又发一次 `{"type":"message","content":"..."}` ，WebUI 检测到 `done:true` 的 assistant 消息已存在，创建一条全新的重复消息。

**根因**：orchestrator 不区分 streaming 和非 streaming channel，对所有 channel 都执行 `send()`。

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

1. 修复 WebUI 回复双显示 bug
2. 引入 `MessagePayload` 枚举统一文本/媒体/按钮/投票/语音/卡片的发送接口，替代 `SendMessage` 字段膨胀
3. 新增 `edit_message` / `delete_message` / `send_draft`，为非 WebUI channel 的流式推送和交互闭环打基础
4. 引入 `ChannelCapabilities` 结构体，集中声明各 channel 的能力
5. 按 platform 实际计量单位切分消息
6. 统一平台文本清洗（`format_message`）
7. 预留群组安全策略抽象

### 非目标

- 不做 OpenClaw 式的 15+ adapter 插件组合（Rust trait 组合天然支持）
- 不做 Hermes 式的外挂 `GatewayStreamConsumer`（`push_event` + `TurnEvent` 已是等价设计）
- 不做完整插件系统（`before_send` / `after_send` 钩子作为 P3 预留）
- 不做跨 channel 的 presentation 抽象渲染层（`renderPresentation`），由各 channel 自行处理 payload

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
      → return TurnResult { text }
    ← TurnResult
    → channel.send(SendMessage { text })  ──────────── 路径 2: 重复!
```

**Orchestrator 中所有 `channel.send()` 调用点**（共 9 处）：

| 位置 | 场景 | 走 streaming？ | Phase 0 影响 |
|------|------|---------------|-------------|
| L591 | 无 pending retry 的回调响应 | 否 | 无 |
| L597 | abort 确认 | 否 | 无 |
| L628 | incomplete turn 提示（retry/abort 按钮） | 否 | 无 |
| L678 | command 响应（如 /help、/status） | 否 | 无 |
| **L724** | **process_turn 结果发送** | **是（ClientChannel）** | **需加条件** |
| L731 | process_turn 错误发送 | 始终发送 | 无（错误需保证送达） |
| L884 | startup recovery 恢复响应 | 否（无 push_event） | 无 |
| L1322 | startup recovery 发送 | 否 | 无 |

**结论**：只有 L724 一处需要加 `!channel.supports_streaming()` 条件判断。其余场景都是一次性控制消息或非 streaming 路径，不受影响。

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
| 发送接口 | `sendText/Media/Payload/Poll` | `send` + `edit_message` + `send_draft` | `send_payload` 统一 + `edit/delete/draft` |
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
// orchestrator.rs — process_turn 返回后
Ok(turn_result) => {
    if !turn_result.text.trim().is_empty()
        && !channel.supports_streaming()  // streaming channel 已通过 TurnEvent 交付
    {
        let send_msg = SendMessage::new(turn_result.text, reply_target);
        let _ = channel.send(&send_msg).await;
    }
}
```

未来当 Telegram/QQBot 也支持 edit-based streaming 时，它们的 `supports_streaming()` 会返回 `true`，同一个判断点自然生效。

---

## 6. 新增类型

### 6.1 ChannelCapabilities

```rust
/// Channel 能力声明。不可变，在 channel 构造时确定。
#[derive(Debug, Clone)]
pub struct ChannelCapabilities {
    // ── 传输能力 ──
    /// 是否支持 streaming events（push_event + TurnEvent）
    pub supports_streaming: bool,
    /// 是否支持编辑已发消息（edit_message）
    pub supports_edit: bool,
    /// 是否支持删除消息（delete_message）
    pub supports_delete: bool,
    /// 是否支持原生 draft streaming（send_draft），如 Telegram sendMessageDraft
    pub supports_draft: bool,

    // ── 内容能力 ──
    /// 是否支持发送媒体文件（图片/文件）
    pub supports_media: bool,
    /// 是否支持发送投票
    pub supports_poll: bool,
    /// 是否支持发送语音消息
    pub supports_voice: bool,
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
    /// 默认能力：不支持任何高级特性，4096 codepoint 限制
    pub fn minimal() -> Self {
        Self {
            supports_streaming: false,
            supports_edit: false,
            supports_delete: false,
            supports_draft: false,
            supports_media: false,
            supports_poll: false,
            supports_voice: false,
            supports_buttons: false,
            supports_threads: false,
            message_chunk_limit: 4096,
            message_len_unit: LenUnit::Codepoints,
        }
    }

    /// WebUI/WebSocket channel 能力
    pub fn client() -> Self {
        Self {
            supports_streaming: true,
            ..Self::minimal()
        }
    }

    /// Telegram Bot API 能力
    pub fn telegram() -> Self {
        Self {
            supports_edit: true,
            supports_delete: true,
            supports_draft: true,
            supports_media: true,
            supports_buttons: true,
            supports_threads: true,
            message_chunk_limit: 4096,
            message_len_unit: LenUnit::Utf16Units,
            ..Self::minimal()
        }
    }

    /// QQ Bot 能力
    pub fn qqbot() -> Self {
        Self {
            supports_buttons: true,
            message_chunk_limit: 2000,
            ..Self::minimal()
        }
    }
}
```

**设计选择**：用预定义的 `client()` / `telegram()` / `qqbot()` 构造函数，避免各 channel 实现手写 10+ 字段。需要微调时链式调用：`ChannelCapabilities::telegram().with_message_chunk_limit(2048)`（Phase 2 再加 builder 方法，目前不做）。

### 6.2 MessagePayload

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

    /// 语音消息
    Voice {
        data: Vec<u8>,
        format: AudioFormat,
        caption: Option<String>,
    },

    /// 投票
    Poll {
        question: String,
        options: Vec<String>,
        anonymous: bool,
    },

    /// 富文本卡片（标题 + 正文 + 操作）
    Card {
        title: String,
        body: String,
        actions: Vec<CardAction>,
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

/// 音频格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Ogg,   // Telegram voice
    Mp3,
    Wav,
    M4a,
    Opus,
}

/// 卡片操作
#[derive(Debug, Clone)]
pub struct CardAction {
    pub label: String,
    pub action: String,  // callback_data / url / postback
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
            Self::Voice { caption, .. } => caption.clone().unwrap_or_default(),
            Self::Poll { question, options, .. } => {
                let opts = options.iter()
                    .enumerate()
                    .map(|(i, o)| format!("{}. {}", i + 1, o))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("📊 {}\n{}", question, opts)
            }
            Self::Card { title, body, actions } => {
                let act_text = actions.iter()
                    .map(|a| format!("[{}]", a.label))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("**{}**\n{}\n{}", title, body, act_text)
            }
        }
    }

    /// 提取文本内容（如果有的话）
    pub fn text_content(&self) -> Option<&str> {
        match self {
            Self::Text { content } => Some(content),
            Self::Interactive { content, .. } => Some(content),
            Self::Media { caption, .. } => caption.as_deref(),
            Self::Voice { caption, .. } => caption.as_deref(),
            Self::Poll { question, .. } => Some(question),
            Self::Card { body, .. } => Some(body),
        }
    }
}
```

### 6.3 SendTarget

```rust
/// 发送目标（从 SendMessage 中提取）
#[derive(Debug, Clone)]
pub struct SendTarget {
    /// 目标路由 key / chat_id
    pub recipient: String,
    /// 线程回复目标（平台消息 ID）
    pub reply_to: Option<String>,
    /// 线程 ID（Telegram topic / Slack thread）
    pub thread_id: Option<String>,
}

impl SendTarget {
    pub fn new(recipient: impl Into<String>) -> Self {
        Self {
            recipient: recipient.into(),
            reply_to: None,
            thread_id: None,
        }
    }
}
```

---

## 7. Channel trait 改动

### 7.1 新 trait 定义

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    // ── 元信息 ──
    fn name(&self) -> &str;

    /// Channel 能力声明。不可变。
    fn capabilities(&self) -> &ChannelCapabilities {
        // 使用 thread_local 避免非 const fn 的 static 初始化问题。
        thread_local! {
            static DEFAULT: ChannelCapabilities = ChannelCapabilities::minimal();
        }
        DEFAULT.with(|c| c)
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
        self.send(&msg).await?;
        Ok(SendResult::Delivered)
    }

    // ── 出站：兼容旧接口 ──
    /// 发送文本消息（向后兼容）。
    async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>;

    // ── 出站：编辑/删除 ──
    /// 编辑已发消息。不支持时返回 Err。
    async fn edit_message(
        &self,
        target: &SendTarget,
        message_id: &str,
        payload: &MessagePayload,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("edit_message not supported by {}", self.name()))
    }

    /// 删除消息。不支持时返回 Err。
    async fn delete_message(
        &self,
        target: &SendTarget,
        message_id: &str,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("delete_message not supported by {}", self.name()))
    }

    // ── 出站：Draft streaming ──
    /// 发送/更新原生流式预览（如 Telegram sendMessageDraft）。
    /// 返回 None 表示不支持（降级到 edit 路径）。
    async fn send_draft(
        &self,
        target: &SendTarget,
        draft_id: u64,
        content: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(None) // None = 不支持
    }

    // ── 流式事件 ──
    /// 是否支持 streaming turn events。
    /// 保留向后兼容：默认从 capabilities() 读取。
    fn supports_streaming(&self) -> bool {
        self.capabilities().supports_streaming
    }

    /// 推送 per-turn 流式事件（chunk/thinking/tool_call/done）。
    async fn push_event(&self, _reply_target: &str, _event: TurnEvent) {}

    /// 获取当前 turn 的取消令牌。
    fn cancel_signal(&self, _reply_target: &str) -> Option<CancellationToken> {
        None
    }

    // ── 格式化 ──
    /// 将通用 markdown 文本转为平台格式（Telegram MarkdownV2 / QQBot markdown 子集等）。
    fn format_message(&self, content: &str) -> String {
        content.to_string()
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

/// send_payload 的返回结果
#[derive(Debug, Clone)]
pub enum SendResult {
    /// 消息已送达
    Delivered,
    /// 消息已送达，携带平台消息 ID（用于后续 edit/delete）
    WithId { message_id: String },
}
```

### 7.2 向后兼容

| 现有方法 | 变化 |
|----------|------|
| `name()` | 不变 |
| `send()` | 不变，所有现有调用继续工作 |
| `listen()` | 不变 |
| `health_check()` | 不变 |
| `supports_streaming()` | 不变，默认实现改为从 `capabilities()` 读取 |
| `push_event()` | 不变，默认 no-op |
| `cancel_signal()` | 不变，默认 None |
| `on_status()` | 不变，默认 no-op |
| **新增** `capabilities()` | 默认返回 `ChannelCapabilities::minimal()` |
| **新增** `send_payload()` | 默认降级到 `send()` |
| **新增** `edit_message()` | 默认返回 Err |
| **新增** `delete_message()` | 默认返回 Err |
| **新增** `send_draft()` | 默认返回 Ok(None) |
| **新增** `format_message()` | 默认原样返回 |
| **新增** `message_len()` | 默认按 `capabilities().message_len_unit` 计算 |

所有新增方法都有默认实现，现有 channel 实现无需改动即可编译。

---

## 8. Orchestrator 改动

### 8.1 防重复发送

```rust
// orchestrator.rs handle_channel_event — user message dispatch
tokio::spawn(async move {
    let result = session_ctx
        .process_turn(inbound_msg, Some(channel.clone()), runtime)
        .await;
    match result {
        Ok(turn_result) => {
            if !turn_result.text.trim().is_empty()
                && !channel.supports_streaming()
                // ↑ streaming channel 已通过 TurnEvent::Chunk + Done 交付，
                //   不需要再 channel.send()
            {
                let send_msg = SendMessage::new(turn_result.text, reply_target);
                if let Err(e) = channel.send(&send_msg).await {
                    tracing::error!(err = %e, "send response failed");
                }
            }
        }
        Err(_) => {
            // 错误消息始终发送（streaming 不保证错误事件已推送）
            let send_msg = SendMessage::new(MSG_TURN_FAILED, reply_target);
            let _ = channel.send(&send_msg).await;
        }
    }
});
```

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
| `send_draft` | 新增，调用 Telegram `sendMessageDraft` API |
| `format_message` | 新增，包装现有 `markdown_to_telegram_html()` |
| `message_len` | 新增，`encode_utf16().count()`，**修复现有 `chars().count()` bug** |
| `chunk_for_telegram` | 改用 `self.message_len()` 替代硬编码 `chars().count()` |

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
| `format_message` | 新增，QQBot markdown 子集清洗（去除不支持的语法） |

**现有安全策略**：
- `allow_from: Option<Vec<String>>`（C2C 白名单）+ `is_user_allowed(openid)`
- `group_allow_from: Option<Vec<String>>`（群组白名单）+ `is_group_allowed(group_openid)`
- 按钮点击通过 `INTERACTION_CREATE` 事件接收，`button_data` 转为文本消息

QQBot 的 `send()` 内部自行调用 `split_message_chunk(&msg.content, QQ_MAX_MESSAGE_LENGTH)` 处理分片。Phase 1 改造 `split_message_chunk` 时需同步更新此处。

### WeChatChannel

| 改动 | 内容 |
|------|------|
| `capabilities()` | 基本保持 `minimal()`（默认） |
| `format_message` | 新增，Markdown → 纯文本降级 |

**现有安全策略**：`allowed_users` 白名单检查。

---

## 11. 实施阶段

### Phase 0：修复 WebUI 双显示 bug（P0）

**范围**：仅改 orchestrator.rs 1 行

```rust
// orchestrator.rs:722 加条件判断
if !channel.supports_streaming() { channel.send(...) }
```

**风险**：极低。`supports_streaming()` 已有，ClientChannel 返回 `true`，其他 channel 返回 `false`。

### Phase 1：引入 ChannelCapabilities + message_len_unit（P1）

**范围**：
- 新增 `ChannelCapabilities` / `LenUnit` 类型（含 `client()` / `telegram()` / `qqbot()` 预定义构造函数）
- Channel trait 新增 `capabilities()` / `format_message()` / `message_len()` 默认方法
- ClientChannel 覆盖 `capabilities()` 返回 `ChannelCapabilities::client()`
- TelegramChannel 覆盖 `capabilities()` 返回 `ChannelCapabilities::telegram()`
- QQBotChannel 覆盖 `capabilities()` 返回 `ChannelCapabilities::qqbot()`
- 改造 `split_message_chunk()` 接受 `LenUnit` 参数（**同时修复 Telegram UTF-16 计量 bug**）
- TelegramChannel 的 `chunk_for_telegram` 改用 `self.message_len()` 替代 `chars().count()`
- QQBotChannel 的 `send()` 内部 `split_message_chunk` 调用改用 `self.capabilities().message_chunk_limit` + `self.capabilities().message_len_unit`

**风险**：低。所有新方法有默认实现。`split_message_chunk` 签名变化会影响所有调用方（Telegram + QQBot + 测试），但改动是机械的。

### Phase 2：引入 MessagePayload + send_payload（P1）

**范围**：
- 新增 `MessagePayload` / `MediaSource` / `AudioFormat` / `CardAction` / `SendTarget` / `SendResult` 类型
- Channel trait 新增 `send_payload()` 默认方法（降级到 `send`）
- 重构 `SendMessage` 的 `inline_buttons` 走 `send_payload(Interactive{...})`
- 各 channel 实现覆盖 `send_payload()` 支持各自的 payload 类型

**风险**：中。需要逐一改造 channel 实现，但默认降级保证不影响现有功能。

### Phase 3：edit_message / delete_message / send_draft（P1）

**范围**：
- Channel trait 新增 `edit_message()` / `delete_message()` / `send_draft()` 默认方法
- TelegramChannel：**将已有私有 `edit_message(chat_id, msg_id, text)` 提升为 trait 实现**（接受 `SendTarget + MessagePayload`，内部复用现有 `editMessageText` API 调用）
- TelegramChannel：**将已有私有 `delete_message(chat_id, msg_id)` 提升为 trait 实现**
- TelegramChannel：新增 `send_draft()` 调用 `sendMessageDraft` API
- QQBotChannel：调研并实现 `edit_message`（如 API 支持）

**风险**：低。默认实现返回 Err，不影响不支持的 channel。Telegram 的 edit/delete 已有成熟实现，只是签名包装。

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
- [ ] 错误消息仍然正常发送到 WebUI

### Phase 1

- [ ] 各 channel 的 `capabilities()` 返回正确值
- [ ] `message_len()` 对 UTF-16 计量准确（emoji 算 2 单位）
- [ ] `split_message_chunk` 按 UTF-16 切分 Telegram 消息不超限
- [ ] `format_message()` 对 Telegram MarkdownV2 正确转义

### Phase 2

- [ ] `send_payload(Text)` 等价于现有 `send()`
- [ ] `send_payload(Interactive)` 在 Telegram 渲染 inline_keyboard
- [ ] `send_payload(Poll)` 在 Telegram 发起投票
- [ ] `send_payload(Voice)` 在 Telegram 发送语音
- [ ] 不支持 payload 类型的 channel 降级到 `to_fallback_text()`

### Phase 3

- [ ] Telegram `edit_message` 更新已发消息内容
- [ ] Telegram `delete_message` 删除指定消息
- [ ] Telegram `send_draft` 原生流式预览（降级到 edit 当不支持时）
- [ ] QQBot `edit_message` 正常工作（或返回 Err）
- [ ] WebUI 不受影响（不调用 edit/delete/draft）

### Phase 4

- [ ] Telegram 群组只响应 @提及（配置启用时）
- [ ] 白名单过滤正确生效
- [ ] DM 策略按配置执行

### Phase 5

- [ ] 按钮点击产生结构化 callback（非 `__retry:` 前缀）
- [ ] orchestrator 正确路由 callback 到目标处理逻辑
- [ ] 现有 retry/abort 按钮平滑迁移

---

## 附录 A：SendMessage 保留 vs 淘汰

`send(&SendMessage)` 在 Phase 2 之后标记为 `#[deprecated]`，建议新代码使用 `send_payload()`。但不删除——太多内部和测试代码依赖它（orchestrator 中 9 处调用 + 测试代码）。过渡期两者共存，`send_payload` 的默认实现内部调用 `send()`。

## 附录 B：与 RFC Session 架构的关系

本 RFC 不依赖 Session 架构重构（`rfc-session-architecture.md`），也不被其依赖。两者独立实施。唯一交集点是 `SessionContext.process_turn()` 中的 `channel.send()` 调用——Phase 0 的防重复修复在 `orchestrator.rs` 的 spawn block 中，不影响 `process_turn()` 本身。

## 附录 C：已知风险和待决事项

### C.1 Telegram `sendMessageDraft` API 稳定性

Telegram 的 `sendMessageDraft` API 尚未正式发布（截至 2026-05 仍在 beta），`send_draft` 的实现可能需要随 API 变动调整。建议在 Phase 3 实现时确认 API 状态，必要时改为纯 edit-based streaming 方案（发送一条 → 持续 edit 更新）。

### C.2 `split_message_chunk` 签名变更影响面

当前 `split_message_chunk(message: &str, limit: usize)` 被 3 处调用：
1. `TelegramChannel::chunk_for_telegram`（通过 `raw_limit` 参数）
2. `QQBotChannel::send`（直接调用 `split_message_chunk(&msg.content, QQ_MAX_MESSAGE_LENGTH)`）
3. `QQBotChannel` 的 bot command 回复

改为 `split_message_chunk(message: &str, limit: usize, unit: LenUnit)` 后，所有调用方都需要加 `LenUnit` 参数。可以考虑提供兼容的 `split_message_chunk_chars(message, limit)` 包装函数来减少改动面。

### C.3 `capabilities()` 的 `thread_local` 性能

默认实现使用 `thread_local!` 返回 `&ChannelCapabilities`，每次调用有一次 TLS 访问开销。对于不覆盖 `capabilities()` 的 channel（如 WeChatChannel），可以考虑让它们直接覆盖返回 `&'static ChannelCapabilities`。但这属于微优化，不阻塞实施。
