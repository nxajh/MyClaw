# MyClaw 架构设计文档

> 2026-04-24 初版 · **2026-04-29 更新（实现状态）**
>
> ⚠️ **状态说明**：Phase 1-4 全部已完成。本文档描述的是最终目标架构，
> 部分细节（如 LoopBreakerAgent）仍在实现中，详见各章节标注。

---

## 1. DDD 架构分层

### 1.1 五层架构

```┌─────────────────────────────────────────────────┐
│           Orchestration Layer（编排层）              │
│  src/agents/orchestrator.rs                       │
├─────────────────────────────────────────────────┤
│           Interface Layer（接口层）                │
│  src/channels/wechat.rs                         │
│  src/channels/telegram.rs                       │
├─────────────────────────────────────────────────┤
│           Application Layer（应用层）              │
│  ┌─────────────────────────────────────────┐   │
│  │  src/agents/agent_impl.rs               │   │
│  │  - AgentLoop (核心循环)                  │   │
│  │  - SkillsManager (Skills 管理)          │   │
│  │  - McpManager (MCP 管理)               │   │
│  │  - SystemPromptBuilder (Prompt 构建)    │   │
│  └─────────────────────────────────────────┘   │
├─────────────────────────────────────────────────┤
│               Domain Layer（核心域）               │
│  ┌───────────────┬─────────────────────────┐   │
│  │  Session      │  Memory                 │   │
│  │  src/agents/ │  src/storage/           │   │
│  │  session_mgr  │  memory.rs, shared.rs   │   │
│  ├───────────────┼─────────────────────────┤   │
│  │  Provider     │  Tool trait            │   │
│  │  src/providers│                         │   │
│  │  /capability_│                         │   │
│  │  chat.rs     │                         │   │
│  └───────────────┴─────────────────────────┘   │
├─────────────────────────────────────────────────┤
│           Infrastructure Layer（基础设施）        │
│  ┌───────────────┬─────────────────────────┐   │
│  │  Registry     │  LoopBreakerAgent       │   │
│  │  src/registry│  (待实现)               │   │
│  ├───────────────┼─────────────────────────┤   │
│  │  Provider 实现│  Memory 存储实现        │   │
│  │  src/providers│  src/storage/sqlite.rs  │   │
│  │  openai.rs   │  src/storage/embedding  │   │
│  │  minimax.rs  │                        │   │
│  │  glm.rs      │                        │   │
│  ├───────────────┼─────────────────────────┤   │
│  │  Tool 实现    │  MCP Protocol          │   │
│  │  src/tools/  │  src/mcp/               │   │
│  │  (13个工具) │  (Stdio/HTTP/SSE)     │   │
│  └───────────────┴─────────────────────────┘   │
└─────────────────────────────────────────────────┘
```

> **注意**：当前采用单体 crate 结构（`src/` 下分模块），而非多 crate workspace。
> 编译时通过 Cargo feature flags 选择 Channel（`wechat`、`telegram`）。

### 1.2 各层职责

| 层次 | 职责 | 包含内容 |
|------|------|---------|
| **Orchestration** | Channel 管理、消息路由 | Orchestrator |
| **Interface** | 接入协议适配 | Channel Adapters（编译时可选） |
| **Application** | 业务编排 | AgentLoop, SkillsManager, McpManager, SystemPromptBuilder |
| **Domain** | 业务逻辑核心 | Session, Memory, Provider trait, Tool trait |
| **Infrastructure** | 技术实现 | ServiceRegistry, Provider 实现, Storage, Tool 实现, LoopBreakerAgent |

### 1.3 Provider 按能力拆分

**Domain 层：按能力定义独立 trait**

```rust
// Chat 能力
trait ChatProvider: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;
    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream>;
}

// Search 能力
trait SearchProvider: Send + Sync {
    async fn search(&self, req: SearchRequest) -> Result<SearchResponse>;
}

// Embedding 能力
trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse>;
}

// Provider 组合多个能力
trait Provider: ChatProvider + SearchProvider + EmbeddingProvider + Send + Sync {}
```

**Infrastructure 层：按需实现**

```rust
// OpenAI 实现 Chat + Search + Embedding
pub struct OpenAIProvider { ... }
impl ChatProvider for OpenAIProvider { ... }
impl SearchProvider for OpenAIProvider { ... }      // OpenAI Search API
impl EmbeddingProvider for OpenAIProvider { ... }

// Anthropic 只实现 Chat
pub struct AnthropicProvider { ... }
impl ChatProvider for AnthropicProvider { ... }

// Ollama 实现 Chat + Embedding
pub struct OllamaProvider { ... }
impl ChatProvider for OllamaProvider { ... }
impl EmbeddingProvider for OllamaProvider { ... }
```

**好处**：
- 按需注入，不需要所有 Provider 实现所有能力
- 类型安全，调用方只看到自己的能力接口
- 可组合，多个 Provider 可以组合成一个 Provider

### 1.4 Loop Breaker 装饰器

> ⚠️ **状态**：待实现（`src/agents/loop_breaker.rs`）

**目标**：检测 AgentLoop 中的循环模式，防止无限 tool 调用。

**三种循环模式检测**：

| 模式 | 条件 | 阈值 |
|------|------|------|
| **Exact repeat** | 同一 tool + 同一 args 连续调用 | ≥3 |
| **Ping-pong** | 两个 tool 来回切换 | ≥4 轮 |
| **No progress** | 同一 tool 调用 args 不同但结果 hash 相同 | ≥5 |

**配置参数**：

```toml
[agent.loop_breaker]
max_tool_calls = 100      # 硬限制兜底
window_size = 20           # 滑动窗口大小
max_repeats = 3           # Exact repeat 阈值
```

### 1.5 ServiceRegistry 位置

ServiceRegistry 属于 Infrastructure Layer。

```rust
// src/providers/service_registry.rs — trait 定义
// src/registry/mod.rs — 具体实现

pub trait ServiceRegistry: Send + Sync {
    fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;
    fn get_chat_provider_with_hint(&self, capability: Capability, provider_hint: Option<&str>) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;
    fn get_chat_fallback_chain(&self, capability: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>>;
    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)>;
    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)>;
    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)>;
    fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)>;
    fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)>;
    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)>;
}
```

Application 层依赖 ServiceRegistry 获取 Provider 能力。

### 1.6 Orchestrator 独立层

Orchestrator 是 Interface Layer 和 Application Layer 之间的"编排层"：

```rust
// src/agents/orchestrator.rs

pub struct Orchestrator {
    channels: Arc<DashMap<String, Arc<dyn Channel>>>,  // channel name → Channel
    sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>>,  // session key → AgentLoop
    agent: Agent,
    session_manager: SessionManager,
    pending_asks: Arc<DashMap<String, (oneshot::Sender<String>, String)>>,  // ask_user 处理
}

impl Orchestrator {
    pub async fn run(&self, shutdown_rx: watch::Receiver<bool>) -> anyhow::Result<()>;
    pub async fn shutdown_listeners(&mut self);
}
```

### 1.7 Memory 独立 Domain 模块

```
src/storage/
├── lib.rs              # 模块入口，导出
├── memory.rs           # Memory trait 定义
├── shared.rs           # SharedMemory 装饰器
├── private.rs          # PrivateMemory 装饰器
├── session.rs          # SessionBackend trait
├── sqlite.rs           # SqliteSessionBackend 实现（472行）
├── embedding.rs         # Embedding 存储
├── vector.rs           # 向量存储
├── policy.rs           # 记忆策略
└── types.rs            # MemoryEntry 等类型
```

> **MemoryStore**（`src/tools/memory.rs`）是内置工具，持有 `Arc<Memory>` 后端。
> **MCP** 通过 `McpManager` 连接外部 MCP Server，发现工具并包装为 `dyn Tool`。

### 1.8 Channel 独立 Crates（编译时选择）

```toml
# 编译时选择需要的 Channel
[features]
default = []
wechat = ["myclaw-channel-wechat"]
telegram = ["myclaw-channel-telegram"]
discord = ["myclaw-channel-discord"]
slack = ["myclaw-channel-slack"]

# 用户配置
[dependencies]
myclaw-orchestrator = { workspace = true }
myclaw-channel-wechat = { workspace = true, optional = true }
myclaw-channel-telegram = { workspace = true, optional = true }
```

```rust
// myclaw-channels/src/lib.rs
#[cfg(feature = "wechat")]
pub mod wechat;
#[cfg(feature = "telegram")]
pub mod telegram;
#[cfg(feature = "discord")]
pub mod discord;

// Interface 层聚合
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, msg: &SendMessage) -> Result<()>;
    async fn listen(&self, tx: Sender<ChannelMessage>) -> Result<()>;
}
```

**好处**：
- 不需要的 Channel 不编译进 binary
- 减少 binary 体积和依赖
- 微信 SDK 不影响 Telegram 用户

### 1.9 依赖关系

```
Interface (Channels: wechat.rs, telegram.rs)
        ↓ (依赖)
Orchestration (Orchestrator: orchestrator.rs)
        ↓ (依赖)
Application (AgentLoop, SkillsManager, McpManager, SystemPromptBuilder)
        ↓ (依赖)
Domain (Session, Memory trait, Provider trait, Tool trait)
        ↑ (实现)
Infrastructure (Registry, Provider 实现, Storage, Tools, MCP)
```

### 1.10 Section 映射

| Section | 所属层次 | 实现状态 |
|---------|---------|---------|
| §2 设计原则 | - | ✅ |
| §3 核心 trait 体系 | Domain | ✅ `src/providers/capability*.rs` |
| §4 Provider + Model 层 | Domain + Infrastructure | ✅ `src/providers/` |
| §5 Chat 能力的协议实现 | Infrastructure | ✅ `src/providers/openai.rs` 等 |
| §6 ServiceRegistry | Infrastructure | ✅ `src/registry/mod.rs` |
| §7 差异处理机制 | Infrastructure | ✅ 各 provider 自己实现 |
| §8 Agent Loop | Application | ✅ `src/agents/agent_impl.rs` |
| §9 System Prompt | Application | ✅ `src/agents/prompt.rs` |
| §10 Skills | Application | ✅ `src/agents/skills.rs` |
| §11 MCP | Application | ✅ `src/agents/mcp_manager.rs` + `src/mcp/` |
| §12 配置层 | Infrastructure | ✅ `src/config/` |
| §13 工厂函数 | Infrastructure | ✅ `src/providers/shared.rs` |
| §14 重构路径 | - | ✅ Phase 1-4 全部完成 |
| §15 设计边界 | - | ✅ |

---

## 2. 设计原则

1. **Provider 按能力拆分为独立 trait**。Chat、Search、Embedding、ImageGeneration、TTS、STT、Video 各有独立 trait，Provider struct 根据自身支持的能力实现并组合。
2. **API 协议是 provider 的实现细节**。不同的 provider 有不同的 endpoint、请求格式、响应格式，各自在自己的代码里处理。
3. **能力差异体现在 Provider 实现中**。每个 provider 只实现自己支持的 trait，不需要实现不支持的。
4. **路由基于 capability**。Agent Loop 和 Tools 通过 `registry.get_chat_provider()` 等方法获取对应能力的 provider，由 ServiceRegistry 负责路由。
5. **Chat 子能力（Vision、Tool Calling）是配置标记**，不是独立的 trait。它们在 Chat 消息流中发生。
6. **Search 是 Provider 能力**。Web Search Tool 内部调用 Provider 的 search 能力（由 Perplexity、Tavily 等搜索服务实现）。

---

## 3. 核心 trait 体系

### 2.1 能力层级

架构分为两层：

```
Agent Loop（编排层）
  └── tools: Vec<Box<dyn Tool>>      ← 工具在此层
       ├── web_search（调用 registry.get_provider(Search) → SearchProvider）
       ├── calculator（纯本地）
       ├── image_generator（调用 Provider）
       ├── MCP tools（从 MCP server 发现）
       └── ...（70+ 实现）

Provider 层（能力层）
  ├── ChatProvider trait      → LLM 对话
  ├── SearchProvider trait    → 搜索 API（Perplexity、Tavily 等）
  ├── EmbeddingProvider trait → 文本嵌入
  ├── ImageGenerationProvider trait
  ├── TtsProvider trait
  ├── SttProvider trait
  └── VideoGenerationProvider trait
```

**工具和 provider 的关系：**
- Provider 不知道 tools——它只管"提供能力"
- Tool 知道调用哪个 Provider 来完成任务
- 同一个 Provider 能力可以被多个 Tool 使用

**Search 是 Provider 能力，Web Search Tool 调用 Provider 的 Search 能力：**
- SearchProvider trait 定义搜索接口，由 Perplexity、Tavily 等搜索服务实现
- WebSearch tool 内部调用 ServiceRegistry 获取 SearchProvider
- LLM Provider 如果也有搜索能力（如 Perplexity），可以同时实现 ChatProvider + SearchProvider

**MCP 也是 Tool：**
- MCP Server 暴露工具（filesystem、database、github 等）
- MCP Client 连接 server，发现工具，转换成 `Box<dyn Tool>`
- 和原生 tools 在同一抽象层

### 2.2 Provider trait（按能力拆分）

每个能力独立定义 trait，Provider struct 根据支持的能力实现并组合。

```rust
// === Chat 能力 ===
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// 流式响应。非流式 = 收集所有 events 合成 ChatResponse，由调用方决定。
    fn chat(&self, req: ChatRequest) -> Result<BoxStream<StreamEvent>>;
}

// === Image Generation 能力 ===
#[async_trait]
pub trait ImageGenerationProvider: Send + Sync {
    fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse>;
}

// === Text-to-Speech 能力 ===
#[async_trait]
pub trait TtsProvider: Send + Sync {
    fn synthesize(&self, req: TtsRequest) -> Result<AudioResponse>;
}

// === Speech-to-Text 能力 ===
#[async_trait]
pub trait SttProvider: Send + Sync {
    fn transcribe(&self, req: SttRequest) -> Result<TranscriptionResponse>;
}

// === Video Generation 能力 ===
#[async_trait]
pub trait VideoGenerationProvider: Send + Sync {
    fn generate_video(&self, req: VideoRequest) -> Result<VideoResponse>;
}

// === Search 能力（搜索 API，如 Perplexity、Tavily）===
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn search(&self, query: &str) -> Result<SearchResults>;
}

// === Embedding 能力 ===
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse>;
}
```

**ServiceRegistry 按能力类型存储和路由**，调用方只看到自己需要的能力接口。

**为什么按能力拆分，不用统一 trait：**
- 类型安全：调用方拿到 `dyn ChatProvider` 就知道它一定能 chat，不需要运行时检查 unsupported
- 按需注入：Jina 只实现 `EmbeddingProvider`，ElevenLabs 只实现 `TtsProvider`，不需要实现空的 chat
- 编译时检查：缺少实现会编译报错，而不是运行时 unsupported panic
- Provider 可以自由组合：OpenAI 同时实现 6 个 trait，DeepSeek 只实现 1 个

**Usage 结构只针对 Chat（有 token 概念的能力）：**

```rust
pub struct ChatUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}
```

其他能力（search、embed、image_gen）的用量单位不同，按需定义。

### 2.3 Chat 子能力（配置标记）

```rust
/// Chat 的行为特征——不是 trait，是配置声明
pub struct ChatFeatures {
    pub vision: bool,
    pub audio_input: bool,
    pub video_input: bool,
    pub native_tools: bool,
    pub max_image_size: Option<u64>,
    pub supported_image_formats: Vec<String>,
}
```

### 2.4 运行时 Capability 枚举（用于路由和查询）

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    // Provider 能力
    Chat,
    ImageGeneration,
    TextToSpeech,
    SpeechToText,
    VideoGeneration,
    Search,
    Embedding,

    // Chat 子能力（配置标记）
    Vision,
    AudioInput,
    VideoInput,
    NativeTools,
}
```

### 2.5 Tools 层（统一由 ServiceRegistry 负责路由）

Tools 在 Agent 编排层，不属于 Provider。Tool 持有 ServiceRegistry 引用，**路由统一由 ServiceRegistry 负责**，Tool 只负责调用。

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn execute(&self, args: &Value) -> Result<Value>;
}

// Tool 持有 registry，路由委托给 ServiceRegistry
pub struct ImageGeneratorTool {
    registry: Arc<dyn ServiceRegistry>,
    default_model: String,   // 配置："dall-e-3"
}

impl Tool for ImageGeneratorTool {
    fn execute(&self, args: &Value) -> Result<Value> {
        let model = args.get("model").unwrap_or(&self.default_model);
        let prompt = args.get("prompt").unwrap();

        // 1. 路由统一由 ServiceRegistry 负责
        let (provider, model_id) = self.registry.get_image_provider()?;

        // 2. 调用 provider 能力
        let response = provider.generate_image(ImageRequest {
            model: model_id,
            prompt: prompt.clone(),
        })?;

        Ok(response.into())
    }
}
```

**Tools（70+实现）：**

MyClaw 内置工具按功能分类：

| 分类 | 工具 | 调用方式 |
|------|------|---------|
| **核心（默认加载）** | `shell`, `file_read`, `file_write`, `file_edit`, `glob_search`, `content_search` | 纯本地或 registry |
| **Memory** | `memory_store`, `memory_recall`, `memory_forget`, `memory_export`, `memory_purge` | 调用 memory backend |
| **Cron/定时** | `cron_add`, `cron_list`, `cron_remove`, `cron_run`, `cron_runs`, `cron_update`, `schedule` | 调用 cron 系统 |
| **Web** | `web_search`, `web_fetch`, `http_request`, `browser`, `browser_open`, `text_browser` | 外部 API / 浏览器 |
| **Image/Media** | `image_gen`, `image_info`, `screenshot` | registry.get_provider(ImageGeneration) |
| **AI/代码** | `llm_task`, `claude_code`, `delegate`, `model_switch`, `skill_tool` | registry.get_provider(Chat) |
| **Shell/Git** | `shell` (已包含), `git_operations` | 本地执行 |
| **通信** | `pushover`, `poll`, `ask_user`, `escalate_to_human`, `reaction` | 外部 API |
| **Session** | `sessions_list`, `sessions_history`, `sessions_send` | session store |
| **外部集成** | `notion_tool`, `jira_tool`, `linkedin`, `google_workspace`, `microsoft365`, `composio` | 外部 API |
| **配置** | `model_routing_config`, `proxy_config`, `security_ops` | 配置系统 |
| **其他** | `calculator`, `weather`, `canvas`, `backup_tool`, `pipeline`, `project_intel` | 各类服务 |
| **MCP** | `mcp_client`, `mcp_tool`, `mcp_deferred` | MCP Server 发现 |

**示例工具实现（image_generator）：**

```rust
// Tool 调用 registry，按 capability 路由
pub struct ImageGeneratorTool {
    registry: Arc<dyn ServiceRegistry>,
}

impl Tool for ImageGeneratorTool {
    fn execute(&self, args: &Value) -> Result<Value> {
        let prompt = args.get("prompt").unwrap();

        // 路由统一由 ServiceRegistry 负责
        let (provider, model_id) = self.registry.get_image_provider()?;

        // 调用 provider 能力
        let response = provider.generate_image(ImageRequest {
            model: model_id,
            prompt: prompt.clone(),
        })?;

        Ok(response.into())
    }
}
```

**Tool 的职责分工：**
- **ServiceRegistry**：负责路由——按 model 找 provider，支持 fallback 等策略
- **Tool**：负责参数验证、结果格式化、错误处理；**不负责路由**

```
Agent Loop
  └── tools: Vec<Box<dyn Tool>>
       ├── web_search（调用 registry.get_provider(Search)）
       ├── image_generator（调用 registry.get_provider(ImageGeneration)）
       ├── calculator（纯本地）
       ├── tts（调用 registry.get_provider(TextToSpeech)）
       └── ...

ServiceRegistry（统一路由层）
  ├── routing.chat → [minimax-m2.7, gpt-4o, glm-4-plus]
  ├── routing.search → [tavily, perplexity]
  ├── routing.image_generation → [dall-e-3]
  └── routing.tts → [minimax-tts]
```

**MCP (Model Context Protocol)：**

```
MCP Server（外部服务）
  ├── filesystem
  ├── database
  └── github

MCP Client（MyClaw 内）
  ├── 连接 Server
  ├── 发现可用 tools
  └── 转换成 Box<dyn Tool>

Agent Loop
  └── tools: Vec<Box<dyn Tool>>
       ├── MCP tools（动态）
       └── Native tools（静态）
```

### 2.6 与现有 trait 的关系

| 现有 trait | 改造 | 说明 |
|-----------|------|------|
| `Provider` | 拆分 | 拆分为 ChatProvider、SearchProvider、EmbeddingProvider、ImageGenerationProvider、TtsProvider、SttProvider、VideoGenerationProvider |
| `Tool` | 不变 | 70+ 实现，简洁统一 |
| `Channel` | 不变 | 25+ 实现，接口清晰 |
| `Memory` | 不变 | 多后端支持 |
| `RuntimeAdapter` | 不变 | 平台抽象 |

## 4. Provider + Model 层

### 3.1 三层架构

```
Provider（连接信息：endpoint + credentials + 协议行为）
  └── Model（能力 + 定价，声明在 provider 下面）

运行时：直接返回 Provider 实例
```

- **Provider** = 纯连接：base_url、api_key、auth_style、请求行为配置
- **Model** = 纯声明：capabilities、pricing、max_output_tokens
- **同一个 model 可以出现在多个 provider 下**（如 gpt-4o 在 openai 和 azure-openai），能力一样但定价可能不同
- **每个 provider 的 model 声明自包含**——capabilities 写在每个 provider 下，不搞全局 model 定义 + 引用。清晰比 DRY 重要。

### 3.2 Provider（连接层）

```rust
/// 纯粹描述"一个 API 服务的接入信息"
pub struct Provider {
    pub name: String,               // "openai"
    pub base_url: String,
    pub api_key: SecretString,
    pub auth_style: AuthStyle,      // Bearer / Azure / ZhipuJWT
    pub extra_headers: HashMap<String, String>,
    pub timeout_secs: u64,
    // provider 级别的请求行为（所有走 OpenAI 协议的 model 共享）
    pub chat_config: Option<OpenAiChatConfig>,
    // 该 provider 提供的 model 列表
    pub models: HashMap<String, ProviderModel>,
}

/// Provider 下的 Model 实例——能力 + 定价
pub struct ProviderModel {
    pub name: String,                       // "gpt-4o"
    pub capabilities: Vec<Capability>,      // [Chat, Vision, NativeTools]
    pub chat_features: Option<ChatFeatures>,// Chat 子能力细节
    pub max_output_tokens: Option<u32>,
    pub pricing: Pricing,
}
```

### 3.3 ServiceRegistry 接口

ServiceRegistry 按能力类型存储 provider 实例，调用方通过 capability 获取对应的 trait object。

```rust
pub trait ServiceRegistry: Send + Sync {
    /// 注册 provider（根据其支持的 capability 分类存储）
    fn register(&mut self, config: ProviderConfig);

    /// 按 capability 获取 Chat provider，返回 (provider, model_id)
    fn get_chat_provider(&self, capability: Capability) -> Result<(Box<dyn ChatProvider>, String)>;

    /// 按 capability 获取 Search provider
    fn get_search_provider(&self) -> Result<(Box<dyn SearchProvider>, String)>;

    /// 按 capability 获取 Embedding provider
    fn get_embedding_provider(&self) -> Result<(Box<dyn EmbeddingProvider>, String)>;

    /// 按 capability 获取 ImageGeneration provider
    fn get_image_provider(&self) -> Result<(Box<dyn ImageGenerationProvider>, String)>;

    /// 按 capability 获取 TTS provider
    fn get_tts_provider(&self) -> Result<(Box<dyn TtsProvider>, String)>;

    /// 按 capability + provider_hint 覆盖
    fn get_chat_provider_with_hint(&self, provider_hint: Option<&str>) -> Result<(Box<dyn ChatProvider>, String)>;
}
```

调用方直接用：

```rust
// Agent Loop - chat
let (provider, model_id) = registry.get_chat_provider(Capability::Chat)?;
provider.chat(ChatRequest {
    model: model_id,  // 来自 routing 配置
    messages: [...],
})?;

// Tools - Image Generation
let (provider, model_id) = registry.get_image_provider()?;
provider.generate_image(ImageRequest { model: model_id, ... })?;

// Tools - Search
let (provider, model_id) = registry.get_search_provider()?;
provider.search(query)?;
```

不需要 ResolvedModel——model_id 在调用方的请求里。

### 3.4 Provider 实现示例

```rust
// providers/openai.rs — 一个 provider，实现多个能力 trait
pub struct OpenAi {
    descriptor: ProviderDescriptor,
}

impl ChatProvider for OpenAi {
    fn chat(&self, req: ChatRequest) -> Result<BoxStream<StreamEvent>> {
        let url = format!("{}/chat/completions", self.descriptor.base_url);
        Ok(http_stream(url, self.build_chat_request(req)))
    }
}

impl ImageGenerationProvider for OpenAi {
    fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse> {
        let url = format!("{}/images/generations", self.descriptor.base_url);
        http_post(url, self.build_image_request(req))
    }
}

impl TtsProvider for OpenAi { fn synthesize(&self, req: TtsRequest) -> Result<AudioResponse> { ... } }
impl SttProvider for OpenAi { fn transcribe(&self, req: SttRequest) -> Result<TranscriptionResponse> { ... } }
impl EmbeddingProvider for OpenAi { fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse> { ... } }

// providers/minimax.rs
pub struct Minimax { descriptor: ProviderDescriptor }
impl ChatProvider for Minimax { fn chat(&self, req: ChatRequest) -> Result<BoxStream<StreamEvent>> { ... } }
impl ImageGenerationProvider for Minimax { fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse> { ... } }
impl TtsProvider for Minimax { fn synthesize(&self, req: TtsRequest) -> Result<AudioResponse> { ... } }

// providers/elevenlabs.rs — 只有 TTS
pub struct ElevenLabs { descriptor: ProviderDescriptor }
impl TtsProvider for ElevenLabs { fn synthesize(&self, req: TtsRequest) -> Result<AudioResponse> { ... } }

// providers/jina.rs — 只有 Embedding
pub struct Jina { descriptor: ProviderDescriptor }
impl EmbeddingProvider for Jina { fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse> { ... } }

// providers/perplexity.rs — Chat + Search
pub struct Perplexity { descriptor: ProviderDescriptor }
impl ChatProvider for Perplexity { fn chat(&self, req: ChatRequest) -> Result<BoxStream<StreamEvent>> { ... } }
impl SearchProvider for Perplexity { fn search(&self, query: &str) -> Result<SearchResults> { ... } }
```

### 3.5 现实中的 Provider + Model 分布

```
Provider: openai
  ├── gpt-4o           → Chat ✅ Vision ✅ Tools ✅   $2.50/$10.00
  ├── gpt-4o-mini      → Chat ✅ Vision ✅ Tools ✅   $0.15/$0.60
  ├── dall-e-3         → Image Gen ✅                  $0.04/张
  ├── tts-1            → TTS ✅                        $15/百万字符
  ├── whisper-1        → STT ✅                        $0.006/分钟
  └── text-embedding-3 → Embedding ✅                  $0.02/百万token

Provider: minimax
  ├── minimax-m2.7     → Chat ✅ Vision ✅             ¥1.00/¥2.00
  ├── minimax-tts      → TTS ✅
  └── minimax-img      → Image Gen ✅

Provider: glm
  ├── glm-4-plus       → Chat ✅ Vision ✅ Tools ✅   ¥0.70/¥0.70
  ├── glm-4-flash      → Chat ✅ Tools ✅             ¥0.10/¥0.10
  ├── cogview-4        → Image Gen ✅
  ├── cogvideox        → Video Gen ✅
  └── embedding-3      → Embedding ✅

Provider: azure-openai
  └── gpt-4o           → Chat ✅ Vision ✅ Tools ✅   $2.20/$8.80 (reserved)

Provider: deepseek
  └── deepseek-chat    → Chat ✅                      ¥1.00/¥2.00

Provider: elevenlabs
  └── eleven-turbo     → TTS ✅                        $3/百万字符

Provider: jina
  └── jina-embeddings  → Embedding ✅                  免费额度
```

**同一个 gpt-4o**：
- 在 `openai` 下：标准定价
- 在 `azure-openai` 下：reserved capacity 定价
- 能力完全一样，定价不同，各自声明

---

## 5. Type Specification（类型规范）

> 本节定义所有能力 trait 的精确类型，是动手实现的直接依据。  
> 格式：`pub struct` / `pub enum` / `pub trait` + 关键字段注释。  
> 不在本节定义的范围见 §5.13。

### 5.1 Chat 能力（ChatProvider）

```rust
// === 消息内容（多模态） ===

/// 消息内容片段，支持文本、图片等模态
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { url: String, detail: ImageDetail },
    ImageB64 { b64_json: String, detail: ImageDetail },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum ImageDetail {
    #[default]
    Auto,    // provider 自行决定
    Low,     // 低分辨率
    High,    // 高分辨率
}

/// 消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub parts: Vec<ContentPart>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self { role: role.into(), parts: vec![ContentPart::Text { text: text.into() }] }
    }
    pub fn user_text(text: impl Into<String>) -> Self { Self::text("user", text) }
    pub fn assistant_text(text: impl Into<String>) -> Self { Self::text("assistant", text) }
    pub fn system_text(text: impl Into<String>) -> Self { Self::text("system", text) }
    pub fn with_image_url(mut self, url: impl Into<String>) -> Self {
        self.parts.push(ContentPart::ImageUrl { url: url.into(), detail: ImageDetail::Auto });
        self
    }
    /// 拼接所有 Text part
    pub fn text_content(&self) -> String {
        self.parts.iter().filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("")
    }
}

// === 流式事件 ===

/// 流式响应事件，由 ChatProvider::chat() 同步返回
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 文本增量
    Delta { text: String },
    /// reasoning/thinking 增量
    Thinking { text: String },
    /// tool_call 开始
    ToolCallStart { id: String, name: String },
    /// tool_call 参数增量（用于流式工具调用）
    ToolCallDelta { id: String, delta: String },
    /// tool_call 完成（保留完整 arguments，供后续执行）
    ToolCallEnd { id: String, name: String, arguments: String },
    /// token 用量（通常在流结束时到达）
    Usage(ChatUsage),
    /// 流结束，携带停止原因
    Done { reason: StopReason },
    /// 出错
    Error(String),
}

/// 停止原因
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ContentFilter,
    ToolUse,
    Timeout,
}

/// Chat token 用量
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

// === 请求 / 响应 ===

/// Chat 请求（发给 ChatProvider::chat()）
pub struct ChatRequest<'a> {
    /// 模型 ID（由 ServiceRegistry 从 routing 配置填充）
    pub model: &'a str,
    /// 对话消息列表
    pub messages: &'a [ChatMessage],
    /// 温度（0.0–2.0）
    pub temperature: Option<f64>,
    /// 最大输出 token 数
    pub max_tokens: Option<u32>,
    /// 是否返回 reasoning/thinking 内容（由配置决定，不由用户指定）
    pub thinking: Option<ThinkingConfig>,
    /// stop 序列
    pub stop: Option<Vec<String>>,
    /// 随机种子
    pub seed: Option<u64>,
    /// 工具定义（ToolSpec 列表，供支持原生工具调用的 provider 使用）
    pub tools: Option<&'a [ToolSpec]>,
    /// 流式标志（固定 true，调用方不应传 false）
    pub stream: bool,
}

pub struct ThinkingConfig {
    /// reasoning effort，如 "high" | "medium" | "low"
    pub effort: Option<String>,
    /// reasoning 预算（token 上限）
    pub budget_tokens: Option<u32>,
}

/// Chat 响应（非流式，由调用方从 StreamEvent 合成）
#[derive(Default)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<ChatUsage>,
    pub reasoning_content: Option<String>,
    pub stop_reason: StopReason,
}

impl ChatResponse {
    /// 从 stream 收集完整响应
    pub async fn from_stream(stream: BoxStream<StreamEvent>) -> Self { todo!() }
}

/// LLM 请求的工具
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON 字符串，工具参数
    pub arguments: String,
}

// === ChatProvider trait ===

#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// 发起流式 chat，返回 SSE/流式事件 stream
    /// 非流式 = 调用方 collect 后用 ChatResponse::from_stream() 包装
    fn chat(&self, req: ChatRequest<'_>) -> Result<BoxStream<StreamEvent>>;
}
```

### 5.2 Streaming 便捷方法

```rust
// 便捷方法——不是 trait 方法，是调用方按需使用
pub async fn chat_full(
    chat: &dyn ChatProvider,
    req: ChatRequest<'_>,
) -> Result<ChatResponse> {
    let mut response = ChatResponse::default();
    let mut current_tool_call: Option<ToolCall> = None;

    let stream = chat.chat(req)?;
    tokio::pin!(stream);

    use futures_util::StreamExt;
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Delta { text } => response.text.push_str(&text),
            StreamEvent::Thinking { text } => {
                response.reasoning_content.get_or_insert_with(String::new).push_str(&text);
            }
            StreamEvent::ToolCallStart { id, name } => {
                current_tool_call = Some(ToolCall { id, name, arguments: String::new() });
            }
            StreamEvent::ToolCallDelta { id, delta } => {
                if let Some(ref mut tc) = current_tool_call {
                    if tc.id == id { tc.arguments.push_str(&delta); }
                }
            }
            StreamEvent::ToolCallEnd { id, name, arguments } => {
                if let Some(tc) = current_tool_call.take() {
                    if tc.id == id { response.tool_calls.push(tc); }
                }
                response.tool_calls.push(ToolCall { id, name, arguments });
            }
            StreamEvent::Usage(u) => response.usage = Some(u),
            StreamEvent::Done { reason } => response.stop_reason = reason,
            StreamEvent::Error(e) => return Err(anyhow::anyhow!("stream error: {}", e)),
        }
    }
    Ok(response)
}
```

### 5.3 搜索能力（SearchProvider）

```rust
// 搜索查询
pub struct SearchRequest {
    /// 查询字符串
    pub query: String,
    /// 最大返回结果数
    pub limit: Option<usize>,
    /// 搜索类型（web/news/images 等，由具体 provider 支持）
    pub search_type: Option<String>,
}

/// 搜索结果
#[derive(Debug, Clone)]
pub struct SearchResults {
    pub results: Vec<SearchResult>,
    pub total: Option<u64>,
    pub query: String,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    /// 标题
    pub title: String,
    /// URL
    pub url: String,
    /// 摘要/片段
    pub snippet: String,
    /// 发布时间（可选）
    pub published_at: Option<String>,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn search(&self, req: SearchRequest) -> Result<SearchResults>;
}
```

**与 WebSearch Tool 的关系：** WebSearch Tool 调用 `registry.get_search_provider()?.0.search(req)` 获取结果，不需要知道底层是 Perplexity、Tavily 还是 DuckDuckGo。

### 5.4 嵌入能力（EmbeddingProvider）

```rust
pub struct EmbedRequest {
    pub input: EmbedInput,
    pub model: String,
    /// 嵌入维度，仅部分 provider 支持
    pub dimensions: Option<u32>,
}

pub enum EmbedInput {
    /// 单条文本
    Text(String),
    /// 多条文本（批量）
    Texts(Vec<String>),
}

pub struct EmbedResponse {
    pub embeddings: Vec<f32>,
    /// 每条文本的 token 用量
    pub usage: Option<EmbeddingUsage>,
    pub model: String,
}

pub struct EmbeddingUsage {
    pub prompt_tokens: u64,
}

/// provider 内部处理批量：单条 = Vec::from([input])，多条直接转发
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse>;
}
```

### 5.5 图像生成能力（ImageGenerationProvider）

```rust
pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    /// 输出格式：url（默认）| b64_json
    pub response_format: Option<ImageFormat>,
    pub size: Option<ImageSize>,
    pub quality: Option<ImageQuality>,
    /// 生成数量（默认 1）
    pub n: Option<u32>,
}

pub enum ImageFormat {
    Url,     // 返回 URL
    B64Json, // 返回 base64 JSON
}

pub enum ImageSize {
    Square1024,   // 1024x1024
    Landscape1792, // 1792x1024
    Portrait1024,  // 1024x1792
}

pub enum ImageQuality {
    Standard,
    HD,
}

pub struct ImageResponse {
    pub images: Vec<ImageOutput>,
    pub usage: Option<ImageGenerationUsage>,
}

pub struct ImageOutput {
    pub url: Option<String>,
    pub b64_json: Option<String>,
    pub revised_prompt: Option<String>, // DALL-E 3 会返回优化后的 prompt
}

pub struct ImageGenerationUsage {
    pub prompt_tokens: u64,
    /// 该字段用于部分 provider 的计费
    pub completion_tokens: Option<u64>,
}

#[async_trait]
pub trait ImageGenerationProvider: Send + Sync {
    fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse>;
}
```

### 5.6 语音合成能力（TtsProvider）

```rust
pub struct TtsRequest {
    pub model: String,
    pub input: String,              // 要合成的内容
    pub voice: TtsVoice,
    pub response_format: Option<TtsFormat>,
    /// 语速 0.25 ~ 4.0，默认 1.0
    pub speed: Option<f32>,
}

pub enum TtsVoice {
    Id(String), // 使用 provider 提供的 voice ID 字符串
}

pub enum TtsFormat {
    Mp3,
    Opus,
    Flac,
    Wav,
}

pub struct AudioResponse {
    pub audio: AudioData,
    pub usage: Option<TtsUsage>,
}

pub struct AudioData {
    pub bytes: Vec<u8>,
    /// MIME 类型（audio/mpeg / audio/opus / audio/flac / audio/wav）
    pub mime_type: String,
}

pub struct TtsUsage {
    pub characters: u64,
    pub audio_duration_secs: Option<f32>,
}

#[async_trait]
pub trait TtsProvider: Send + Sync {
    fn synthesize(&self, req: TtsRequest) -> Result<AudioResponse>;
}
```

### 5.7 语音识别能力（SttProvider）

```rust
pub struct SttRequest {
    pub model: String,
    pub audio: SttAudioInput,
    /// BCP-47 语言标签，如 "en", "zh", "zh-CN"
    pub language: Option<String>,
    pub auto_detect: Option<bool>,
}

pub enum SttAudioInput {
    Url(String),
    Bytes { data: Vec<u8>, mime_type: String },
}

pub struct TranscriptionResponse {
    pub text: String,
    pub language: Option<String>,
    pub duration_secs: Option<f32>,
    pub segments: Option<Vec<SttSegment>>,
    pub usage: Option<SttUsage>,
}

pub struct SttSegment {
    pub start_secs: f32,
    pub end_secs: f32,
    pub text: String,
}

pub struct SttUsage {
    pub audio_duration_secs: f32,
    pub prompt_tokens: Option<u64>,
}

#[async_trait]
pub trait SttProvider: Send + Sync {
    fn transcribe(&self, req: SttRequest) -> Result<TranscriptionResponse>;
}
```

### 5.8 视频生成能力（VideoGenerationProvider）

```rust
pub struct VideoRequest {
    pub model: String,
    pub prompt: String,
    pub duration_secs: Option<u32>,
    pub resolution: Option<VideoResolution>,
    pub aspect_ratio: Option<AspectRatio>,
}

pub enum VideoResolution { Standard, HD }
pub enum AspectRatio { Landscape16x9, Portrait9x16, Square1x1 }

pub struct VideoResponse {
    pub videos: Vec<VideoOutput>,
    pub usage: Option<VideoUsage>,
}

pub struct VideoOutput {
    pub url: Option<String>,
    pub path: Option<String>,
    pub revised_prompt: Option<String>,
}

pub struct VideoUsage {
    pub video_duration_secs: u32,
    pub prompt_tokens: u64,
}

#[async_trait]
pub trait VideoGenerationProvider: Send + Sync {
    fn generate_video(&self, req: VideoRequest) -> Result<VideoResponse>;
}
```

### 5.9 Capability 枚举

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Chat,
    Vision,
    NativeTools,
    Search,
    Embedding,
    ImageGeneration,
    TextToSpeech,
    SpeechToText,
    VideoGeneration,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::Chat => "chat",
            Capability::Vision => "vision",
            Capability::NativeTools => "native-tools",
            Capability::Search => "search",
            Capability::Embedding => "embedding",
            Capability::ImageGeneration => "image-generation",
            Capability::TextToSpeech => "text-to-speech",
            Capability::SpeechToText => "speech-to-text",
            Capability::VideoGeneration => "video-generation",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "chat" => Some(Capability::Chat),
            "vision" => Some(Capability::Vision),
            "native-tools" => Some(Capability::NativeTools),
            "search" => Some(Capability::Search),
            "embedding" => Some(Capability::Embedding),
            "image-generation" => Some(Capability::ImageGeneration),
            "text-to-speech" => Some(Capability::TextToSpeech),
            "speech-to-text" => Some(Capability::SpeechToText),
            "video-generation" => Some(Capability::VideoGeneration),
            _ => None,
        }
    }
}
```

### 5.10 配置类型

#### ProviderConfig 新增 capabilities 字段

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub api: String, // "openai-compatible" | "anthropic" | "gemini"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model_id: String,
    /// 该 model 支持的能力列表
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,  // "text" | "image" | "audio"
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostConfig>,
}
```

#### RoutingConfig

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub chat: Option<RouteEntry>,
    pub search: Option<RouteEntry>,
    pub embedding: Option<RouteEntry>,
    pub image_generation: Option<RouteEntry>,
    pub text_to_speech: Option<RouteEntry>,
    pub speech_to_text: Option<RouteEntry>,
    pub video_generation: Option<RouteEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub strategy: RoutingStrategy,
    /// 按该顺序尝试的 model 列表
    pub models: Vec<String>,
    /// 覆盖默认路由：指定 provider
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RoutingStrategy {
    Fixed,    // 固定用第一个
    Fallback, // 前一个失败用下一个
    Cheapest, // 选最低成本
    Fastest,  // 选最低延迟
}
```

#### config.toml 示例

```toml
[[providers]]
name = "minimax"
api = "openai-compatible"
api_key = "your-key"
base_url = "https://api.minimaxi.com/v1"

[[providers.model]]
model_id = "MiniMax-M2.7"
capabilities = ["chat", "vision", "native-tools"]
input = ["text", "image"]
output = ["text"]
context_window = 1000000
max_tokens = 8192

[[providers.model]]
model_id = "MiniMax-Image"
capabilities = ["image-generation"]

[routing.chat]
strategy = "fixed"
models = ["MiniMax-M2.7"]

[[providers]]
name = "perplexity"
api = "openai-compatible"
api_key = "your-key"
base_url = "https://api.perplexity.ai"

[[providers.model]]
model_id = "sonar-pro"
capabilities = ["chat", "search"]

[routing.chat]
strategy = "fallback"
models = ["MiniMax-M2.7", "sonar-pro"]
```

### 5.11 错误类型

每个能力模块定义自己的错误类型。

```rust
use thiserror::Error;

// === Chat 错误 ===
#[derive(Error, Debug)]
pub enum ChatError {
    #[error("request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("provider error: {code} {message}")]
    ProviderError { code: String, message: String },
    #[error("stream ended unexpectedly")]
    UnexpectedEof,
    #[error("timeout after {0}s")]
    Timeout(u64),
    #[error("model {0} not found")]
    ModelNotFound(String),
    #[error("model {0} does not support {1:?}")]
    CapabilityNotSupported { model: String, capability: Capability },
    #[error("context window exceeded ({0} tokens)")]
    ContextWindowExceeded(u64),
}

// === Search 错误 ===
#[derive(Error, Debug)]
pub enum SearchError {
    #[error("request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("provider error: {0}")]
    ProviderError(String),
    #[error("rate limited, retry after {0}s")]
    RateLimited(u64),
}

// === Embedding 错误 ===
#[derive(Error, Debug)]
pub enum EmbeddingError {
    #[error("request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("input too long: {0} chars (max {1})")]
    InputTooLong(usize, usize),
}

// === ImageGeneration 错误 ===
#[derive(Error, Debug)]
pub enum ImageError {
    #[error("request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("provider error: {0}")]
    ProviderError(String),
}

// === Audio 错误（TTS / STT 共用） ===
#[derive(Error, Debug)]
pub enum AudioError {
    #[error("request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),
}
```

所有 trait 方法返回 `Result<T, E>`，`E` 是对应能力的错误类型，不是 `anyhow::Error`。Agent Loop 按需将特定错误转换为 `anyhow::Error`。

### 5.12 从旧 ChatMessage 迁移到新 ContentPart

**关键变更：content 从 `String` 变为 `Vec<ContentPart>`**

现有 `ChatMessage { role: String, content: String }` 只支持纯文本。新设计通过 `parts: Vec<ContentPart>` 支持文本、图片、音频等多模态内容。

```rust
// 新设计
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub parts: Vec<ContentPart>,
}

// 兼容旧代码：从 content String 构建 parts（deprecated）
impl ChatMessage {
    #[deprecated(since = "0.2.0", note = "use ChatMessage::text()")]
    pub fn from_content_string(role: String, content: String) -> Self {
        Self { role, parts: vec![ContentPart::Text { text: content }] }
    }
    /// 拼接所有 Text part（兼容旧 content）
    pub fn content_as_string(&self) -> String {
        self.parts.iter().filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("")
    }
}
```

**迁移策略（渐进式）：**

1. Provider 迁移期间：旧代码用 `ChatMessage::from_content_string(role, content)` 构建，新代码用 `ChatMessage::text(role, text)`
2. Provider 实现内部：处理 `ContentPart` → 各 provider API 格式的转换（各自实现）
3. 迁移完成后：删除 deprecated 方法

### 5.13 本规范的范围

以下类型的定义**不在本节范围内**，由对应模块自行定义：

| 类型 | 所属 |
|------|------|
| `ToolSpec` / `ToolCall` | Tool trait 层（myclaw-api/tool.rs），本架构不变 |
| `ChannelMessage` / `SendMessage` | Channel trait 层（myclaw-api/channel.rs），本架构不变 |
| `Memory store / retrieve types` | Memory trait 层，本架构不变 |
| `Skill definition types` | Skills 模块，本架构不变 |
| `Scheduler TaskSpec` | Cron 模块，本架构不变 |
| 各 provider 的私有 config（如 `OpenAiChatConfig`） | Provider 实现细节，各 provider 文件中定义 |
| `MemoryStore` / `RecallResult` 等 | Memory crate 定义 |

---

## 6. Chat 能力的协议实现

### 6.1 Provider 实现层（从 compatible.rs 重构而来）

```
providers/
  mod.rs              ← 工厂函数 + ProviderInstance 枚举
  openai.rs           ← OpenAI provider：Chat + ImageGen + TTS + STT + Embedding
  minimax.rs          ← MiniMax provider：Chat + ImageGen + TTS
  kimi.rs             ← Kimi provider：Chat
  glm.rs              ← GLM provider：Chat + ImageGen + VideoGen + Embedding
  deepseek.rs         ← DeepSeek provider：Chat
  anthropic.rs        ← Anthropic provider：Chat（独立协议）
  gemini.rs           ← Gemini provider：Chat（独立协议）
  elevenlabs.rs       ← ElevenLabs provider：TTS
  jina.rs             ← Jina provider：Embedding
  perplexity.rs       ← Perplexity provider：Chat + Search
```

每个文件是一个完整的 provider，实现它支持的各个能力 trait。

### 6.2 ChatProvider trait 的 chat 方法

```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// 流式响应。非流式 = 收集所有 events 合成 ChatResponse，由调用方决定。
    fn chat(&self, req: ChatRequest<'_>) -> Result<BoxStream<StreamEvent>>;
}
```

**为什么只保留流式：**

1. **流式是更通用的操作**。非流式是流式的子集（collect），反过来不成立
2. **流式在网络可靠性上更好**：持续有数据传输，连接不会被中间设备判定为空闲而断开；非流式 30-60 秒无数据，极易触发空闲超时被静默断开；流式通过 chunk 间隔可快速感知故障，非流式只能等超时
3. **重试代价一样**。流式中断后丢弃部分结果重试 = 非流式失败重试
4. **实现更简单**。每个 provider 只实现一个方法，不存在"两个方法行为不一致"的风险
5. **非流式只是便捷方法**，不是独立的接口

### 6.3 每个 provider 的 Chat 实现（parse_usage 差异示例）

```rust
// providers/minimax.rs
impl ChatProvider for Minimax {
    fn chat(&self, req: ChatRequest<'_>) -> Result<BoxStream<StreamEvent>> {
        let body = self.format_request(&req);
        let raw_stream = http_stream(&self.descriptor, "/chat/completions", &body);
        raw_stream.map(|chunk| self.parse_chunk(&chunk))
    }
    fn parse_usage(&self, raw: &Value) -> Option<ChatUsage> {
        let usage = raw.get("usage")?;
        Some(ChatUsage {
            input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
            output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()),
            cached_input_tokens: None,
            reasoning_tokens: usage.get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens"))
                .and_then(|v| v.as_u64()),
            cache_write_tokens: None,
        })
    }
}

// providers/kimi.rs
impl ChatProvider for Kimi {
    fn chat(&self, req: ChatRequest<'_>) -> Result<BoxStream<StreamEvent>> {
        let body = self.format_request(&req);
        let raw_stream = http_stream(&self.descriptor, "/chat/completions", &body);
        raw_stream.map(|chunk| self.parse_chunk(&chunk))
    }
    fn parse_usage(&self, raw: &Value) -> Option<ChatUsage> {
        let usage = raw.get("usage")?;
        Some(ChatUsage {
            input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
            output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()),
            cached_input_tokens: usage.get("cached_tokens").and_then(|v| v.as_u64()), // Kimi: 顶层
            reasoning_tokens: None,
            cache_write_tokens: None,
        })
    }
}

// providers/glm.rs
impl ChatProvider for Glm {
    fn chat(&self, req: ChatRequest<'_>) -> Result<BoxStream<StreamEvent>> {
        let body = self.format_request(&req);
        let raw_stream = http_stream(&self.descriptor, "/chat/completions", &body);
        raw_stream.map(|chunk| self.parse_chunk(&chunk))
    }
    fn parse_usage(&self, raw: &Value) -> Option<ChatUsage> {
        let usage = raw.get("usage")?;
        Some(ChatUsage {
            input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
            output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()),
            cached_input_tokens: usage.get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64()),
            reasoning_tokens: None,
            cache_write_tokens: None,
        })
    }
}
```

### 6.4 网络层作为共享基础设施

```rust
// providers/mod.rs 中的共享函数

/// 共享的流式 HTTP 请求——所有走 OpenAI 协议的 provider 都用这个
pub fn http_stream(
    descriptor: &ProviderDescriptor,
    path: &str,
    body: &serde_json::Value,
) -> BoxStream<SseChunk> {
    let url = format!("{}{path}", descriptor.base_url);
    // 统一的 SSE 流式调用：header 构建、超时、chunk 解析
}

/// 共享的非流式 HTTP 请求
pub fn http_post(
    descriptor: &ProviderDescriptor,
    path: &str,
    body: &serde_json::Value,
) -> Result<String> {
    let url = format!("{}{path}", descriptor.base_url);
    // 统一的 HTTP POST
}
```

### 6.5 每个 provider 的 Chat 实现策略

| Provider | `parse_usage` | 请求侧 | 说明 |
|----------|--------------|--------|------|
| `Minimax` | 自己实现（reasoning_tokens） | `merge_system_into_user` | 精确匹配 MiniMax API |
| `Kimi` | 自己实现（顶层 cached_tokens） | 标准 | 精确匹配 Kimi API |
| `Glm` | 自己实现（prompt_tokens_details） | `auth_style=zhipu-jwt` | 精确匹配 GLM API |
| `DeepSeek` | 自己实现或直接用标准 | 标准 | 看差异程度 |
| `OpenAi` | 标准 OpenAI 格式 | 标准 | 默认，大多数新 provider 直接用 |
| `Anthropic` | 完全独立 | 完全独立 | 不同协议 |
| `Gemini` | 完全独立 | 完全独立 | 不同协议 |

**原则：每个 provider 的 `parse_usage` 精确匹配自己的 API 文档，各写各的。如果恰好一样，代码重复没关系——清晰比 DRY 重要。**


## 7. 差异处理机制



### 7.1 每个 provider 自己的代码精确匹配自己的 API

不需要通用 parser，不需要 variant，不需要 fallback。

每个 provider struct 的 `parse_usage` 就是它 API 文档的精确映射。默认实现覆盖标准 OpenAI 格式，有差异的 provider override 自己的方法。代码即文档。

### 7.2 请求侧差异：配置驱动

"要不要合并 system 到 user"、"用什么 auth"、"发什么 UA"——这些是行为开关，不是 API 契约差异，放配置：

```rust
pub struct OpenAiChatConfig {
    pub auth_style: AuthStyle,             // Bearer / ZhipuJwt / XApiKey
    pub merge_system_into_user: bool,      // MiniMax 需要
    pub responses_fallback: bool,          // GLM 不需要
    pub user_agent: Option<String>,        // 某些 provider 需要
}
```

```toml
[providers.minimax.capabilities.chat]
type = "minimax"
merge_system_into_user = true

[providers.glm.capabilities.chat]
type = "glm"
auth_style = "zhipu-jwt"
responses_fallback = false
```

### 7.3 协议差异：独立实现

根本不同的 API（Anthropic Messages API vs OpenAI Chat API vs Gemini generateContent）——各自独立实现，不共享 parse 逻辑。

### 7.4 总结

| 差异类型 | 处理方式 | 需要用户配置？ | 需要写代码？ |
|---------|---------|-------------|------------|
| 响应字段不同 | 各 provider 精确实现自己的 parse_usage | ❌ | ✅ 每个 provider 一次 |
| 请求行为不同 | 配置驱动 | ✅ 选择性配置 | ❌ |
| 协议根本不同 | 独立实现 | ✅ 选 protocol | ✅ 完整实现一次 |

---

---

## 8. ServiceRegistry（能力路由中心）

### 8.1 核心接口

ServiceRegistry 按能力类型分别存储和路由 provider 实例。

```rust
pub struct ServiceRegistry {
    providers: HashMap<String, ProviderConfig>,
    routing: RoutingConfig,
    // 按能力分类缓存 provider 实例
    chat_handles: HashMap<String, ProviderHandle>,
    search_handles: HashMap<String, ProviderHandle>,
    embedding_handles: HashMap<String, ProviderHandle>,
    image_handles: HashMap<String, ProviderHandle>,
    tts_handles: HashMap<String, ProviderHandle>,
    stt_handles: HashMap<String, ProviderHandle>,
    video_handles: HashMap<String, ProviderHandle>,
}
```

### 8.2 按能力路由

```rust
impl ServiceRegistry {
    /// 按 capability 路由，获取 ChatProvider
    fn get_chat_provider(&self) -> Result<(Box<dyn ChatProvider>, String)> {
        let entry = self.get_routing_entry(Capability::Chat)?;
        self.route_chat(entry)
    }

    /// 按 capability 路由，获取 SearchProvider
    fn get_search_provider(&self) -> Result<(Box<dyn SearchProvider>, String)> {
        let entry = self.get_routing_entry(Capability::Search)?;
        self.route_search(entry)
    }

    /// 通用路由逻辑（按策略选择 model → 找到对应 provider）
    fn route<T>(&self, entry: &RouteEntry, capability: Capability) -> Result<(ProviderHandle, String)> {
        match entry.strategy {
            RoutingStrategy::Fixed => {
                let model = entry.models.first()
                    .ok_or_else(|| anyhow!("No models configured"))?;
                let handle = self.get_handle_by_model(model, capability)?;
                Ok((handle, model.clone()))
            }
            RoutingStrategy::Fallback => {
                for model in &entry.models {
                    if let Ok(handle) = self.get_handle_by_model(model, capability) {
                        return Ok((handle, model.clone()));
                    }
                }
                Err(anyhow!("All providers failed for capability {:?}", capability))
            }
            RoutingStrategy::Cheapest => { /* 按 pricing 排序 */ }
            RoutingStrategy::Fastest => { /* 按历史 latency 排序 */ }
        }
    }

    /// 按 capability + provider_hint 覆盖
    fn get_chat_provider_with_hint(&self, provider_hint: Option<&str>) -> Result<(Box<dyn ChatProvider>, String)> {
        if let Some(name) = provider_hint {
            let config = self.providers.get(name)
                .ok_or_else(|| anyhow!("Unknown provider: {}", name))?;
            // 找该 provider 支持 Chat 的第一个 model
            for (model_id, model) in &config.models {
                if model.capabilities.contains(&Capability::Chat) {
                    return Ok((self.build_chat_provider(config)?, model_id.clone()));
                }
            }
            return Err(anyhow!("Provider {} does not support Chat", name));
        }
        self.get_chat_provider()
    }

    /// 获取 routing 配置（内部方法）
    fn get_routing_entry(&self, capability: Capability) -> Result<&RouteEntry> {
        match capability {
            Capability::Chat => self.config.routing.chat.as_ref(),
            Capability::ImageGeneration => self.config.routing.image_generation.as_ref(),
            Capability::TextToSpeech => self.config.routing.text_to_speech.as_ref(),
            Capability::SpeechToText => self.config.routing.speech_to_text.as_ref(),
            Capability::VideoGeneration => self.config.routing.video_generation.as_ref(),
            Capability::Search => self.config.routing.search.as_ref(),
            Capability::Embedding => self.config.routing.embedding.as_ref(),
            _ => return Err(anyhow!("Capability {:?} not supported in routing", capability)),
        }.ok_or_else(|| anyhow!("No routing configured for {:?}", capability))
    }

    /// 按 model 找 provider handle（内部方法）
    fn get_handle_by_model(&self, model_id: &str, capability: Capability) -> Result<ProviderHandle> {
        for (provider_name, config) in &self.providers {
            if let Some(model) = config.models.get(model_id) {
                if model.capabilities.contains(&capability) {
                    return self.build_handle(config, capability);
                }
            }
        }
        Err(anyhow!("No provider found for model: {}", model_id))
    }

### 8.3 路由策略

```rust
pub enum RoutingStrategy {
    /// 固定指向某个 model
    Fixed,
    /// 按 model 列表依次尝试，失败自动降级
    Fallback,
    /// 按成本优先
    Cheapest,
    /// 按速度优先
    Fastest,
}

pub struct RouteEntry {
    pub strategy: RoutingStrategy,
    pub models: Vec<String>,      // 选 model（chat、image_gen、tts、embedding 等）
    pub providers: Vec<String>,   // 选 provider（search 等直接路由 provider 的场景）
}
```

### 8.4 路由流程示例

**场景：用户发消息 "你好"，routing.chat 配置为 fallback ["minimax-m2.7", "gpt-4o"]**

```
1. 遍历 model 列表 ["minimax-m2.7", "gpt-4o"]
2. 尝试 minimax-m2.7
   - 搜索所有 provider.models，找到 providers["minimax"].models["minimax-m2.7"]
   - 构建 Minimax provider 实例
   - 调用 chat() → 成功
3. 返回结果
```

**场景：minimax 挂了，fallback 到 gpt-4o**

```
1. 尝试 minimax-m2.7 → 连接超时
2. 尝试 gpt-4o
   - 搜索所有 provider.models，找到 providers["openai"].models["gpt-4o"]
   - 可能还找到 providers["azure-openai"].models["gpt-4o"]
   - 策略选 providers["openai"]
   - 构建 OpenAi provider 实例
3. 返回结果
```

---




## 9. Agent Loop（编排层）

### 9.1 核心结构

```
Session Manager
  ├── Session A ──→ AgentLoop A ──→ history_a, private_memory_a
  ├── Session B ──→ AgentLoop B ──→ history_b, private_memory_b
  └── Session C ──→ AgentLoop C ──→ history_c, private_memory_c
       ↑
    并行执行
```

**Agent（共享工厂）：**
```rust
pub struct Agent {
    pub registry: Arc<dyn ServiceRegistry>,  // 共享
    pub tools: Arc<Vec<Box<dyn Tool>>>,       // 共享（需 thread-safe）
    pub config: AgentConfig,
}
```

**AgentLoop（per-session 执行实例）：**
```rust
pub struct AgentLoop<'a> {
    registry: Arc<dyn ServiceRegistry>,       // 共享引用
    tools: Arc<Vec<Box<dyn Tool>>>,           // 共享引用
    session: &'a Session,                     // Session-specific
    loop_breaker: LoopBreaker,
}
```

**Session（隔离单元）：**
```rust
pub struct Session {
    pub session_id: String,
    pub history: Vec<ChatMessage>,             // 当前上下文窗口内的历史
    pub private_memory: Vec<MemoryEntry>,      // 从历史提取的关键信息（持久化）
    pub shared_memory_namespace: String,        // "shared"
    pub private_memory_namespace: String,       // "private_{session_id}"
    pub system_prompt_id: String,
}
```

### 9.2 Memory 分类

| 类型 | 作用域 | 持久化 | Session 结束后 | 用途 |
|------|--------|--------|--------------|------|
| **Shared Memory** | 所有 Session 共享 | ✓ | 保留 | 跨 Session 知识、用户偏好、事实 |
| **Private Memory** | 当前 Session | ✓ | 清空 | 被压缩历史的关键信息 |
| **History** | 当前 Session | ✓ | 保留（窗口内） | 当前上下文窗口内的对话 |

**Private Memory 作用：** History 被压缩时，从丢弃的历史中提取关键信息存入 Private Memory。

**跨 Channel 查询：**

可以通过传入其他 session 的 `session_id` 查询其私有 memory：
```rust
mem.recall(query, limit, session_id: Some("wechat:o9cq80zXXX")).await?
```

**限制（BM25 模式）：**

| 模式 | session_id 过滤 | 说明 |
|------|----------------|------|
| BM25 | ✗ | FTS 搜索不过滤 session，返回所有 session 的匹配 |
| Embedding | ✓ | 向量搜索过滤 session_id |
| Hybrid | 部分 | FTS 不过滤，vector 过滤 |

**注意：** 使用 Embedding 或 Hybrid 模式才能正确限制跨 Channel 查询范围。纯 BM25 模式会返回所有 session 的匹配结果。

### 9.3 Context 构建

```rust
async fn build_context(loop: &AgentLoop, user_message: &str) -> Result<Vec<Message>> {
    let mut messages = vec![];

    // 1. System prompt
    messages.push(system_prompt());

    // 2. Shared memory（按需查询）
    let shared = loop.memory.recall(
        query: user_message,
        namespace: "shared",
        session_id: None,
    ).await?;
    messages.extend(shared.into_iter().map(Message::from));

    // 3. Private memory（按需查询）
    let private = loop.memory.recall(
        query: user_message,
        namespace: &loop.session.private_memory_namespace,
        session_id: Some(&loop.session.session_id),
    ).await?;
    messages.extend(private.into_iter().map(Message::from));

    // 4. History（当前窗口内）
    messages.extend(loop.session.history.iter().cloned());

    // 5. 当前输入
    messages.push(user_message.clone().into());

    Ok(messages)
}
```

### 9.4 Memory 加载时机

| 时机 | 方式 | 说明 |
|------|------|------|
| **Session 启动** | Pre-inject（系统自动） | 加载相关 memory 到 context |
| **对话过程中** | Tool call（LLM 决定） | LLM 显式调用 `memory_recall` 查询 |

**Session 启动触发 Pre-inject 的条件：**

| 条件 | 是否 Pre-inject |
|------|----------------|
| 新 session（无 history） | ✓ |
| 有 session 但 context 为空（服务器重启等） | ✓ |
| 有 session 且有 context（正常运行） | ✗ |

### 9.5 History 管理

**持久化格式（带版本号）：**
```rust
pub struct InteractiveSessionState {
    pub version: u32,
    pub history: Vec<ChatMessage>,
}
```

**窗口压缩流程：**
```
History: [msg_1, ..., msg_100]  ← 超出窗口
  ↓ trim
History: [msg_50, ..., msg_100]  ← 窗口内保留
  ↓ extract(msg_1~msg_49)
Private Memory: ["Albert 在北京工作", "女儿 Joy 2岁", ...]
```

**Tool result 截断：**
```rust
// 截断中间，保留 head (2/3) + tail (1/3)
fn truncate_tool_result(output: &str, max_chars: usize) -> String {
    let head_len = max_chars * 2 / 3;
    let tail_len = max_chars - head_len;
    format!("{}\n\n[... truncated ...]\n\n{}",
        &output[..head_end], &output[tail_start..])
}
```

**预防性 trim：**
```rust
if estimate_tokens(history) > token_budget {
    fast_trim_tool_results(history, 4);  // 先快速截断
    if still_over_budget {
        prune_history(history, ...);      // 再深度清理
    }
}
```

### 9.6 Agent Loop 流程

```rust
use std::collections::HashMap;

impl AgentLoop<'_> {
    pub async fn run(&mut self, user_message: &str) -> Result<String> {
        // 1. 构建 context
        let messages = build_context(self, user_message).await?;

        loop {
            // 2. 获取 provider（per-provider 锁，序列化同 provider 的调用）
            let (provider, model_id) = self.registry.get_chat_provider().await?;

            // 3. 调用 chat
            let stream = provider.chat(ChatRequest {
                model: model_id,
                messages,
            }).await?;

            // 4. 处理流式响应
            let mut response = ChatResponse::new();
            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::TextDelta(text) => response.text.push(text),
                    StreamEvent::ToolCall(call) => response.tool_calls.push(call),
                    StreamEvent::Usage(usage) => response.usage = Some(usage),
                    StreamEvent::Done => break,
                }
            }

            // 5. 没有 tool calls → 返回文本给用户
            if response.tool_calls.is_empty() {
                self.session.history.push(assistant_msg(response.text.clone()));
                self.save_session().await?;
                return Ok(response.text.join(""));
            }

            // 6. 并行执行 tool calls（HashMap 保证配对正确）
            let mut results: HashMap<String, Value> = HashMap::new();
            let futures: Vec<_> = response.tool_calls.iter()
                .map(|call| async move {
                    let result = self.execute_tool(call).await;
                    (call.id.clone(), result)
                })
                .collect();

            for (id, result) in futures::future::join_all(futures).await {
                results.insert(id, result?);
            }

            // 7. 按 LLM 返回顺序处理结果
            for call in &response.tool_calls {
                self.loop_breaker.tool_call_count += 1;

                if self.loop_breaker.should_break() {
                    return Err(anyhow!("Loop breaker triggered"));
                }

                let result = results.remove(&call.id).unwrap();
                messages.push(Message::tool_result(call.id.clone(), result));
            }

            // 8. 加回 messages，继续 loop
            self.session.history.push(assistant_msg_with_tools(response.tool_calls));
            for (call, result) in response.tool_calls.iter().zip(results.into_values()) {
                self.session.history.push(tool_result_msg(call.id.clone(), result));
            }
        }
    }
}
```

### 9.7 Tool 执行

```rust
impl AgentLoop<'_> {
    async fn execute_tool(&self, call: &ToolCall) -> Result<Value> {
        let tool = self.tools.iter().find(|t| t.name() == call.name).unwrap();

        match call.name {
            // 纯本地 tool
            "calculator" | "shell" | "file_read" => tool.execute(&call.args),

            // Provider 能力的 tool
            "web_search" => {
                let (provider, model_id) = self.registry.get_search_provider().await?;
                provider.search(&call.args["query"])
            }
            "image_generator" => {
                let (provider, model_id) = self.registry.get_image_provider().await?;
                provider.generate_image(ImageRequest { model: model_id, .. })
            }
            "memory_recall" | "memory_store" => tool.execute(&call.args),

            // MCP tools
            _ => tool.execute(&call.args),
        }
    }
}
```

### 9.8 Loop Breaker（双重保护）

**双重保护机制：**

| 保护层 | 作用 | 触发条件 |
|--------|------|---------|
| **Circuit breaker** | 检测"卡住了" | 模式检测到循环 |
| **Max tool calls** | 资源保护兜底 | 达到硬限制 |

**Circuit breaker 检测三种循环模式：**

| 模式 | 条件 | 阈值 | 触发 |
|------|------|------|------|
| **Exact repeat** | 同一 tool + 同一 args 连续调用 | 3/4/5+ | Warning/Block/Break |
| **Ping-pong** | 两个 tool 来回切换（A→B→A→B） | 4/5/6+ 轮 | Warning/Block/Break |
| **No progress** | 同一 tool 调用 5+ 次，args 不同但结果 hash 相同 | 5/6/7+ | Warning/Block/Break |

```rust
impl LoopBreaker {
    fn should_break(&self) -> bool {
        if self.loop_detector.tool_call_count >= self.max_tool_calls {
            return true;
        }
        self.loop_detector.should_break()
    }
}

pub enum LoopDetectionResult {
    Ok,                        // 继续正常
    Warning(String),            // 注入 system message 引导 LLM 换策略
    Block(String),              // 拒绝执行，结果替换为错误信息
    Break(String),              // 断开 loop
}
```

**配置参数：**

```toml
[agent.loop_breaker]
max_tool_calls = 100           # 硬限制兜底
window_size = 20              # LoopDetector 滑动窗口大小
max_repeats = 3               # Exact repeat 阈值
```

| 场景 | max_tool_calls | 说明 |
|------|---------------|------|
| 简单任务 | 20-30 | 预期几轮完成 |
| 复杂多步 | 50-100 | 需要更多 tool 调用 |
| 调试模式 | 设高或不设 | 不希望被中断 |

### 9.9 Provider 限流（per-provider 锁）

**同一时刻，一个 provider 实例只能被一个 AgentLoop 持有：**

```
Session A Loop ──→ Lock(provider) ──→ OpenAI API
Session B Loop ──→ waiting...       ──→ (blocked)
Session C Loop ──→ waiting...       ──→ (blocked)
```

```rust
impl ServiceRegistry {
    struct ProviderHandle {
        provider: Box<dyn ChatProvider>,  // 或其他能力 trait
        lock: Arc<Mutex<()>>,
    }

    async fn get_chat_provider(&self) -> Result<ProviderGuard> {
        let (provider, model_id) = self.route_chat().await?;
        let guard = self.provider_pool.acquire(&provider.key()).await;
        Ok(ProviderGuard { guard, provider, model_id })
    }
}
```

---



## 10. System Prompt（提示词管理）

### 10.1 构建流程

```
build_system_prompt() → 按顺序拼接 sections：

0. Anti-narration（最高优先级）
   - "Never narrate tool usage..."

0b. Tool Honesty
   - "Never fabricate tool results..."

1. Tooling（可选 compact 模式）
   - Full: 工具名称 + 描述
   - Compact: 只显示工具名称

1b. Hardware（如果存在 gpio/arduino 工具）
   - 硬件访问授权说明

1c. Action instruction
   - native_tools vs 非 native 的不同提示

2. Safety（受 autonomy_level 影响）
   - Full autonomy: 直接执行
   - ReadOnly: 只读限制
   - Default: 需确认

3. Skills（Full 或 Compact 模式）

4. Workspace
   - 工作目录

5. Bootstrap files（AIEOS 或 OpenClaw 格式）
   - AIEOS: 从配置加载 JSON
   - OpenClaw: SOUL.md, USER.md 等 workspace 文件

6. Date & Time（强制北京时间）

7. Runtime
   - Host, OS, Model

8. Channel Capabilities（compact 模式跳过）
   - 消息机器人说明
   - TTS 由 channel 处理

9. Truncation（按 max_system_prompt_chars 截断）
```

### 10.2 关键参数

```rust
build_system_prompt_with_mode_and_autonomy(
    workspace_dir,
    model_name,
    tools: &[(&str, &str)],      // (name, description)
    skills: &[Skill],
    identity_config: Option<&IdentityConfig>,  // AIEOS 或 OpenClaw
    bootstrap_max_chars: Option<usize>,        // 每个文件上限
    autonomy_config: Option<&AutonomyConfig>,
    native_tools: bool,                         // 原生 vs XML 协议
    skills_prompt_mode: SkillsPromptInjectionMode,
    compact_context: bool,                     // 紧凑模式
    max_system_prompt_chars: usize,             // 总字符数上限
)
```

### 10.3 native_tools

**来源：** `provider.supports_native_tools()`

| Provider | 返回值 | 说明 |
|----------|--------|------|
| OpenAI | true | 支持原生 function calling |
| Anthropic | true | 支持原生 function calling |
| Ollama | **false** | 很多模型不支持原生 tool-calling，用 XML 协议 |

**作用：**
- native_tools=true → 用原生 tool calling
- native_tools=false → 用 XML 协议（`
<tool_call>` 标签），需要额外注入 tool instructions

```rust
// 非原生 tool calling，需要额外注入
if !native_tools {
    system_prompt.push_str(&build_tool_instructions(&tools_registry, Some(&i18n_descs)));
}
```

### 10.4 compact_context

**配置项：** `config.agent.compact_context: bool`

**用途：** 用于 13B 或更小的模型

| 条件 | bootstrap_max_chars | rag_chunk_limit |
|------|-------------------|----------------|
| compact_context=true | 6000 | 2 |
| compact_context=false | 20000（默认） | 5 |

**compact_context=true 时：**
- 工具列表只显示名称，不显示描述
- Channel Capabilities 跳过
- Bootstrap 文件截断到 6000 chars

### 10.5 max_system_prompt_chars

**配置项：** `config.agent.max_system_prompt_chars: usize`

**作用：** 限制组装后的 system prompt 总字符数，超出则截断（保留开头部分）

**默认值：** 0（不限制）

**用途：** 小上下文模型（如 glm-4.5-air ~8K tokens → 设为 8000）

### 10.6 Bootstrap 文件

**OpenClaw 格式（默认）：**
```
AGENTS.md, SOUL.md, TOOLS.md, IDENTITY.md, USER.md, BOOTSTRAP.md, MEMORY.md
```

每个文件有字符上限（默认 20000，compact 模式 6000）。

**AIEOS 格式：** 从配置加载 JSON，替换 OpenClaw 文件。

### 10.7 配置项汇总

```toml
[agent]
# 紧凑模式（用于小模型）
compact_context = false

# System prompt 总字符数上限（0 = 不限制）
max_system_prompt_chars = 0

# 最大 history 消息数
max_history_messages = 50

# 最大 context tokens（触发压缩）
max_context_tokens = 32000

# 最大 tool 调用迭代次数
max_tool_iterations = 10

[agent.thinking]
# Thinking/reasoning 级别控制
```

---



## 11. Skills（用户定义的 Skill）

### 11.1 定位

Skill 是用户定义的工具 + 指令集，与 Tools 的区别：

| | Tools | Skills |
|--|-------|--------|
| **来源** | 内置/厂商提供 | 用户本地定义 |
| **协议** | 固定接口 | 自定义（shell/http/script） |
| **定义方式** | Rust 代码 | SKILL.toml / SKILL.md |

### 11.2 Skill 结构

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: Option<String>,
    pub tags: Vec<String>,
    pub tools: Vec<SkillTool>,    // skill 定义的工具
    pub prompts: Vec<String>,     // skill 指令
    pub location: Option<PathBuf>,
}

pub struct SkillTool {
    pub name: String,
    pub description: String,
    pub kind: String,     // "shell", "http", "script"
    pub command: String,
}
```

### 11.3 加载来源

| 来源 | 路径 | 说明 |
|------|------|------|
| Workspace skills | `~/.zeroclaw/workspace/skills/<name>/` | 用户本地 |
| Open-skills | GitHub besoeasy/open-skills | 社区 skills（可选） |

**加载流程：**
```
load_skills()
  ├── load_workspace_skills()      // 从 workspace/skills 目录
  └── load_open_skills()          // 从 open-skills repo（可选）
        ├── git clone
        └── git pull (每周一次)
```

### 11.4 SKILL.md 格式

```markdown
---
name: ctrip-flight
description: 机票查询 skill
---

# Ctrip 机票查询 Skill

## 使用方法
...
```

**SKILL.toml 格式：**
```toml
[skill]
name = "ctrip-flight"
description = "机票查询"
version = "0.1.0"
author = "Albert"

[[tools]]
name = "search_flight"
description = "查询机票"
kind = "shell"
command = "python3 scripts/ctrip_flight.py {from} {to} {date}"
```

### 11.5 Skill → Tool 转换

```rust
skills_to_tools(skills) → Vec<Box<dyn Tool>>
  ├── kind="shell"/"script" → SkillShellTool
  └── kind="http" → SkillHttpTool
```

Skill 定义的 tools 会被转换为可调用的 Tool 对象。

### 11.6 System Prompt 注入

```rust
skills_to_prompt_with_mode(skills, mode)
  ├── Full: 完整 instructions + tools 列表
  └── Compact: 只显示 summary，instructions 按需加载
```

### 11.7 安装来源

| 来源 | 示例 | 说明 |
|------|------|------|
| ClawhHub | `clawhub:ctrip-flight` | marketplace |
| Git SSH | `git@github.com:user/repo.git` | SSH 协议 |
| Git HTTPS | `https://github.com/user/repo` | HTTPS 协议 |
| 本地路径 | `/path/to/skill` | 本地目录 |

### 11.8 安全审计

每个 skill 安装前需要通过安全审计：
- 检查危险命令
- 脚本文件需特别授权（`allow_scripts = true`）
- 审计失败则拒绝安装

---

## 12. MCP（Model Context Protocol）

### 12.1 定位

MCP 是连接外部 MCP Server 的协议，发现和调用其 tools。

**与 Skills 的区别：**

| | Skills | MCP |
|--|--------|-----|
| **来源** | 本地定义 | 外部 Server |
| **协议** | 自定义（shell/http/script） | MCP 协议（JSON-RPC） |
| **工具发现** | 静态定义 | 动态发现（tools/list） |
| **部署** | 本地文件 | 远程服务 |

### 12.2 连接流程

```
McpServer::connect(config)
  → create_transport()      // stdio / HTTP / SSE
  → initialize handshake     // JSON-RPC
  → tools/list              // 获取可用工具
  → McpServer { tools }
```

### 12.3 Transport 类型

| Transport | 说明 | 适用场景 |
|-----------|------|---------|
| **Stdio** | 启动本地进程，stdin/stdout 通信 | 本地 MCP Server |
| **HTTP** | POST 请求发送 JSON-RPC | 远程 HTTP Server |
| **SSE** | HTTP POST + SSE 流接收响应 | 支持流式响应的 Server |

### 12.4 Stdio Transport

```rust
StdioTransport {
    _child: Child,                           // 进程
    stdin: ChildStdin,                        // 输入
    stdout_lines: Lines<BufReader<ChildStdout>>,  // 输出
}

// 通信：JSON-RPC over stdio（每行一个 JSON）
```

### 12.5 HTTP Transport

```rust
HttpTransport {
    url: String,
    client: reqwest::Client,
    session_id: Option<String>,  // 保持会话状态
}

// 使用 Mcp-Session-Id header 保持状态
```

### 12.6 SSE Transport

```rust
SseTransport {
    sse_url: String,                    // SSE 端点
    message_url: Option<String>,        // POST 端点
    reader_task: JoinHandle<()>,        // SSE 读取任务
}

// POST 请求发送， SSE 流接收
```

### 12.7 MCP Server 结构

```rust
pub struct McpServer {
    inner: Arc<Mutex<McpServerInner>>,
}

struct McpServerInner {
    config: McpServerConfig,
    transport: Box<dyn McpTransportConn>,
    next_id: AtomicU64,
    tools: Vec<McpToolDef>,  // 发现到的 tools
}
```

### 12.8 Tool 调用

```rust
mcp_server.call_tool(tool_name, arguments) → JSON-RPC tools/call
```

### 12.9 超时控制

| 操作 | 默认超时 | 可配置 | 最大值 |
|------|---------|--------|--------|
| 初始化/列表 | 30s | ✗ | 30s |
| 工具调用 | 180s | ✓ | 600s |

### 12.10 配置示例

```toml
[[mcp_servers]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["mcp-server-filesystem"]

[[mcp_servers]]
name = "github"
transport = "http"
url = "https://mcp.github.com/mcp"
headers = { Authorization = "Bearer xxx" }
tool_timeout_secs = 300
```

---


## 13. 配置层

### 13.1 Provider（连接信息）+ Model（能力 + 定价）

每个 provider 的 model 声明自包含——capabilities 写在每个 provider 下，不搞全局 model 定义 + 引用。清晰比 DRY 重要。

```toml
# ═══ Provider：纯连接信息 ═══

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key = "${OPENAI_API_KEY}"
auth_style = "bearer"

[providers.azure-openai]
base_url = "https://myresource.openai.azure.com/openai"
api_key = "${AZURE_OPENAI_API_KEY}"
auth_style = "azure"

[providers.minimax]
base_url = "https://api.minimaxi.com/v1"
api_key = "${MINIMAX_API_KEY}"
auth_style = "bearer"
chat.merge_system_into_user = true    # provider 级别的请求行为

[providers.glm]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = "${GLM_API_KEY}"
auth_style = "zhipu-jwt"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "${DEEPSEEK_API_KEY}"

[providers.elevenlabs]
base_url = "https://api.elevenlabs.io/v1"
api_key = "${ELEVENLABS_API_KEY}"

[providers.jina]
base_url = "https://api.jina.ai/v1"
api_key = "${JINA_API_KEY}"

# ═══ Model 声明：挂在 provider 下，自包含 ═══

# --- openai 的 models ---

[providers.openai.models.gpt-4o]
capabilities = ["chat", "vision", "native_tools"]
max_output_tokens = 16384
[providers.openai.models.gpt-4o.pricing]
input_per_million = 2.50
output_per_million = 10.00
cached_input_per_million = 1.25

[providers.openai.models.gpt-4o-mini]
capabilities = ["chat", "vision", "native_tools"]
max_output_tokens = 16384
[providers.openai.models.gpt-4o-mini.pricing]
input_per_million = 0.15
output_per_million = 0.60

[providers.openai.models.dall-e-3]
capabilities = ["image_generation"]
[providers.openai.models.dall-e-3.pricing]
per_image_standard = 0.040
per_image_hd = 0.080

[providers.openai.models.tts-1]
capabilities = ["text_to_speech"]

[providers.openai.models.whisper-1]
capabilities = ["speech_to_text"]

[providers.openai.models.text-embedding-3-small]
capabilities = ["embedding"]

# --- azure-openai 的 models（同一个 gpt-4o，不同定价）---

[providers.azure-openai.models.gpt-4o]
capabilities = ["chat", "vision", "native_tools"]
max_output_tokens = 16384
[providers.azure-openai.models.gpt-4o.pricing]
input_per_million = 2.20
output_per_million = 8.80

# --- minimax 的 models ---

[providers.minimax.models.minimax-m2.7]
capabilities = ["chat", "vision"]
max_output_tokens = 32768
[providers.minimax.models.minimax-m2.7.pricing]
input_per_million = 1.00
output_per_million = 2.00

[providers.minimax.models.minimax-tts]
capabilities = ["text_to_speech"]

[providers.minimax.models.minimax-img]
capabilities = ["image_generation"]

# --- glm 的 models ---

[providers.glm.models.glm-4-plus]
capabilities = ["chat", "vision", "native_tools"]
max_output_tokens = 4096
[providers.glm.models.glm-4-plus.pricing]
input_per_million = 0.70
output_per_million = 0.70

[providers.glm.models.glm-4-flash]
capabilities = ["chat", "native_tools"]
max_output_tokens = 4096
[providers.glm.models.glm-4-flash.pricing]
input_per_million = 0.10
output_per_million = 0.10

[providers.glm.models.cogview-4]
capabilities = ["image_generation"]

[providers.glm.models.cogvideox]
capabilities = ["video_generation"]

[providers.glm.models.embedding-3]
capabilities = ["embedding"]

# --- deepseek ---

[providers.deepseek.models.deepseek-chat]
capabilities = ["chat"]
[providers.deepseek.models.deepseek-chat.pricing]
input_per_million = 0.14
output_per_million = 0.28

# --- elevenlabs（只有 TTS）---

[providers.elevenlabs.models.eleven-turbo]
capabilities = ["text_to_speech"]

# --- jina（只有 embedding）---

[providers.jina.models.jina-embeddings-v3]
capabilities = ["embedding"]

# ═══ 路由：选 model，系统自动找 provider ═══

[routing.chat]
strategy = "fallback"
models = ["minimax-m2.7", "gpt-4o", "glm-4-plus"]

[routing.image_generation]
strategy = "fixed"
models = ["dall-e-3"]

[routing.text_to_speech]
strategy = "fixed"
models = ["minimax-tts"]

[routing.search]
strategy = "fallback"
providers = ["tavily", "perplexity"]

[routing.embedding]
strategy = "fixed"
models = ["jina-embeddings-v3"]

[defaults]
model = "minimax-m2.7"
```

### 13.2 配置结构体

```rust
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<SecretString>,
    pub auth_style: AuthStyle,
    pub extra_headers: HashMap<String, String>,
    pub timeout_secs: Option<u64>,
    // provider 级别的请求行为（所有走 OpenAI 协议的 model 共享）
    pub chat_config: Option<OpenAiChatConfig>,
    // 该 provider 提供的 model 列表
    pub models: HashMap<String, ModelConfig>,
}

pub struct ModelConfig {
    pub name: String,
    pub capabilities: Vec<Capability>,
    pub chat_features: Option<ChatFeatures>,  // Chat 子能力细节
    pub max_output_tokens: Option<u32>,
    pub pricing: Option<Pricing>,
}

pub struct ChatFeatures {
    pub vision: bool,
    pub audio_input: bool,
    pub video_input: bool,
    pub native_tools: bool,
    pub max_image_size: Option<u64>,
    pub supported_image_formats: Vec<String>,
}

pub struct OpenAiChatConfig {
    pub merge_system_into_user: bool,
    pub responses_fallback: bool,
    pub user_agent: Option<String>,
    pub reasoning_effort: Option<String>,
}

pub struct RoutingConfig {
    pub chat: Option<RouteEntry>,
    pub image_generation: Option<RouteEntry>,
    pub text_to_speech: Option<RouteEntry>,
    pub speech_to_text: Option<RouteEntry>,
    pub video_generation: Option<RouteEntry>,
    pub web_search: Option<RouteEntry>,
    pub embedding: Option<RouteEntry>,
}

pub struct RouteEntry {
    pub strategy: RoutingStrategy,
    pub models: Vec<String>,      // 选 model（chat、image_gen、tts、embedding 等）
    pub providers: Vec<String>,   // 选 provider（search 等直接路由 provider 的场景）
}
```

---

## 14. 工厂函数（从 3800 行 match 简化）

```rust
/// 按 base_url 域名匹配，构建对应的 provider 实例
/// 返回的 ProviderInstance 实现了其支持的所有能力 trait
fn build_provider_instance(config: &ProviderConfig) -> ProviderInstance {
    let descriptor = ProviderDescriptor::from(config);
    let url = config.base_url.to_lowercase();

    if url.contains("minimaxi.com") {
        ProviderInstance::Minimax(Minimax::new(descriptor))
    } else if url.contains("bigmodel.cn") {
        ProviderInstance::Glm(Glm::new(descriptor))
    } else if url.contains("deepseek.com") {
        ProviderInstance::DeepSeek(DeepSeek::new(descriptor))
    } else if url.contains("anthropic.com") {
        ProviderInstance::Anthropic(Anthropic::new(descriptor))
    } else if url.contains("elevenlabs.io") {
        ProviderInstance::ElevenLabs(ElevenLabs::new(descriptor))
    } else if url.contains("jina.ai") {
        ProviderInstance::Jina(Jina::new(descriptor))
    } else {
        // 默认 OpenAI 兼容——大多数新 provider 直接用
        ProviderInstance::OpenAiCompatible(OpenAiCompatible::new(descriptor))
    }
}

/// ProviderInstance 是一个枚举包装，按能力向下转型
/// ServiceRegistry 根据 config.models 中声明的 capabilities，
/// 将 ProviderInstance 转为对应能力的 Box<dyn XxxProvider>
impl ServiceRegistry {
    /// 注册时根据 provider 的 capabilities 声明，分别存入对应的能力存储
    fn register(&mut self, config: ProviderConfig) {
        let instance = build_provider_instance(&config);
        
        for (model_id, model) in &config.models {
            for cap in &model.capabilities {
                match cap {
                    Capability::Chat => self.chat_handles.insert(/* ... */),
                    Capability::Search => self.search_handles.insert(/* ... */),
                    Capability::Embedding => self.embedding_handles.insert(/* ... */),
                    Capability::ImageGeneration => self.image_handles.insert(/* ... */),
                    Capability::TextToSpeech => self.tts_handles.insert(/* ... */),
                    Capability::SpeechToText => self.stt_handles.insert(/* ... */),
                    Capability::VideoGeneration => self.video_handles.insert(/* ... */),
                    _ => {}
                }
            }
        }
    }
}
```

用 base_url 匹配比名字匹配更稳定——用户给 provider 取什么名字都不影响实现选择。

加新 provider：跟标准 OpenAI 格式一致 → 走 `_` 默认分支；有差异 → 加一行 match。

---

## 15. 对现有代码的改造映射

### 15.1 Crate 级别改动

| Crate | 改动 |
|-------|------|
| **myclaw-api** | `Provider` trait 扩展——新增 generate_image、synthesize、embed 等方法；新增 `Capability` 枚举、`ServiceRegistry` |
| **myclaw-providers** | `compatible.rs` → 每个 provider 一个文件（openai.rs、minimax.rs 等），实现多个 capability trait；`router.rs` 删除；`lib.rs` 大幅简化 |
| **myclaw-runtime** | `Agent.provider` → `Arc<ServiceRegistry>`；agent loop 调用 registry 获取能力 |
| **myclaw-channels** | `orchestrator/mod.rs` provider 相关字段 → registry |
| **myclaw-config** | `ProviderConfig` 加 `models` 字段；新增 `ModelConfig`、`RoutingConfig` |
| **myclaw-tools** | `web_search_tool` 等查询 registry 获取能力 |
| **myclaw-memory** | 不变 |

### 15.2 文件级别改动

| 现有文件 | 动作 | 说明 |
|---------|------|------|
| `api/provider.rs` | 扩展 | `Provider` trait 新增 generate_image、synthesize、embed 等方法 |
| `api/capabilities.rs` | 新增 | `Capability` 枚举 |
| `api/registry.rs` | 新增 | `ServiceRegistry`、`Provider`、`ProviderModel` |
| `providers/mod.rs` | 重构 | 共享网络函数（http_stream、http_post）+ 工厂函数 |
| `providers/openai.rs` | 新增 | OpenAi provider：Chat + ImageGen + TTS + STT + Embedding |
| `providers/minimax.rs` | 新增 | Minimax provider：Chat + ImageGen + TTS |
| `providers/kimi.rs` | 新增 | Kimi provider：Chat |
| `providers/glm.rs` | 重写 | Glm provider：Chat + ImageGen + VideoGen + Embedding（替换旧死代码） |
| `providers/deepseek.rs` | 新增 | DeepSeek provider：Chat |
| `providers/anthropic.rs` | 迁移 | Anthropic provider：Chat（独立协议） |
| `providers/gemini.rs` | 迁移 | Gemini provider：Chat（独立协议） |
| `providers/elevenlabs.rs` | 新增 | ElevenLabs provider：TTS |
| `providers/jina.rs` | 新增 | Jina provider：Embedding |
| `providers/router.rs` | 删除 | 路由移入 ServiceRegistry |
| `providers/reliable.rs` | 保留 | 变成 ServiceRegistry 内部执行策略 |
| `providers/lib.rs` | 大幅简化 | 工厂函数从 3800 行 match 简化为按 capabilities 构建 |
| `runtime/agent/agent.rs` | 改 | `provider: Box<dyn Provider>` → `registry: Arc<ServiceRegistry>`，按需获取 `dyn ChatProvider` 等 |
| `runtime/agent/loop_.rs` | 改 | `provider.chat(req)` → `let (provider, model_id) = registry.get_chat_provider()?; provider.chat(ChatRequest { model: model_id, ..req })` |
| `channels/orchestrator/mod.rs` | 改 | provider 管理 → registry |
| `config/schema.rs` | 改 | ProviderConfig 加 models；新增 ModelConfig、RoutingConfig |
| `tools/web_search_tool.rs` | 改 | 查询 registry 获取 WebSearch 能力 |

### 15.3 可复用的部分（不改动）

| 组件 | 说明 |
|------|------|
| Tool trait | 简洁统一，70+ 实现不动 |
| Channel trait | 25+ 实现不动 |
| Memory trait | 多后端不动 |
| RuntimeAdapter | 平台抽象不动 |
| ReliableProvider | failover 策略保留 |
| SecurityPolicy | 细粒度控制不动 |
| MCP client | 标准协议支持不动 |
| normalize.rs | think tag 处理不动 |

---

## 14. 重构路径

> ✅ **2026-04-29：所有 Phase 已完成**

### Phase 1：不改变 trait，修复已知问题 ✅
- ✅ 删除 `glm.rs` 死代码
- ✅ `UsageInfo` 加 `completion_tokens_details`，修复 MiniMax reasoning_tokens 丢失
- ✅ 验证现有 provider 的 token usage 解析

### Phase 2：引入 Provider + Model 两层 + ServiceRegistry ✅
- ✅ 配置从 `provider → capabilities` 改为 `provider → models → capabilities`
- ✅ 新增 `Provider`、`ProviderModel` 结构体
- ✅ 新增 `ServiceRegistry`，按 model 查找 + provider 连接
- ✅ 路由从选 provider 改为选 model

### Phase 3：Provider trait 拆分为独立能力 trait + 每个 provider 一个 struct ✅
- ✅ `Provider` trait 拆分为 `ChatProvider`、`SearchProvider`、`EmbeddingProvider`、`ImageGenerationProvider`、`TtsProvider`、`SttProvider`、`VideoGenerationProvider`
- ✅ 每个 provider 一个 struct，实现其支持的能力 trait
- ✅ `providers/mod.rs`（共享网络函数）+ 每个 provider 独立文件
- ✅ `router.rs` 删除
- ✅ `lib.rs` 工厂简化为一个 match

### Phase 4：工具系统接入 Capability + 策略化路由 ✅
- ✅ `web_search_tool` 通过 registry 获取 SearchProvider 能力
- ✅ MCP 工具通过 registry 获取能力
- ✅ RoutingStrategy 接入（fallback、cheapest、fastest）
- ✅ 编排层支持策略化路由


---

## 17. 启动与关闭

### 17.1 启动顺序

```
main()
  │
  ├── 1. 配置加载 (ConfigLoader)
  │
  ├── 2. 初始化 Infrastructure
  │       ├── ServiceRegistry
  │       ├── MemoryStorage
  │       └── Providers
  │
  ├── 3. 初始化 Domain
  │       ├── SessionManager
  │       └── MemoryManager
  │
  ├── 4. 初始化 Application
  │       ├── AgentLoop (注入 LoopBreakerAgent)
  │       ├── SkillsManager
  │       └── McpManager
  │
  ├── 5. 启动 Orchestrator
  │       └── 启动所有启用的 Channels
  │
  └── 6. 主循环运行
          └── 等待 shutdown 信号
```

### 17.2 关闭流程（Graceful Shutdown）

```
收到 SIGTERM / SIGINT
  │
  ├── 1. 停止接收新消息
  ├── 2. 等待处理中请求完成
  ├── 3. 关闭 Channels
  ├── 4. 持久化状态
  │       ├── Session 历史写入 storage
  │       └── Memory autosave
  └── 5. 退出进程
```

### 17.3 配置加载

```toml
# 两种加载方式

# 1. Init Load（启动时完整加载）
config = ConfigLoader::from_file("config.toml").load()?

# 2. Runtime Watch（运行时监听文件变化）
loader.watch(|change| {
    // ConfigChange::Full 完整替换
    // ConfigChange::Partial 部分更新
})
```

**支持运行时部分更新的参数**：

| 参数 | 说明 |
|------|------|
| Provider API keys | OpenAI key 刷新 |
| Provider model selection | 切换模型 |
| Channel enabled/disabled | 开关 channel |
| Memory settings | embedding 开关等 |
| Agent autonomy level | 安全级别调整 |
| Tool timeout | 超时配置 |
| Loop breaker thresholds | 熔断参数 |
| MCP servers | 增删 MCP server |

---

## 18. 错误处理

### 18.1 错误分类

| 类别 | 来源 | 示例 |
|------|------|------|
| Provider | LLM API | 网络超时、限流、模型不可用 |
| Tool | 工具执行 | 命令失败、超时、权限拒绝 |
| Channel | 消息通道 | 连接断开、webhook 验证失败 |
| Storage | 存储层 | SQLite 错误、Embedding 服务不可用 |
| Validation | 输入校验 | 非法参数、格式错误 |

### 18.2 错误处理策略

| 错误类型 | 处理方式 |
|---------|---------|
| **Provider** | 由 ServiceRegistry fallback 处理 |
| **Tool** | 返回错误给 LLM，让它决定 |
| **Channel** | 自动重连 |
| **Storage** | 降级到内存模式 |
| **Timeout** | 终止操作，返回超时错误 |

### 18.3 Provider 错误

```rust
enum ProviderError {
    Network(String),
    RateLimited { retry_after: u64 },
    ModelUnavailable(String),
    AuthFailed(String),
    Timeout,
}

impl ProviderError {
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::Network(_) | 
            ProviderError::RateLimited { .. } |
            ProviderError::Timeout
        )
    }
}
```

### 18.4 Tool 错误

```rust
enum ToolError {
    ExecutionFailed(String),
    Timeout,
    PermissionDenied,
    NotFound,
}

impl ToolError {
    fn user_message(&self) -> String {
        match self {
            ToolError::ExecutionFailed(msg) => format!("执行失败: {}", msg),
            ToolError::Timeout => "执行超时".to_string(),
            ToolError::PermissionDenied => "权限不足".to_string(),
            ToolError::NotFound => "工具不存在".to_string(),
        }
    }
}
```

### 18.5 Channel 错误

```rust
enum ChannelError {
    ConnectionFailed(String),
    Disconnected,
    WebhookInvalid,
    MessageSendFailed,
}
```

**策略**：自动重连，后台运行，不阻塞消息处理。

### 18.6 Storage 降级

```rust
struct MemoryManager {
    storage: Option<MemoryStorage>,  // None = 降级到内存
    in_memory: InMemoryStore,
}
```

Storage 失败时降级到内存模式，不丢失数据。

---

## 19. 日志

### 19.1 日志配置

```toml
[logging]
level = "INFO"
path = "/var/log/zeroclaw/zeroclaw.log"

[logging.rotation]
max_size_mb = 100
max_files = 10

[logging.levels]
default = "INFO"
provider = "DEBUG"
tool = "DEBUG"
channel = "INFO"
```

### 19.2 日志输出

| 输出 | 说明 |
|------|------|
| **stdout** | 开发环境 |
| **文件** | 生产环境 |

### 19.3 日志格式（普通文本）

```
2026-04-26T14:30:00.123+08:00 INFO  Tool executed tool=calculator duration_ms=12 session_id=wechat:o9cq80zXXX
```

### 19.4 日志内容清单

| 事件 | 级别 | 字段 |
|------|------|------|
| 启动/关闭 | INFO | version, config_path |
| Provider 调用成功 | DEBUG | trace_id, provider, model, duration_ms |
| Provider 调用失败 | ERROR | trace_id, provider, error |
| Tool 执行成功 | DEBUG | trace_id, tool_name, duration_ms |
| Tool 执行失败 | WARN | trace_id, tool_name, error |
| Session 创建 | INFO | session_id, channel, user |
| Channel 消息收到 | DEBUG | session_id, channel, direction=in |
| Channel 消息发送 | DEBUG | session_id, channel, direction=out |
| Fallback 触发 | WARN | trace_id, from, to, reason |
| Config 变更 | INFO | patch_summary |

---

## 20. 安全策略

### 20.1 Channel 认证

| Channel | 认证方式 |
|---------|---------|
| WeChat | HTTP Signature 验证 |
| Telegram | Bot Token 验证 |
| Discord | Bot Token + Webhook Secret |
| Slack | Signing Secret |

```rust
pub trait Channel: Send + Sync {
    fn verify(&self, request: &HttpRequest) -> Result<(), AuthError>;
}
```

### 20.2 Tool 危险命令控制

```toml
[security]
# 黑名单模式：默认允许，列出危险命令禁止
dangerous_commands = [
    "rm -rf /",
    "dd if=/dev/zero",
    "mkfs",
    ":(){ :|:& };:",
    "curl | sh",
    "wget | sh",
]
```

| 配置 | 说明 |
|------|------|
| 默认 | 允许所有命令 |
| 黑名单 | 只禁止列出的命令 |

### 20.3 Skill 安全审计

```rust
// 安装时自动执行
impl SkillsManager {
    pub async fn install(&self, skill: Skill) -> Result<()> {
        let audit_result = self.audit(&skill)?;
        
        if audit_result.has_dangerous {
            return Err(SkillsManagerError::AuditFailed(audit_result));
        }
        
        self.install_unchecked(skill).await
    }
}
```

| 审计结果 | 行为 |
|---------|------|
| 无危险 | 自动安装 |
| 有危险命令 | 拒绝安装，记录日志 |

### 20.4 API Key 安全存储

```toml
# 从环境变量读取
[providers.openai]
api_key = "env:OPENAI_API_KEY"

# 从安全文件读取
[providers.anthropic]
api_key = "file:/run/secrets/anthropic_key"
```

### 20.5 Session 数据隔离

```rust
pub struct Session {
    history: Vec<ChatMessage>,       // 只属于这个 session
    private_memory: MemoryStore,     // 只属于这个 session
}

pub enum MemoryNamespace {
    Shared(String),     // 跨 session 共享
    Private(String),   // 单 session 私有
}
```

---

## 21. 设计边界

### 21.1 支持的范围

| 维度 | 结论 |
|------|------|
| **用户数** | 单用户 |
| **进程数** | 单进程 |
| **部署** | 单体部署 |

### 21.2 不支持的特性

| 特性 | 原因 |
|------|------|
| 多租户/用户认证 | 单用户 |
| 分布式 Agent | 单进程 |
| 跨进程通信 | 单进程 |
| 水平扩展 | 单进程 |


---

---

## 22. Scheduler（定时任务）

### 22.1 功能概述

Scheduler 管理定时任务的创建、调度和执行。

### 22.2 任务类型

| 类型 | 说明 | 示例 |
|------|------|------|
| Cron | 标准 cron 表达式 | `0 9 * * 1-5` |
| One-shot | RFC 3339 时间戳 | `2025-01-15T14:00:00Z` |
| Interval | 固定间隔（毫秒） | `60000`（每分钟） |

### 22.3 任务分类

| 类型 | 说明 | 触发方式 |
|------|------|---------|
| Agent | LLM 执行 | AgentLoop 处理 |
| Shell | 命令执行 | 系统 shell |
| Notification | 推送通知 | Channel 发送 |

### 22.4 配置

```toml
[scheduler]
enabled = true
timezone = "Asia/Shanghai"
max_concurrent = 10
```

### 22.5 CLI 命令

```bash
zeroclaw cron list                          # 列出所有任务
zeroclaw cron add '0 9 * * 1-5' 'Good morning' --agent
zeroclaw cron add '*/30 * * * *' 'Check health' --agent
zeroclaw cron add-at 2025-01-15T14:00:00Z 'Reminder' --agent
zeroclaw cron add-every 60000 'Ping'
zeroclaw cron pause <task-id>
zeroclaw cron resume <task-id>
zeroclaw cron remove <task-id>
```

### 22.6 实现

```rust
// application/myclaw-runtime/src/cron/scheduler.rs
// Scheduler 保持在 myclaw-runtime 内，不独立为单独 crate

pub struct Scheduler {
    registry: Arc<ServiceRegistry>,
    agent_loop: Arc<Mutex<dyn AgentLoop>>,
    channel_sender: Sender<ChannelMessage>,
}

impl Scheduler {
    pub async fn add(&self, spec: TaskSpec) -> Result<TaskId> {
        // 解析 cron 表达式
        let schedule = Schedule::from_str(&spec.expression)?;
        
        // 创建任务
        let task = Task {
            id: TaskId::new(),
            spec,
            schedule,
            status: TaskStatus::Active,
        };
        
        // 调度执行
        self.schedule_task(task).await
    }
}
```

---

## 23. Self-check（自检与诊断）

### 23.1 功能概述

Self-check 验证 MyClaw 安装完整性，包括配置、网络、存储等。

### 23.2 检查项

| 检查项 | 说明 | 失败处理 |
|--------|------|---------|
| Config | 配置文件存在且有效 | 阻止启动 |
| Provider | Provider API 连接测试 | 警告 |
| Memory | Memory 读写测试 | 警告 |
| Channel | Channel 连接测试 | 警告 |
| Storage | 存储读写测试 | 警告 |
| Network | 网络连通性 | 警告 |

### 23.3 CLI 命令

```bash
zeroclaw doctor                    # 完整诊断
zeroclaw doctor --quick            # 快速检查（无网络）
zeroclaw self-test                 # 运行自检
zeroclaw self-test --quick         # 快速自检
```

### 23.4 Doctor 子命令

```bash
zeroclaw doctor run                # 运行诊断
zeroclaw doctor status             # 显示组件状态
zeroclaw doctor fix                # 自动修复问题
```

### 23.5 实现

```rust
// application/myclaw-runtime/src/doctor/mod.rs

pub struct Doctor {
    config: Config,
    registry: Arc<ServiceRegistry>,
    storage: Arc<dyn Storage>,
}

impl Doctor {
    pub async fn run_all(&self) -> Result<DoctorReport> {
        let mut report = DoctorReport::new();
        
        // 并行运行所有检查
        let results = futures::join!(
            self.check_config(),
            self.check_providers(),
            self.check_memory(),
            self.check_channels(),
        );
        
        for result in results {
            report.add_result(result);
        }
        
        Ok(report)
    }
}

pub struct DoctorReport {
    pub results: Vec<CheckResult>,
    pub passed: bool,
}

impl DoctorReport {
    pub fn summary(&self) -> String {
        // 格式化输出
    }
}
```



## 24. 关键设计决策记录

| # | 决策 | 理由 |
|---|------|------|
| 1 | Provider 按能力拆分为独立 trait（ChatProvider、SearchProvider、EmbeddingProvider 等） | 类型安全，按需注入，编译时检查。Jina 只实现 EmbeddingProvider，ElevenLabs 只实现 TtsProvider |
| 2 | "openai compatible" 是 Chat 的协议，不是全局属性 | Web search、embedding 有自己的协议，跟 openai 格式无关 |
| 3 | 每个 provider 独立实现 parse_usage，精确匹配自己的 API | 代码即文档，各写各的，清晰比 DRY 重要 |
| 4 | 网络层作为共享基础设施（http_stream 等函数），不是 provider | 避免重复 HTTP/SSE 代码，但不影响解析独立性 |
| 5 | 请求侧差异配置驱动 | 行为开关（merge_system、auth_style）可配置，不需要写代码 |
| 6 | 协议差异是唯一需要独立实现的分界线 | openai vs anthropic vs gemini 是根本不同的 API |
| 7 | ChatProvider.chat 只保留流式接口，非流式是便捷方法 | 流式更通用、网络可靠性更好（保活、快速故障感知）、实现更简单。详见 4.2 节 |
| 8 | 能力拆分为独立 trait，Chat 子能力是配置标记 | Vision、Tool Calling 在 Chat 消息流中发生，不是独立 API；Image Gen、TTS 等是完全独立的 API 和独立 trait |
| 9 | ServiceRegistry 按 capability 路由，不按 provider | 路由说"用 gpt-4o"，系统知道从哪个 provider 拿、怎么连 |
| 10 | 每个 provider 一个 struct，按支持的能力实现对应 trait | OpenAI 实现 6 个 trait，DeepSeek 只实现 ChatProvider |
| 11 | 工厂函数用 base_url 做匹配 | 比名字匹配更稳定，用户给 provider 取什么名字都不影响 |
| 12 | 每个能力定义自己的 Usage 结构，不要通用 TokenUsage | 各能力的用量单位根本不同（tokens / 图片数 / 字符数 / 时长），统一计费是成本计算层的事 |
| 13 | Model 声明挂在 Provider 下面，每个 provider 自包含 | 同一个 model 在不同 provider 下定价可能不同；清晰比 DRY 重要 |
| 14 | Provider 是纯连接层，Model 是能力声明层 | Provider = base_url + api_key + auth；Model = capabilities + pricing。正交的两个维度 |
| 15 | Tools 在 Agent 编排层，Provider 不知道 tools | Provider 只管"提供能力"，Tool 知道调用哪个 Provider 来完成任务 |
| 16 | Search 是 Provider 能力（SearchProvider trait），Web Search Tool 调用 Provider 的 search 能力 | Perplexity 等有独立 search API，Tavily 等通过 SearchProvider trait 统一接口；Tool 层调用 registry.get_search_provider() |
| 17 | ServiceRegistry 是统一路由层，按 capability 路由 | Agent Loop 和 Tools 都通过 `get_chat_provider()` / `get_search_provider()` 等方法获取对应能力的 provider |
| 18 | Scheduler 保持在 myclaw-runtime，不独立为单独 crate | 现有 cron 功能复杂度不高，独立 crate 收益不大，保持简单 |