# MyClaw Provider 系统重构 RFC

> **状态**: Phase 1-6 已完成（commit `43312af`），Phase 7（回归测试）待执行  
> **分支**: `refactor/provider-factory`  
> **日期**: 2026-05-15  
> **变更统计**: 16 files changed, +1161 -1288, 净减 127 行（代码净减 1072 行，新增类型/协议层）

---

## 目录

1. [问题陈述](#1-问题陈述)
2. [目标与非目标](#2-目标与非目标)
3. [代码现状](#3-代码现状)
4. [核心设计决策](#4-核心设计决策)
5. [新增类型](#5-新增类型)
6. [协议客户端层](#6-协议客户端层)
7. [ProviderFactory](#7-providerfactory)
8. [Message Rendering](#8-message-rendering)
9. [配置变更](#9-配置变更)
10. [Provider 文件 Slim Down](#10-provider-文件-slim-down)
11. [实施阶段](#11-实施阶段)
12. [实际变更清单](#12-实际变更清单)
13. [设计讨论中的修正](#13-设计讨论中的修正)
14. [未完成工作](#14-未完成工作)
15. [测试计划](#15-测试计划)

---

## 1. 问题陈述

MyClaw 的 Provider 系统存在以下核心问题：

### 1.1 URL 硬编码猜测 Provider/协议

当前通过 `base_url` 域名特征硬编码判断 Provider 类型和 API 协议：

```rust
// shared.rs 中的 from_url 逻辑
if url.contains("anthropic.com") { ProviderHandle::Anthropic(...) }
else { ProviderHandle::OpenAi(...) }  // 其他全部默认 OpenAI
```

**问题**：
- 第三方代理/中转站（如 `us.jinl.in`）无法被正确识别
- Xiaomi MiMo 使用 Anthropic 协议，但 host 不含 `anthropic.com`
- 用户无法显式指定 API 协议

### 1.2 大量重复代码

- `xiaomi.rs` 从 `anthropic.rs` 复制粘贴了约 300 行（SSE 解析、body 构建、消息渲染）
- `kimi.rs` 与 `openai.rs` 共享 SSE 解析逻辑但有独立 body 构建
- `minimax.rs` 委托 `AnthropicProvider`，本质是 Anthropic 协议的又一个变体

### 1.3 daemon.rs 注册逻辑过于复杂

`build_registry()` 手动匹配 7+ 种 Provider，每种构造不同的 `ProviderHandle`，约 140 行硬编码分发逻辑。

### 1.4 缺乏扩展性

添加新 Provider 需要修改 `shared.rs`（添加 enum variant）、`daemon.rs`（添加注册分支），耦合严重。

---

## 2. 目标与非目标

### 2.1 目标

- **显式配置**: 用户可以在配置中指定 `provider` 和 `protocol`，不再依赖 URL 猜测
- **协议复用**: 提取 `OpenAiChatCompletionsClient` 和 `AnthropicMessagesClient` 为标准协议客户端，供多个 Provider 复用
- **Provider 文件瘦身**: Xiaomi/Kimi/OpenAI/Anthropic 的 ChatProvider 实现委托到协议客户端
- **零破坏**: 所有现有配置继续工作，`base_url` 语义不变

### 2.2 非目标

- **不重写 GLM**: GLM 有 `do_sample`、`tool_stream`、`reasoning_content` top-level、自定义 SSE parser 等独特行为，差异太大，本次不动
- **不迁移 Embedding/Image/TTS/Search/Video/STT**: 这些能力继续通过 `ProviderHandle` 分发
- **不改 base_url 语义**: 配置中的 `base_url` 可以包含 `/v1`、`/api/paas/v4` 等路径前缀，保持现有行为
- **不新增 normalization 层**: message rendering 只搬迁现有逻辑，不新增删除/改写规则（如 thinking block 不默认删除）
- **不改 auth_style**: 现有 `auth_style` 字段名和行为保持不变

---

## 3. 代码现状

### 3.1 重构前文件清单

| 文件 | 行数 | 职责 |
|---|---|---|
| `config/provider.rs` | 239 | ProviderConfig（无 provider/protocol 字段） |
| `providers/shared.rs` | 427 | ProviderHandle enum、URL factory、AuthStyle、SSE parser、body builder |
| `providers/openai.rs` | 512 | Chat+Embedding+Image+TTS，自有 body+SSE |
| `providers/anthropic.rs` | 402 | Chat only，自有 body+SSE |
| `providers/glm.rs` | 522 | Chat+Embedding+Search，do_sample/tool_stream/reasoning_content |
| `providers/xiaomi.rs` | 477 | Chat only，Anthropic body+SSE 的复制粘贴 |
| `providers/kimi.rs` | 295 | Chat only，OpenAI-like + reasoning_content top-level |
| `providers/minimax.rs` | 146 | Chat 委托 AnthropicProvider + 独立 SearchProvider |
| `providers/google.rs` | 274 | Search only，Gemini generateContent API |
| `daemon.rs:193-336` | ~140 | build_registry 手动构造 ProviderHandle 注册 |
| `registry/mod.rs` | 563 | Registry struct，from_config |

**总计**: ~3,893 行 provider 相关代码

### 3.2 重复代码热点

| 重复区域 | 源文件 | 目标文件 | 重复行数 |
|---|---|---|---|
| Anthropic body 构建 | `anthropic.rs` | `xiaomi.rs` | ~180 行 |
| Anthropic SSE 解析 | `anthropic.rs` | `xiaomi.rs` | ~80 行 |
| Anthropic 消息渲染 | `anthropic.rs` | `xiaomi.rs` | ~100 行 |
| OpenAI SSE 解析 | `shared.rs` | `kimi.rs`（已复用） | — |
| OpenAI body 构建 | `openai.rs` | `kimi.rs` | ~60 行 |

---

## 4. 核心设计决策

### 4.1 ProviderId — newtype 而非 enum

```rust
pub struct ProviderId(String);
```

**为什么不用 enum**：
- Enum 是封闭类型，添加第三方 Provider 需要修改核心代码
- Newtype 允许任意字符串值，插件可以自定义
- 通过 `well_known` 模块提供常量：`OPENAI`、`ANTHROPIC`、`GLM` 等

**URL 推断**（`detect_from_url`）仅用于 fallback，不作为主要识别方式：

| URL host 特征 | 推断 ProviderId |
|---|---|
| `bigmodel.cn` / `zhipuai` | `glm` |
| `xiaomimimo` | `xiaomi` |
| `anthropic.com` / `claude.ai` | `anthropic` |
| `minimax` / `minimaxi` | `minimax` |
| `moonshot` / `kimi` | `kimi` |
| `googleapis.com` | `google` |
| `openai.com` / `deepseek` / `siliconflow` | `openai` |
| 其他 | `None`（需要显式配置） |

### 4.2 Protocol — 显式覆盖

```rust
pub enum Protocol {
    #[default]
    OpenAi,
    Anthropic,
}
```

**解析优先级**：
1. 显式配置 `protocol = "anthropic"` → 使用该值
2. Provider 默认值：`anthropic` / `xiaomi` / `minimax` → Anthropic
3. 其他 → OpenAI（默认值）

### 4.3 base_url 语义不变

经过讨论，最初提议的 "base_url 强制 origin-only" 被否决：

- GLM 需要 `https://open.bigmodel.cn/api/paas/v4`
- Xiaomi 需要 `https://api.xiaomimimo.com/anthropic/v1`
- 第三方代理可能带各种路径前缀
- 强制 validation 容易误伤

改为：保持 `base_url` 原样，endpoint path 拼接逻辑不变。

### 4.4 AuthStyle 类型桥接

`config::provider::AuthStyle`（有 serde）和 `providers::shared::AuthStyle`（无 serde）是两个独立类型，通过 `From` impl 桥接：

```rust
impl From<config::AuthStyle> for providers::AuthStyle {
    fn from(style: AuthStyle) -> Self {
        match style {
            AuthStyle::Bearer => providers::AuthStyle::Bearer,
            AuthStyle::XApiKey => providers::AuthStyle::XApiKey,
        }
    }
}
```

在 `daemon.rs` 中使用 `.into()` 转换。

### 4.5 ProviderHandle 保留为 fallback

本次重构不删除 `ProviderHandle`，而是作为 fallback 使用：

- **Chat**: ProviderFactory 按 `(provider_id, protocol)` 分派，未匹配的组合回退到 ProviderHandle
- **Embedding/Image/TTS/Search/Video/STT**: 完全委托 ProviderHandle

未来逐步将所有能力迁移到协议层后，可移除 ProviderHandle。

### 4.6 GLM 不动

GLM 有以下独特行为，与标准 OpenAI 差异太大：

- `do_sample` 参数（非标准）
- `tool_stream` 特殊处理
- `reasoning_content` 作为 top-level 字段（而非 content part）
- 自定义 SSE parser
- 搜索服务集成

本次保持 `glm.rs` 完全不变。

### 4.7 MiniMax 不动

MiniMax 已经通过 `AnthropicProvider` 委托，有独立的 `SearchProvider`。协议客户端提取后，MiniMax 的 ChatProvider 走 `ProviderFactory → AnthropicMessagesClient` 路径，但 `minimax.rs` 文件本身保持不变。

---

## 5. 新增类型

### 5.1 ProviderId（`src/providers/provider_id.rs`，106 行）

```rust
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Self;
    pub fn as_str(&self) -> &str;
}

// Well-known constants
pub mod well_known {
    pub const GENERIC: &str = "generic";
    pub const OPENAI: &str = "openai";
    pub const ANTHROPIC: &str = "anthropic";
    pub const GLM: &str = "glm";
    pub const XIAOMI: &str = "xiaomi";
    pub const KIMI: &str = "kimi";
    pub const MINIMAX: &str = "minimax";
    pub const GOOGLE: &str = "google";
}

// URL host detection (fallback only)
pub fn detect_from_url(base_url: &str) -> Option<ProviderId>;
```

### 5.2 Protocol（`src/config/provider.rs` 中）

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    OpenAi,
    Anthropic,
}
```

### 5.3 Build Request Structs（`src/providers/provider_factory.rs` 中）

```rust
pub struct BuildChatProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub protocol: Option<Protocol>,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
    pub extra_headers: HashMap<String, String>,
}

// 类似的 Build*ProviderRequest 用于 embedding/image/tts/search/video/stt
```

---

## 6. 协议客户端层

新增 `src/providers/protocols/` 目录，包含两个标准协议客户端：

### 6.1 目录结构

```
src/providers/protocols/
├── mod.rs                  # pub mod openai; pub mod anthropic;
├── openai/
│   ├── mod.rs              # pub mod chat_completions; pub mod chat_message_rendering;
│   ├── chat_completions.rs # OpenAiChatCompletionsClient (125 行)
│   └── chat_message_rendering.rs  # render_openai_chat_body() (101 行)
└── anthropic/
    ├── mod.rs              # pub mod messages; pub mod message_rendering;
    ├── messages.rs         # AnthropicMessagesClient (112 行)
    └── message_rendering.rs # render_anthropic_messages() + build_anthropic_body() (215 行)
```

### 6.2 OpenAiChatCompletionsClient（125 行）

```rust
pub struct OpenAiChatCompletionsClient {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl OpenAiChatCompletionsClient {
    pub fn new(api_key: String, base_url: String) -> Self;
    pub fn with_user_agent(self, user_agent: String) -> Self;
}
```

**特性**：
- 实现 `ChatProvider` trait
- Auth: Bearer token
- URL 自动补全：如果 `base_url` 含 `/v1` 或 `/v4`，直接拼接 `/chat/completions`；否则追加 `/v1/chat/completions`
- SSE 解析：复用 `shared::parse_openai_sse`
- `saw_tool_call` 跟踪：手动检测 `ToolCallStart`/`ToolCallDelta` 事件，用于最终 `StopReason` 判断
- UTF-8 缓冲：处理跨 chunk 的多字节字符

### 6.3 AnthropicMessagesClient（112 行）

```rust
pub struct AnthropicMessagesClient {
    base_url: String,
    api_key: String,
    client: Client,
    user_agent: Option<String>,
}

impl AnthropicMessagesClient {
    pub fn new(api_key: String, base_url: String) -> Self;
    pub fn with_user_agent(self, user_agent: String) -> Self;
}
```

**特性**：
- 实现 `ChatProvider` trait
- Auth: Bearer token（非 x-api-key，因为 Anthropic 代理通常用 Bearer）
- Header: `anthropic-version: 2023-06-01`
- URL: `{base_url}/v1/messages`
- SSE 解析：复用 `anthropic::parse_anthropic_sse`（含 tool_index_map）
- UTF-8 缓冲：与 OpenAI 相同

---

## 7. ProviderFactory

`src/providers/provider_factory.rs`（274 行）

### 7.1 职责

ProviderFactory 是构建所有 Provider trait 对象的唯一入口。替代 `daemon.rs` 中的手动 `ProviderHandle` 构造。

### 7.2 Chat 分派逻辑

```rust
match (provider_id.as_str(), protocol) {
    // OpenAI-compatible providers
    ("openai" | "kimi" | "generic", Protocol::OpenAi) => {
        OpenAiChatCompletionsClient::new(api_key, base_url)
    }
    // Anthropic-compatible providers
    ("anthropic" | "xiaomi" | "minimax" | "generic", Protocol::Anthropic) => {
        AnthropicMessagesClient::new(api_key, base_url)
    }
    // Fallback: ProviderHandle
    _ => {
        ProviderHandle::from_url_with_user_agent(...)
            .into_chat_provider()
    }
}
```

### 7.3 其他能力

Embedding/Image/TTS/Search/Video/STT 暂时全部委托 `ProviderHandle`：

```rust
pub fn build_embedding_provider(&self, req) -> Option<Box<dyn EmbeddingProvider>> {
    ProviderHandle::from_url_with_user_agent(...)
        .and_then(|h| h.into_embedding_provider())
}
```

### 7.4 Protocol 解析

```rust
fn resolve_protocol(provider_id: &ProviderId, configured: Option<Protocol>) -> Protocol {
    if let Some(p) = configured { return p; }
    match provider_id.as_str() {
        "anthropic" | "xiaomi" | "minimax" => Protocol::Anthropic,
        _ => Protocol::OpenAi,
    }
}
```

---

## 8. Message Rendering

### 8.1 OpenAI（`protocols/openai/chat_message_rendering.rs`，101 行）

`render_openai_chat_body(req: &ChatRequest) -> Value`

功能：
- 将 `ChatMessage` → JSON messages 数组
- Content parts 渲染：Text → string/obj，ImageUrl/ImageB64 → image_url obj，Thinking → 跳过
- 单 text 部分渲染为 string，多部分渲染为 array
- Tool calls 转换为 OpenAI 格式（`tool_calls[].function`）
- Tool result 设置 `tool_call_id`
- 请求参数：`max_completion_tokens` + `max_tokens`（兼容新旧 API），`stream_options`，`parallel_tool_calls`

### 8.2 Anthropic（`protocols/anthropic/message_rendering.rs`，215 行）

`render_anthropic_messages(req: &ChatRequest) -> RenderedAnthropicMessages`

功能：
- System 提取：所有 system 消息合并到顶层 `system` 字段
- Tool result 转换：`tool_call_id` → `tool_result` content block
- Tool calls 转换：assistant 的 `tool_calls` → `tool_use` content blocks
- 空 text 过滤：`text.trim().is_empty()` 的 text block 被删除（Anthropic 400 "text content blocks must be non-empty"）
- 连续同 role 合并：MiniMax 等内部转 OpenAI 格式的 provider 需要严格交替
- 空 content 消息删除：assistant 消息 content 为空数组时整条删除

`build_anthropic_body(req: &ChatRequest) -> Value`

功能：
- 调用 `render_anthropic_messages` 获取渲染结果
- 组装完整请求 body：model、messages、system、tools
- Tools 转换为 Anthropic 格式（`name` + `description` + `input_schema`）

---

## 9. 配置变更

### 9.1 新增字段

在 `ProviderConfig` 上新增 `provider` 字段：

```toml
[providers.anthropic]
provider = "anthropic"    # 新增：显式指定 Provider 身份
api_key = "${ANTHROPIC_API_KEY}"
```

在 `ChatSection`、`EmbeddingSection`、`CapabilitySection` 上新增 `protocol` 字段：

```toml
[providers.anthropic.chat]
base_url = "https://us.jinl.in"
protocol = "anthropic"    # 新增：显式指定 API 协议
```

### 9.2 实际配置示例

```toml
# Anthropic (通过代理)
[providers.anthropic]
provider = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"

[providers.anthropic.chat]
base_url = "https://us.jinl.in"
protocol = "anthropic"
```

```toml
# OpenAI
[providers.openai]
provider = "openai"
api_key = "${OPENAI_API_KEY}"

[providers.openai.chat]
base_url = "https://api.openai.com/v1"
protocol = "openai"
```

```toml
# Xiaomi MiMo (Anthropic 协议)
[providers.xiaomi]
provider = "xiaomi"
api_key = "${XIAOMI_API_KEY}"

[providers.xiaomi.chat]
base_url = "https://api.xiaomimimo.com/anthropic/v1"
protocol = "anthropic"
```

```toml
# Kimi (OpenAI 协议)
[providers.kimi]
provider = "kimi"
api_key = "${KIMI_API_KEY}"

[providers.kimi.chat]
base_url = "https://api.moonshot.cn/v1"
protocol = "openai"
```

### 9.3 向后兼容

- `provider` 和 `protocol` 都是 `Option` 字段，不设置时走 fallback
- 不设 `provider` → 尝试 URL host 推断
- 不设 `protocol` → 使用 Provider 默认值（anthropic/xiaomi/minimax → Anthropic，其余 → OpenAI）
- **关键场景**：anthropic 用代理 `us.jinl.in`，必须显式指定 `provider="anthropic"` + `protocol="anthropic"`，否则 Factory 会 fallback 到 ProviderHandle

---

## 10. Provider 文件 Slim Down

### 10.1 变更概览

| 文件 | 重构前 | 重构后 | 变化 |
|---|---|---|---|
| `openai.rs` | 512 | 208 | -304 |
| `anthropic.rs` | 402 | 155 | -247 |
| `xiaomi.rs` | 477 | 65 | -412 |
| `kimi.rs` | 295 | 58 | -237 |
| **合计减少** | | | **-1200** |

### 10.2 各文件变更详情

**openai.rs（512 → 208）**：
- ChatProvider 委托 `OpenAiChatCompletionsClient`
- 删除：`parse_openai_sse`、`build_openai_body`、`chat_url`、未使用 imports
- 保留：Embedding/Image/TTS 逻辑不变

**anthropic.rs（402 → 155）**：
- ChatProvider 委托 `AnthropicMessagesClient`
- 删除：`client` field、`chat_url`、未使用 imports
- 保留：`parse_anthropic_sse`（被 protocol client 调用）

**xiaomi.rs（477 → 65）**：
- ChatProvider 直接构造 `AnthropicMessagesClient` 并委托
- 删除：全部 body 构建、SSE 解析、消息渲染逻辑
- 保留：`new()`、`with_base_url()`、`with_user_agent()` 构造器

**kimi.rs（295 → 58）**：
- ChatProvider 直接构造 `OpenAiChatCompletionsClient` 并委托
- 删除：全部 body 构建和消息渲染逻辑
- 保留：`new()`、`with_base_url()`、`with_user_agent()` 构造器

### 10.3 未变更文件

| 文件 | 行数 | 原因 |
|---|---|---|
| `glm.rs` | 522 | 差异太大，不适合 profile 化 |
| `minimax.rs` | 146 | 已有委托模式，Chat 走 Factory 路径 |
| `google.rs` | 274 | Search only，不在本次 Chat 重构范围 |
| `shared.rs` | 427 | 保留作为 fallback，未来逐步移除 |

---

## 11. 实施阶段

### 原始计划

| Phase | 内容 | 依赖 | 状态 |
|---|---|---|---|
| 1 | Config 新增 provider/protocol + ProviderId + ProviderFactory 骨架 | — | ✅ 完成 |
| 2 | 提取 OpenAiChatCompletionsClient + AnthropicMessagesClient + message rendering | Phase 1 | ✅ 完成 |
| 3 | Xiaomi/Kimi profile 化 | Phase 2 | ✅ 完成 |
| 4 | GLM 专用 client + MiniMax 委托 | Phase 2 | ⏭ 跳过（见下） |
| 5 | RegistryBuilder 替代 daemon 注册 | Phase 3+4 | ⏭ 跳过（见下） |
| 6 | 删除旧 ProviderHandle / shared.rs factory | Phase 5 | ⏭ 调整（见下） |
| 7 | 配置兼容性测试 + 回归验证 | Phase 6 | ❌ 未完成 |

### 实际执行

Phase 1-3 合并执行，Phase 4-6 做了简化：

- **GLM 不动**：分析后发现差异太大（do_sample、tool_stream、reasoning_content top-level、自定义 SSE parser），强行提取为 profile 会增加复杂性
- **MiniMax 不动**：已有委托模式
- **不新增 RegistryBuilder**：daemon.rs 的 `build_registry()` 改为调用 `ProviderFactory`，但没新建独立的 builder 类型
- **ProviderHandle 保留**：作为 Chat 的 fallback 和所有其他能力的分发器
- **shared.rs 保留**：`parse_openai_sse` 被协议客户端调用，`ProviderHandle` 仍用于 embedding/image/tts/search

**依赖关系实际执行**：1 → 2 → 3 → 合并完成，4/5/6 简化

---

## 12. 实际变更清单

### 12.1 新增文件（7 个）

| 文件 | 行数 | 内容 |
|---|---|---|
| `src/providers/provider_id.rs` | 106 | ProviderId newtype + detect_from_url + well_known |
| `src/providers/provider_factory.rs` | 274 | ProviderFactory + Build*Request structs |
| `src/providers/protocols/mod.rs` | 2 | module root |
| `src/providers/protocols/openai/mod.rs` | 1 | module root |
| `src/providers/protocols/openai/chat_completions.rs` | 125 | OpenAiChatCompletionsClient |
| `src/providers/protocols/openai/chat_message_rendering.rs` | 101 | render_openai_chat_body |
| `src/providers/protocols/anthropic/mod.rs` | 1 | module root |
| `src/providers/protocols/anthropic/messages.rs` | 112 | AnthropicMessagesClient |
| `src/providers/protocols/anthropic/message_rendering.rs` | 215 | render_anthropic_messages + build_anthropic_body |

**新增总计**: ~937 行

### 12.2 修改文件（7 个）

| 文件 | 变更 |
|---|---|
| `src/config/provider.rs` | 新增 Protocol enum、provider 字段、protocol 字段、From<AuthStyle> impl |
| `src/daemon.rs` | build_registry 使用 ProviderFactory，传入 provider_id + protocol |
| `src/providers/mod.rs` | 新增 `pub mod protocols` 和 re-exports |
| `src/providers/openai.rs` | 512→208，ChatProvider 委托 OpenAiChatCompletionsClient |
| `src/providers/anthropic.rs` | 402→155，ChatProvider 委托 AnthropicMessagesClient |
| `src/providers/xiaomi.rs` | 477→65，ChatProvider 委托 AnthropicMessagesClient |
| `src/providers/kimi.rs` | 295→58，ChatProvider 委托 OpenAiChatCompletionsClient |

**变更统计**: 16 files changed, +1161 insertions, -1288 deletions

### 12.3 Git 提交

```
43312af fix(clippy): add Default impl for ProviderFactory
b43a6a3 refactor(providers): protocol-based provider factory (Phase 1-6)
```

---

## 13. 设计讨论中的修正

RFC 设计过程中多个方案被讨论后修正：

| 初始提案 | 修正结果 | 原因 |
|---|---|---|
| `ProviderIdentity` / `ProviderKind` enum | `ProviderId` newtype | 允许第三方扩展 |
| base_url 强制 origin-only | 保持现有行为 | GLM/Xiaomi 需要路径前缀 |
| 从 `[providers.<name>]` 推断 provider | 只从 URL host 推断 | name 是用户自定义标识 |
| base_url strict/warning 判断 | 移除 | 容易误伤代理路径 |
| 独立 normalization 层 | message rendering helper | 不新增行为（thinking block 不默认删除） |
| 暴露 `authentication` scheme 配置 | 保持 auth_style | 现有方案够用 |
| 暴露 `rotation_strategy` 配置 | 内部默认处理 | 过度设计 |
| GLM → profile 化 | 保持不动 | 差异太大 |
| 删除 ProviderHandle | 保留为 fallback | 其他能力未迁移 |
| 新增 RegistryBuilder | 直接在 daemon 中调用 Factory | 复用已有结构 |

---

## 14. 未完成工作

### 14.1 Phase 7：回归测试

- [ ] 所有 Provider 端到端 Chat 测试
- [ ] 配置兼容性：不设 provider/protocol 的旧行为是否正常
- [ ] Xiaomi → AnthropicMessagesClient 验证
- [ ] Kimi → OpenAiChatCompletionsClient 验证
- [ ] Anthropic 代理（us.jinl.in）显式配置验证
- [ ] OpenAI 标准配置验证
- [ ] GLM 不受影响验证
- [ ] Embedding/Image/TTS/Search 通过 ProviderHandle fallback 验证

### 14.2 未来改进

| 方向 | 说明 | 优先级 |
|---|---|---|
| Body renderer profile 化 | Xiaomi 有 thinking config，Kimi 有 reasoning_content top-level | P2 |
| GLM 协议客户端提取 | `GlmChatCompletionsClient` 独立实现 | P3 |
| Embedding 迁移到协议层 | `OpenAiEmbeddingsClient` | P3 |
| Image/TTS 迁移到协议层 | `OpenAiImageClient`、`OpenAiTtsClient` | P4 |
| Search 迁移到协议层 | `GlmWebSearchClient`、`GoogleSearchClient`、`MiniMaxSearchClient` | P4 |
| 移除 ProviderHandle | 所有能力迁移完成后 | P5 |
| 新增 RegistryBuilder | daemon 注册逻辑独立化 | P5 |

---

## 15. 测试计划

### 15.1 单元测试

- `ProviderId::detect_from_url`: 各已知 host 的推断
- `ProviderFactory::resolve_protocol`: 显式 > 默认 > fallback
- `render_openai_chat_body`: 消息渲染正确性
- `render_anthropic_messages`: system 提取、tool_use/tool_result 转换、同 role 合并、空 text 过滤

### 15.2 集成测试

- 各 Provider 的 Chat 端到端（实际 API 调用）
- 流式响应解析（SSE event sequence）
- Tool call 正确性（ToolCallStart/Delta/End 序列）
- 错误处理（HTTP 400/403/429/500）

### 15.3 配置兼容性测试

```toml
# Case 1: 无 provider/protocol（旧配置）→ URL 推断 + Provider 默认
[providers.openai]
api_key = "..."
[providers.openai.chat]
base_url = "https://api.openai.com/v1"

# Case 2: 显式 provider，无 protocol → Provider 默认 protocol
[providers.xiaomi]
provider = "xiaomi"
api_key = "..."

# Case 3: 显式 provider + protocol → 完全指定
[providers.anthropic]
provider = "anthropic"
api_key = "..."
[providers.anthropic.chat]
base_url = "https://us.jinl.in"
protocol = "anthropic"

# Case 4: 未知代理 + 显式指定
[providers.my-proxy]
provider = "generic"
[providers.my-proxy.chat]
base_url = "https://proxy.example.com/v1"
protocol = "openai"
```

### 15.4 已有测试

ProviderId 的 URL 推断测试已在 `provider_id.rs` 中：

```rust
#[test] fn detect_glm() { ... }
#[test] fn detect_xiaomi() { ... }
#[test] fn detect_openai() { ... }
#[test] fn detect_unknown() { ... }
```

---

## 附录 A：文件对照表（重构前 → 重构后）

| 重构前 | 重构后 |
|---|---|
| `shared.rs::create_provider()` | `ProviderFactory::build_chat_provider()` |
| `shared.rs::create_provider_by_url()` | `ProviderFactory` + ProviderHandle fallback |
| `shared.rs::ProviderHandle` | 保留（fallback） |
| `openai.rs::build_openai_body()` | `protocols/openai/chat_message_rendering.rs` |
| `openai.rs::parse_openai_sse()` | `shared.rs`（保留，被 protocol client 调用） |
| `anthropic.rs::build_anthropic_body()` | `protocols/anthropic/message_rendering.rs` |
| `anthropic.rs::parse_anthropic_sse()` | `anthropic.rs`（保留，被 protocol client 调用） |
| `xiaomi.rs::build_anthropic_body()` | 删除（委托 AnthropicMessagesClient） |
| `xiaomi.rs::parse_anthropic_sse()` | 删除（委托 AnthropicMessagesClient） |
| `kimi.rs::build_kimi_body()` | 删除（委托 OpenAiChatCompletionsClient） |
| `daemon.rs::build_registry()` 手动构造 | `ProviderFactory` 分派 |

## 附录 B：命名对照表

| 旧名称 | 新名称 | 说明 |
|---|---|---|
| — | `ProviderId` | Provider 身份标识（newtype） |
| — | `Protocol` | API 协议（enum） |
| — | `ProviderFactory` | 统一构建入口 |
| — | `BuildChatProviderRequest` | Chat 构建请求 |
| — | `OpenAiChatCompletionsClient` | OpenAI 协议客户端 |
| — | `AnthropicMessagesClient` | Anthropic 协议客户端 |
| `ProviderHandle` | 保留 | fallback 分发器 |
| `api_format` | `protocol` | 配置字段名 |
| `ProviderIdentity` | `ProviderId` | 避免过长 |
