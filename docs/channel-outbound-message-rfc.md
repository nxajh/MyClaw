# RFC: Channel Outbound 文件发送与统一 Message 接口

> 状态：草案  
> 日期：2026-06-12  
> 范围：Channel ↔ Session 消息边界、outbound 文件发送、`send_media` 升级为通用 `send_message` 工具

---

## 1. 背景

MyClaw 已经完成 inbound/provider 侧的 path-only 多模态改造：

- 核心 provider content model 使用 `ContentPart::File { path, mime_type, name, size_bytes }`；
- 图片、音频、视频、普通文件在 session 内保存为本地文件；
- provider renderer 在最后一公里读取文件并编码为 provider wire format；
- 核心消息、history、session 不再保存 base64/bytes/URL 多模态变体。

但 outbound 文件发送仍停留在旧结构：

- `send_media(path, caption)` 工具读取本地文件；
- 工具层通过 `std::fs::read(path)` 一次性读入 `Vec<u8>`；
- 构造 `MessagePayload::Media { source: MediaSource::Inline { data, ... } }`；
- channel adapter 通过 `send_payload(&SendTarget, &MessagePayload)` 发送；
- Telegram / QQBot adapter 各自消费 inline bytes；
- `ChannelMessage` inbound 仍有 legacy 多字段：`files`、`attachments`、`image_urls`、`image_base64`；
- outbound 仍有 `SendMessage`、`SendTarget`、`MessagePayload`、`MediaSource` 多套结构。

这与当前文件化多模态原则不一致：

- channel/session 边界不应传本地 path；
- channel/session 边界不应传 `Vec<u8>` 或 base64；
- 工具可接收 path，但 path 只能在 agent/tool/session 内部解析；
- channel adapter 只能看到统一的文件 metadata 与可消费 body；
- bytes/base64/multipart 只能出现在平台 adapter 最后一公里。

---

## 2. 当前代码现状

### 2.1 `src/channels/message.rs`

当前核心 channel 类型包括：

```rust
pub struct SendTarget {
    pub recipient: String,
    pub thread_id: Option<String>,
    pub cancellation_token: Option<CancellationToken>,
}
```

问题：

- `target` 语义过宽；
- `recipient`、`thread_id`、`cancellation_token` 混在一个结构中；
- QQBot 当前把 inbound message id 塞进 `thread_id` 用于被动回复，语义不准确；
- `cancellation_token` 不是 receiver/routing 信息，而是发送操作控制信息。

当前 outbound payload：

```rust
pub enum MediaSource {
    Url(String),
    Inline {
        data: Vec<u8>,
        mime_type: Option<String>,
        file_name: Option<String>,
    },
}

pub enum MessagePayload {
    Text { text: String },
    Interactive { text: String, buttons: Vec<InlineButton> },
    Media { source: MediaSource, caption: Option<String> },
}
```

问题：

- `MediaSource::Inline` 在 channel 边界传 `Vec<u8>`；
- `MediaSource::Url` 把 URL 当文件源传给 channel，不符合“URL 由工具下载或作为文本处理”的方向；
- 文本、按钮、文件被拆成 enum variant，难以表达“文本 + 多文件 + 按钮”的自然消息结构；
- `MessagePayload::Media` 只能表达单文件 + caption，不适合作为最终 outbound message model。

当前 inbound message：

```rust
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub timestamp: u64,
    pub thread_ts: Option<String>,
    pub interruption_scope_id: Option<String>,
    pub files: Vec<FileAttachment>,
    pub attachments: Vec<MediaAttachment>,
    pub image_urls: Option<Vec<String>>,
    pub image_base64: Option<Vec<String>>,
}
```

问题：

- `content` 与文件字段分散；
- `files` 是 path-like session-local attachment；
- `attachments` 是 inline bytes；
- `image_urls` / `image_base64` 是旧图片专用字段；
- inbound 与 outbound 结构无法复用。

当前 channel trait：

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, msg: &SendMessage) -> anyhow::Result<()>;
    async fn send_payload(
        &self,
        target: &SendTarget,
        payload: &MessagePayload,
    ) -> anyhow::Result<SendResult>;
    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>>;
    // ...
}
```

问题：

- `send` 和 `send_payload` 两套 outbound API 并存；
- `send_payload` 是后加结构化发送接口，但仍依赖 `SendTarget` / `MessagePayload`；
- 最终应该收敛为一个明确的 outbound message 接口。

### 2.2 旧 `src/tools/send_media.rs`（已移除）

旧工具 schema：

```json
{
  "path": "string",
  "caption": "string?"
}
```

旧核心流程：

```rust
let data = std::fs::read(path)?;
let mime_type = infer_mime(&file_name, &data);
let payload = MessagePayload::Media {
    source: MediaSource::Inline {
        data,
        mime_type: Some(mime_type),
        file_name: Some(file_name),
    },
    caption,
};
channel.send_payload(&target, &payload).await?;
```

问题：

- 工具层一次性读完整文件；
- MIME 推断依赖完整 `data`；
- 文件 bytes 进入 channel 边界；
- 只能发送单文件；
- 工具名是 `send_media`，但实际需求已经扩展为通用 outbound message。

### 2.3 Telegram / QQBot

当前 Telegram media outbound：

- override `send_payload`；
- `MediaSource::Inline` clone bytes；
- `MediaSource::Url` 先下载；
- image → `sendPhoto`；
- 非 image → `sendDocument`；
- multipart 通过 `Part::bytes(data)`。

当前 QQBot media outbound：

- override `send_payload`；
- `MediaSource::Inline` clone bytes；
- `MediaSource::Url` 下载；
- 根据 MIME/扩展名推断 `file_type`；
- 最后一公里 base64 `file_data`；
- two-step API upload + send media message；
- 当前 `ChannelCapabilities::qqbot()` 已声明 `supports_file_send: true` / `supports_file_receive: true`，与现有收发实现对齐。

---

## 3. 目标

1. 将 outbound 文件发送升级为统一 `send_message` 工具。
2. 将 channel outbound API 收敛为 `send_message(ChannelOutboundMessage)`。
3. 将 `target` 命名改为更准确的 `receiver`。
4. 将 routing 信息与 send operation options 分离。
5. channel/session 边界不传本地 path、不传 `Vec<u8>`、不传 base64。
6. 文件传输统一使用 stream/body abstraction，不按大小切换 inline/path/stream 多种形态。
7. inbound/outbound 尽可能复用同一套 content/file 结构。
8. 平台 API 要求的 bytes/base64/multipart 只在 channel adapter 最后一公里出现。
9. `send_message` 只在 active channel、active receiver、channel 支持文件/按钮等所需能力时暴露。
10. 历史中 absent/orphan 的旧 `send_media` / 新 `send_message` tool call 要 fold，避免 provider 拒绝。

---

## 4. 非目标

- 不在本次实现 provider file upload cache。
- 不实现 OpenAI Responses API。
- 不实现远端 URL 作为 channel 文件源；URL 应由工具下载为 session-local file，或作为普通文本。
- 不在 channel 边界引入 `MediaSource::Path`。
- 不按文件大小设计 inline/path/stream 多套传输形态。
- 不强行让 inbound/outbound 共用同一个 envelope；只复用 content/file 子结构。

---

## 5. 核心设计

### 5.1 命名：`target` 改为 `receiver`

`target` 太泛，且当前 `SendTarget` 已经混入 cancellation token。最终命名应改为：

```rust
pub struct MessageReceiver {
    pub id: String,
    pub thread_id: Option<String>,
    pub reply_to_message_id: Option<String>,
}
```

语义：

- `id`：收件人、chat、user、group、room 等平台接收端 id；
- `thread_id`：平台 thread/topic；
- `reply_to_message_id`：引用回复或被动回复所需的 inbound message id。

QQBot 当前把 inbound message id 放进 `SendTarget.thread_id`，最终应改为：

```rust
MessageReceiver {
    id: reply_target,
    thread_id: None,
    reply_to_message_id: Some(last_msg.id.clone()),
}
```

`cancellation_token` 不属于 receiver，应移入 send options：

```rust
pub struct SendOptions {
    pub cancellation_token: Option<CancellationToken>,
}
```

### 5.2 统一 content：`ChannelMessageContent`

inbound/outbound 共享内容结构：

```rust
pub struct ChannelMessageContent {
    pub text: String,
    pub files: Vec<ChannelFile>,
    pub buttons: Vec<InlineButton>,
}
```

规则：

- 普通文本只填 `text`；
- 文件消息填 `files`，可同时带 `text` 作为 caption/说明；
- 多文件天然支持；
- 按钮作为 content 的一部分，而不是 `SendMessage.inline_buttons` 特殊字段；
- channel adapter 根据自身能力决定 native send、拆分发送或报错。

### 5.3 统一文件抽象：`ChannelFile`

channel/session 边界统一传：

```rust
pub struct ChannelFileMeta {
    pub file_name: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
}

#[async_trait]
pub trait ChannelFileBody: Send + Sync {
    async fn open(&self) -> anyhow::Result<Pin<Box<dyn AsyncRead + Send>>>;
}

pub struct ChannelFile {
    pub meta: ChannelFileMeta,
    pub body: Arc<dyn ChannelFileBody>,
}
```

约束：

- `ChannelFile` 不暴露 path；
- `ChannelFile` 不持有 `Vec<u8>`；
- `ChannelFileBody::open()` 每次返回一个新的 readable stream；
- adapter 若需要重试或多步 upload，可重新 open；
- local path 只存在于 `LocalFileBody` 私有字段中。

本地文件 body：

```rust
pub struct LocalFileBody {
    path: PathBuf,
}

#[async_trait]
impl ChannelFileBody for LocalFileBody {
    async fn open(&self) -> anyhow::Result<Pin<Box<dyn AsyncRead + Send>>> {
        let file = tokio::fs::File::open(&self.path).await?;
        Ok(Box::pin(file))
    }
}
```

`LocalFileBody.path` 不应为 public 字段，channel adapter 不应读取 path。

### 5.4 Inbound / outbound envelope 分离

不强行共用同一个 message envelope。推荐：

```rust
pub struct ChannelInboundMessage {
    pub id: String,
    pub sender: MessageSender,
    pub receiver: MessageReceiver,
    pub content: ChannelMessageContent,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
}

pub struct ChannelOutboundMessage {
    pub receiver: MessageReceiver,
    pub content: ChannelMessageContent,
    pub options: SendOptions,
}
```

原因：

- inbound 有 `id`、`sender`、`timestamp`；
- outbound 有 `options`；
- 二者 routing 方向不同；
- 共享 content/file 即可满足复用目标。

### 5.5 Channel trait 收敛

最终 trait：

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    async fn send_message(
        &self,
        message: ChannelOutboundMessage,
    ) -> anyhow::Result<SendResult>;

    async fn edit_message(
        &self,
        receiver: &MessageReceiver,
        message_id: &MessageId,
        content: ChannelMessageContent,
    ) -> anyhow::Result<()>;

    async fn delete_message(
        &self,
        receiver: &MessageReceiver,
        message_id: &MessageId,
    ) -> anyhow::Result<()>;

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelInboundMessage>>;
    async fn health_check(&self) -> bool;
    fn capabilities(&self) -> &ChannelCapabilities;
    fn create_stream(&self, reply_target: &str) -> Option<Box<dyn TurnStream>>;
    fn security_policy(&self) -> ChannelSecurityPolicy;
    fn check_authorization(&self, sender: &str, scope: MessageScope<'_>) -> AuthDecision;
}
```

迁移完成后删除：

- `SendMessage`；
- `SendTarget`；
- `MessagePayload`；
- `MediaSource`；
- `Channel::send`；
- `Channel::send_payload`。

---

## 6. `send_media` 升级为 `send_message` 工具

### 6.1 工具命名

最终不再向模型暴露 `send_media`，改为：

```text
send_message
```

原因：

- outbound 已不只是 media；
- 需要支持文本 + 文件 + 多文件 + 按钮；
- 与 channel trait `send_message` 对齐；
- 避免未来继续增加 `send_image` / `send_audio` / `send_file` 等碎片化工具。

### 6.2 第一版 schema

建议第一版保持克制：

```json
{
  "type": "object",
  "properties": {
    "text": {
      "type": "string",
      "description": "要发送给用户的文本。可单独发送，也可作为文件说明。"
    },
    "files": {
      "type": "array",
      "description": "要发送的本地文件列表。仅支持本地路径，不支持 URL。",
      "items": {
        "type": "object",
        "properties": {
          "path": {
            "type": "string",
            "description": "本地文件路径。"
          },
          "name": {
            "type": "string",
            "description": "可选，发送时显示的文件名。"
          },
          "mime_type": {
            "type": "string",
            "description": "可选，文件 MIME 类型；未提供时由工具推断。"
          }
        },
        "required": ["path"]
      }
    }
  },
  "anyOf": [
    { "required": ["text"] },
    { "required": ["files"] }
  ]
}
```

按钮可作为后续扩展：

```json
{
  "buttons": [
    { "label": "重试", "callback_data": "__retry:xxx" }
  ]
}
```

### 6.3 工具执行流程

`send_message` 工具内部流程：

```text
parse args
  ↓
require active session.channel
  ↓
require session.reply_target()
  ↓
check channel capabilities against requested content
  ↓
for each file path:
    resolve path
    validate exists + is_file
    metadata len
    infer mime from extension / bounded magic bytes
    create ChannelFileMeta
    create LocalFileBody { private path }
  ↓
construct ChannelOutboundMessage {
    receiver: MessageReceiver {
        id: reply_target,
        thread_id: last_msg.thread_ts or platform thread,
        reply_to_message_id: last_msg.id,
    },
    content: ChannelMessageContent { text, files, buttons },
    options: SendOptions { cancellation_token: None },
}
  ↓
channel.send_message(message)
```

关键点：

- 工具 schema 允许 agent 传 path；
- 工具必须 resolve/validate/infer；
- path 不进入 channel API；
- 工具不再 `std::fs::read(path)` 读完整文件；
- MIME magic bytes 只读取文件头部，不读取整个文件；
- file size limit 是工具/平台 capability 决策，不改变传输形态。

### 6.4 返回值

成功：

```text
已发送消息。
```

若包含文件：

```text
已发送消息，包含 2 个文件：report.pdf, chart.png。
```

失败：

- 无 active channel：`send_message requires an active channel`；
- 无 receiver：`send_message requires an active receiver`；
- channel 不支持文件：`current channel does not support file sending`；
- 文件不存在：`文件不存在: path`；
- 文件过大：`文件过大: ...`；
- 发送失败：`发送失败: ...`。

---

## 7. Channel adapter 发送策略

### 7.1 通用 fallback

默认 `send_message` 实现可只支持文本 fallback：

- `content.files.is_empty()` 且 `buttons.is_empty()`：调用平台文本发送；
- 有 buttons 但 channel 不支持：发送文本并追加按钮 label marker，或返回 unsupported；
- 有 files 但 channel 不支持：返回 unsupported，不应把 path 暴露成文本。

文件 fallback 不应显示本地 path。若需要降级，只能显示 file name：

```text
[文件: report.pdf]
```

### 7.2 Telegram

Telegram adapter 消费 `ChannelFileBody`：

- `image/*` → `sendPhoto`；
- `audio/*` → `sendAudio`；
- `video/*` → `sendVideo`；
- other → `sendDocument`。

实现建议：

- 通过 `body.open().await?` 获取 `AsyncRead`；
- 优先使用 reqwest multipart streaming body；
- 不再 clone `Vec<u8>`；
- caption 使用 `content.text`；
- 多文件第一版可逐个发送，第一条携带 text/caption，后续只发文件；
- `receiver.thread_id` 映射 Telegram topic/thread；
- `receiver.reply_to_message_id` 映射 Telegram reply-to。

### 7.3 QQBot

QQBot adapter 消费 `ChannelFileBody`：

- 根据 `mime_type` / file name 推断 file_type：
  - image → 1；
  - video → 2；
  - audio/voice → 3；
  - other → 4。
- 如果 QQBot API 要 base64，则在 adapter 最后一公里读取 stream 并编码；
- base64 不进入 channel common types；
- upload `/v2/users/{openid}/files` 或 `/v2/groups/{group_openid}/files`；
- send `msg_type=7 + media.file_info`；
- `receiver.reply_to_message_id` 用作被动回复 `msg_id`。

同时修正 capability：

```rust
pub const fn qqbot() -> Self {
    Self {
        supports_file_send: true,
        supports_file_receive: true,
        // ...
    }
}
```

如果某些 QQBot 场景只支持部分文件类型，应扩展 capability，而不是错误声明整体 false。

### 7.4 WeChat

当前 WeChat 只支持文本。最终二选一：

1. 不实现文件发送：
   - `supports_file_send: false`；
   - `supports_file_receive: false`；
   - agent 不暴露带文件的 `send_message` 能力；
   - adapter 对 files 返回 unsupported。
2. 实现文件上传：
   - 同样消费 `ChannelFileBody`；
   - 不接收 path/bytes/base64 common type。

### 7.5 ClientChannel / WebUI / TUI

当前 `ChannelCapabilities::client()` 声明 `supports_file_send: false` / `supports_file_receive: true`，表示 WebUI/TUI 可以接收用户上传文件，但当前 outbound 文件发送尚未实现。

最终要求：

- 如果 WebUI/TUI 能接收 outbound 文件 stream/body，应实现 `send_message` 并将 `supports_file_send` 改为 true；
- 如果不能，应保持 `supports_file_send` 为 false；
- 不允许 capability 声明支持但 adapter fallback 成无效文本。

---

## 8. Inbound 改造

### 8.1 新 inbound 结构

```rust
pub struct MessageSender {
    pub id: String,
    pub display_name: Option<String>,
}

pub struct ChannelInboundMessage {
    pub id: String,
    pub sender: MessageSender,
    pub receiver: MessageReceiver,
    pub content: ChannelMessageContent,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
}
```

### 8.2 文件处理原则

channel adapter 收到平台文件后：

1. adapter 可从平台下载文件；
2. adapter 不把 bytes/base64 放入 `ChannelInboundMessage`；
3. adapter 通过统一 `ChannelFile { meta, body }` 交给 session；
4. session 再将 inbound file 持久化为 session-local file；
5. agent/provider history 使用 `ContentPart::File { path, ... }`。

如果 adapter 无法保持远端 body stream 重开能力，可以在 adapter 内部使用临时文件 body，但仍不暴露 path 给 session/channel common API。

### 8.3 删除 legacy 字段

最终删除：

- `ChannelMessage.files: Vec<FileAttachment>`；
- `ChannelMessage.attachments: Vec<MediaAttachment>`；
- `ChannelMessage.image_urls`；
- `ChannelMessage.image_base64`；
- `FileAttachment`；
- `MediaAttachment`。

legacy `image_urls` 不再是 channel 多模态输入；URL 作为普通文本，由模型通过工具链下载/查看。

---

## 9. 文件类型与 MIME 推断共享模块

旧 `send_media.rs` 内部曾有 `infer_mime(file_name, data)`，provider/media/channel 也各有类似判断。建议抽到共享模块：

```rust
pub enum FileKind {
    Image,
    Audio,
    Video,
    Other,
}

pub fn infer_mime_from_name(file_name: &str) -> Option<String>;
pub async fn infer_mime_from_file_head(path: &Path) -> anyhow::Result<Option<String>>;
pub fn kind_from_mime(mime_type: Option<&str>) -> FileKind;
pub fn marker_for_file_kind(kind: FileKind, display: &str) -> String;
```

要求：

- magic bytes 最多读取固定头部，例如 512 或 4096 bytes；
- 不为了 MIME 推断读取整个文件；
- provider fallback marker、view tools、send_message、Telegram/QQBot routing 复用同一套逻辑。

---

## 10. Agent tool visibility

### 10.1 `send_message` 动态暴露

`send_message` 不应无条件暴露。满足以下条件时才暴露：

- 当前 session 有 active channel；
- 当前 session 有 active receiver/reply target；
- 对于纯文本 send，channel 可发送文本；
- 对于文件 send，channel capability 支持 file/media；
- 对于按钮 send，channel capability 支持 inline buttons。

如果工具 schema 无法根据本轮 capability 动态裁剪，可采用两层策略：

- 没有 active channel/reply_target：完全不 advertise `send_message`；
- 有 active channel：advertise `send_message`，执行时再根据具体 args 检查 files/buttons capability。

### 10.2 absent/orphan tool call folding

历史中可能存在：

- 旧 `send_media` tool call；
- 新 `send_message` tool call；
- 当前回合由于 channel 不支持而未 advertise 的 tool call。

Agent 构造 provider messages 时应 fold 这些 absent/orphan tool calls，类似当前 `view_image` / `hear_audio` / `view_video` 的处理，避免 provider 报错。

建议 fold 文本：

```text
[历史工具调用 send_media 已省略：当前通道不支持或工具已升级为 send_message]
```

或：

```text
[历史工具调用 send_message 已省略：当前没有可发送的 active channel]
```

---

## 11. 与普通 assistant final answer 的关系

`send_message` 工具不是普通文本回复的唯一出口。

规则：

1. 普通文本回答仍由 agent final answer 交给 `SessionContext::process_turn` fallback send/stream path。
2. 需要主动发送文件、多文件、按钮、或混合内容时，模型调用 `send_message`。
3. `send_message` 发送成功后，模型 final answer 应简短确认，例如“已发送。”。
4. 后续可增加 tool result 标记，避免 final answer 重复发送同一段文本。

---

## 12. 迁移步骤

本 RFC 目标是最终结构，不设计长期双轨。但代码实施可以按 PR 顺序落地，最终不保留旧接口。

### Step 1：新增 common types

在 `src/channels/message.rs` 或拆分 `src/channels/file.rs` 中新增：

- `MessageSender`；
- `MessageReceiver`；
- `SendOptions`；
- `ChannelMessageContent`；
- `ChannelFileMeta`；
- `ChannelFileBody`；
- `ChannelFile`；
- `LocalFileBody`；
- `ChannelInboundMessage`；
- `ChannelOutboundMessage`。

### Step 2：新增 `Channel::send_message`

新增最终接口，并将旧 `send` / `send_payload` 调用点迁移过去：

- `ask_user.rs`；
- 旧 `send_media.rs` / 新 `send_message.rs`；
- `orchestrator/inbound.rs` slash command / recovery / error send；
- `orchestrator/scheduled.rs`；
- `orchestrator/recovery.rs`；
- `session_context.rs` final answer fallback。

### Step 3：实现 `SendMessageTool`

用 `src/tools/send_message.rs` 替代旧 `send_media.rs`：

- schema 支持 `text` + `files`；
- path 只在工具内部解析；
- 创建 `LocalFileBody`；
- 构造 `ChannelOutboundMessage`；
- 调 `channel.send_message(message)`。

`daemon.rs` 注册改为：

```rust
tools.register(Arc::new(crate::tools::SendMessageTool::new()));
```

`SendMediaTool` 已停止注册并删除源码文件；历史中的 `send_media` 调用只通过 absent-tool fold 兼容。

### Step 4：改造 Telegram / QQBot / WeChat / ClientChannel

- Telegram 实现 stream/body 文件发送；
- QQBot 实现 adapter 最后一公里 base64；
- WeChat 明确 unsupported 或实现 native upload；
- ClientChannel capability 与实现对齐。

### Step 5：改造 inbound listen 返回类型

将：

```rust
listen() -> mpsc::Receiver<ChannelMessage>
```

改为：

```rust
listen() -> mpsc::Receiver<ChannelInboundMessage>
```

同时改造 `SessionContext` 的 inbound normalize：

- 从 `msg.content.text` 取文本；
- 从 `msg.content.files` 持久化 session-local file；
- 生成 provider `ContentPart::File`。

### Step 6：删除旧类型和旧字段

删除：

- `SendMessage`；
- `SendTarget`；
- `MessagePayload`；
- `MediaSource`；
- `FileAttachment`；
- `MediaAttachment`；
- `ChannelMessage.image_urls`；
- `ChannelMessage.image_base64`；
- `Channel::send`；
- `Channel::send_payload`。

### Step 7：Agent tool visibility / fold

- 动态 advertise `send_message`；
- fold absent `send_media`；
- fold absent `send_message`；
- tests 覆盖 unsupported channel 不暴露工具。

---

## 13. 测试计划

### 13.1 Unit tests

- `MessageReceiver` routing 字段构造；
- `LocalFileBody::open()` 可重复打开；
- MIME head inference 不读取整个文件；
- `kind_from_mime()` image/audio/video/other；
- `ChannelMessageContent` fallback text 不泄漏 path。

### 13.2 Tool tests

- `send_message` 纯文本成功；
- `send_message` 单文件成功且工具不读完整文件；
- `send_message` 多文件成功；
- 文件不存在失败；
- directory path 失败；
- 超过平台/工具限制失败；
- 无 active channel 失败；
- 无 reply_target 失败；
- channel 不支持 file 时带 files 失败；
- URL path 不作为文件发送。

### 13.3 Channel tests

Telegram：

- image → photo endpoint；
- audio → audio endpoint；
- video → video endpoint；
- other → document endpoint；
- caption 映射正确；
- thread/reply_to 映射正确。

QQBot：

- image/audio/video/file file_type 映射正确；
- base64 只在 adapter 内部生成；
- `reply_to_message_id` 映射被动回复 `msg_id`；
- capability `supports_file_send` / `supports_file_receive` 与实现一致。

WeChat / Client：

- unsupported files 返回明确错误；
- capability 与实际实现一致。

### 13.4 Agent/provider tests

- active channel 下 advertise `send_message`；
- scheduled/sub-agent 无 channel 时不 advertise；
- unsupported channel 下文件发送工具不暴露或执行时报错；
- 历史 `send_media` absent call 被 fold；
- 历史 `send_message` absent call 被 fold；
- final answer 文本仍走原有 stream/fallback 发送路径。

---

## 14. 实现级补充决策

### 14.1 `ChannelFileBody::open()` 精确定义

最终签名固定为：

```rust
#[async_trait]
pub trait ChannelFileBody: Send + Sync {
    async fn open(&self) -> anyhow::Result<Pin<Box<dyn tokio::io::AsyncRead + Send>>>;
}
```

说明：

- `ChannelFileBody` 自身必须 `Send + Sync`，因为它会被放入 `Arc<dyn ChannelFileBody>` 并跨 async task / channel adapter 传递；
- `open()` 返回的 reader 只要求 `AsyncRead + Send`；
- 不要求 reader `Sync`，因为单个 reader 通常只被一个发送流程顺序消费；
- `open()` 每次应返回一个新的 reader，而不是复用已经读过的 reader；
- adapter 需要重试、两步上传、multipart 构造时，可以重新调用 `open()`。

### 14.2 `ChannelFile` 必须可 clone

`ChannelFile` 使用 `Arc<dyn ChannelFileBody>`，保证 message/content 在运行时可以 clone：

```rust
#[derive(Clone)]
pub struct ChannelFile {
    pub meta: ChannelFileMeta,
    pub body: Arc<dyn ChannelFileBody>,
}
```

要求：

- clone `ChannelFile` 只 clone metadata 与 `Arc`；
- clone 不读取文件；
- clone 不复制 bytes；
- 多次发送或重试通过 `body.open()` 获取新的 stream。

### 14.3 Runtime inbound message 与 persisted last message 分离

新的 `ChannelInboundMessage` 包含 `ChannelFileBody` trait object，不能直接 `Serialize` / `Deserialize`，也不应该直接持久化。

因此需要拆分：

```rust
pub struct ChannelInboundMessage {
    pub id: String,
    pub sender: MessageSender,
    pub receiver: MessageReceiver,
    pub content: ChannelMessageContent,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
}
```

以及可持久化结构：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedChannelMessage {
    pub id: String,
    pub sender_id: String,
    pub receiver: MessageReceiver,
    pub text: String,
    pub timestamp: u64,
    pub interruption_scope_id: Option<String>,
}
```

原则：

- `ChannelInboundMessage` 是 runtime 边界对象，用于 channel → orchestrator/session；
- session 处理 inbound 后，文件已经落为 session-local file，并进入 `ContentPart::File { path, ... }`；
- `session.last_message` 只保存 `PersistedChannelMessage` 这类轻量 routing/context 信息；
- `session.last_message` 不保存 `ChannelFileBody`；
- `session.last_message` 不保存 bytes/base64；
- QQBot 被动回复、ask_user、recovery 等需要的 inbound id 从 `PersistedChannelMessage.id` 获取。

`MessageReceiver` 若需要持久化，应 derive serde：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageReceiver {
    pub id: String,
    pub thread_id: Option<String>,
    pub reply_to_message_id: Option<String>,
}
```

### 14.4 `SendResult` 支持多消息返回

因为 `ChannelMessageContent.files` 支持多文件，而部分平台需要把多文件拆成多条平台消息发送，旧的：

```rust
pub type SendResult = Option<MessageId>;
```

不再足够。最终改为：

```rust
#[derive(Debug, Clone, Default)]
pub struct SendResult {
    pub message_ids: Vec<MessageId>,
}
```

语义：

- `message_ids.is_empty()` 表示平台不返回 message id，或当前 adapter 是 fire-and-forget；
- 单条消息返回一个 id；
- 多文件顺序发送返回多个 id；
- `edit_message` / `delete_message` 仍针对单个 `MessageId` 操作；
- 调用方如果只关心是否成功，只看 `Result<SendResult>` 的 `Ok/Err`。

### 14.5 多文件发送语义

第一版统一语义：

```text
多文件由 channel adapter 顺序发送。
第一条文件消息携带 content.text 作为 caption/说明。
后续文件不重复 content.text。
SendResult.message_ids 按发送顺序返回。
```

说明：

- 不要求第一版实现 Telegram media group；
- 不要求 channel 一次 API 调用发送全部文件；
- adapter 可以在未来优化为平台原生 album/media group，但对上层语义不变；
- 如果某平台只支持单文件，adapter 可顺序发送；
- 如果某平台完全不支持文件，返回 unsupported。

### 14.6 `ChannelCapabilities` 拆分文件收发能力

`supports_media` 命名过宽，且无法区分 inbound/outbound。最终改为：

```rust
pub struct ChannelCapabilities {
    pub supports_streaming: bool,
    pub supports_edit: bool,
    pub supports_delete: bool,
    pub supports_inline_buttons: bool,
    pub supports_file_send: bool,
    pub supports_file_receive: bool,
    pub supports_threads: bool,
    pub message_chunk_limit: usize,
    pub message_len_unit: LenUnit,
}
```

规则：

- `supports_file_send` 控制 agent 是否可暴露带 files 的 `send_message` 能力；
- `supports_file_receive` 表示 channel inbound 是否能接收用户文件并传给 session；
- Telegram 通常二者都为 true；
- QQBot 若 outbound 文件已实现，应将 `supports_file_send` 设为 true；
- WeChat 若当前只支持文本，则二者为 false；
- ClientChannel / WebUI / TUI 必须按实际实现声明，不能声明支持但无有效实现。

---

## 15. 最终形态示例

### 14.1 工具调用

模型调用：

```json
{
  "text": "这是生成的报告和图表。",
  "files": [
    { "path": "sessions/s1/files/report.pdf" },
    { "path": "sessions/s1/files/chart.png" }
  ]
}
```

工具内部构造：

```rust
let message = ChannelOutboundMessage {
    receiver: MessageReceiver {
        id: reply_target,
        thread_id: last_msg.thread_ts.clone(),
        reply_to_message_id: Some(last_msg.id.clone()),
    },
    content: ChannelMessageContent {
        text: "这是生成的报告和图表。".to_string(),
        files: vec![report_file, chart_file],
        buttons: vec![],
    },
    options: SendOptions::default(),
};

channel.send_message(message).await?;
```

channel adapter 看到的是：

```rust
ChannelFile {
    meta: ChannelFileMeta {
        file_name: "report.pdf".to_string(),
        mime_type: Some("application/pdf".to_string()),
        size_bytes: Some(123456),
    },
    body: Arc<dyn ChannelFileBody>,
}
```

channel adapter 看不到：

- `sessions/s1/files/report.pdf` path string；
- `Vec<u8>`；
- base64。

### 14.2 普通文本发送

`SessionContext::process_turn` final answer fallback：

```rust
let message = ChannelOutboundMessage {
    receiver: MessageReceiver::new(reply_target),
    content: ChannelMessageContent::text(turn_result.text.clone()),
    options: SendOptions::default(),
};

channel.send_message(message).await?;
```

---

## 16. 结论

最终推荐方案：

- `target` 改名并收敛为 `MessageReceiver`；
- `cancellation_token` 移入 `SendOptions`；
- `send_media` 升级为通用 `send_message` 工具；
- channel outbound API 收敛为 `send_message(ChannelOutboundMessage)`；
- inbound/outbound envelope 分离，但共享 `ChannelMessageContent` / `ChannelFile`；
- channel/session 文件边界统一使用 `ChannelFile { meta, body }`；
- path 只存在于 agent/tool/session 内部；
- bytes/base64/multipart 只存在于平台 adapter 最后一公里；
- 不再保留 `MediaSource::Inline` / `MediaSource::Url` / legacy image fields；
- capability、tool visibility、历史 tool call folding 必须同步改造。
