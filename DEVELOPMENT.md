# MyClaw 开发计划

> 基于 `docs/zeroclaw-new-architecture.md` 设计文档重写 ZeroClaw

---

## 一、目标架构概述

### 五层架构

| 层次 | 职责 | 包含内容 |
|------|------|---------|
| **Orchestration** | Channel 管理、消息路由 | Orchestrator |
| **Interface** | 接入协议适配 | Channel Adapters（编译时可选） |
| **Application** | 业务编排 | AgentLoop, SkillsManager, McpManager, SystemPromptBuilder |
| **Domain** | 业务逻辑核心 | Session, Memory, Provider trait, Tool trait |
| **Infrastructure** | 技术实现 | ServiceRegistry, Provider 实现, Storage, Tool 实现, LoopBreakerAgent |

### 核心拆分原则

- **Provider 按能力拆分**：Chat / Search / Embedding / Image / TTS / STT / Video 独立 trait
- **Channel 编译时可选**：不需要的 channel 不编译
- **Memory 独立模块**：`claw-memory` + `claw-memory-storage`
- **Loop Breaker 装饰器**：可插拔的限流/熔断机制

---

## 二、当前代码分析

### 当前 `src/` 结构

```
src/
├── agent/          # Agent 逻辑 (混合了多层次)
├── channels/       # Channel 实现
├── config/         # 配置管理
├── memory/         # Memory 实现 (当前混在一起)
├── providers/      # Provider 实现
├── tools/          # Tools 实现
├── lib.rs          # 主入口
└── main.rs         # CLI 入口
```

### 当前 `crates/` 结构

```
crates/
├── zeroclaw-api           # API 层
├── claw-channels      # Channel 聚合
├── zeroclaw-config        # 配置
├── zeroclaw-gateway       # 网关
├── zeroclaw-infra         # 基础设施
├── claw-memory        # Memory 模块
├── claw-providers     # Provider 聚合
├── claw-runtime       # 运行时
├── claw-tools         # Tools 聚合
├── zeroclaw-tui           # TUI
└── ... (其他)
```

---

## 三、目标目录结构

> 完全基于 `zeroclaw-new-architecture.md` 文档各节定义

```
├── orchestration/                         # Orchestration Layer（编排层）
│   └── claw-orchestrator/           # Orchestrator
│       └── src/lib.rs                   # 启动 Channels、消息路由
│
├── interface/                           # Interface Layer（接口层）
│   └── claw-channels/               # Channel Adapters（编译时可选）
│       └── src/
│           ├── lib.rs                   # #[cfg(feature = "...")] 条件编译
│           ├── wechat/                  # zeroclaw-channel-wechat（optional）
│           ├── telegram/               # zeroclaw-channel-telegram（optional）
│           ├── discord/                # zeroclaw-channel-discord（optional）
│           └── slack/                  # zeroclaw-channel-slack（optional）
│
├── application/                         # Application Layer（应用层）
│   ├── claw-runtime/               # AgentLoop、SkillsManager、McpManager、SystemPromptBuilder
│   │   └── src/
│   │       ├── agent/                  # AgentLoop 实现
│   │       ├── skills/                 # SkillsManager
│   │       ├── mcp/                    # McpManager
│   │       ├── prompt/                 # SystemPromptBuilder
│   │       ├── cron/                   # Scheduler（保持在 runtime 内，不独立 crate）
│   │       └── doctor/                 # Self-check 诊断
│   └── zeroclaw-cli/                   # CLI 入口（main.rs）
│
├── domain/                              # Domain Layer（核心域）
│   ├── claw-session/               # Session 领域
│   │   └── src/session.rs
│   │
│   └── claw-memory/                # Memory 领域
│       └── src/
│           ├── memory.rs               # Memory trait
│           ├── shared.rs               # Shared Memory 逻辑
│           └── private.rs              # Private Memory 逻辑
│
├── infrastructure/                      # Infrastructure Layer（基础设施）
│   ├── claw-registry/              # ServiceRegistry（能力路由中心）
│   │   └── src/lib.rs                  # get_chat_provider() 等方法
│   ├── claw-providers/             # Provider 实现
│   │   └── src/
│   │       ├── mod.rs                  # 共享网络函数 + 工厂函数
│   │       ├── openai.rs              # OpenAIProvider（Chat + Search + Embedding）
│   │       ├── anthropic.rs           # AnthropicProvider（Chat only）
│   │       ├── minimax.rs             # MinimaxProvider
│   │       ├── glm.rs                 # GLMProvider
│   │       ├── ollama.rs              # OllamaProvider（Chat + Embedding）
│   │       ├── jina.rs                # JinaProvider（Embedding）
│   │       ├── elevenlabs.rs          # ElevenLabsProvider（TTS）
│   │       ├── perplexity.rs          # PerplexityProvider（Search）
│   │       └── ...                    # 其他 Provider
│   ├── claw-memory-storage/        # Memory 存储实现
│   │   └── src/
│   │       ├── sqlite.rs              # SQLite 实现
│   │       ├── embedding.rs           # Embedding 计算
│   │       └── mod.rs
│   ├── claw-tools/                 # 内置 Tool 实现
│   │   └── src/
│   │       ├── mod.rs
│   │       └── ...                    # 70+ 内置工具
│   └── claw-mcp/                   # MCP Client 实现
│       └── src/
│           ├── lib.rs
│           └── transport/              # Stdio/HTTP/SSE
│
└── Cargo.toml                          # Workspace 根
```

### 各层依赖关系

```
Interface (Channels)
        ↓（依赖）
Orchestration (Orchestrator)
        ↓（依赖）
Application (AgentLoop, Skills, MCP, SystemPrompt)
        ↓（依赖）
Domain (Session, Memory, Provider trait, Tool trait)
        ↑（实现）
Infrastructure (Registry, Provider 实现, Storage, LoopBreakerAgent)
```

### Crate 映射表

| 文档中定义的 crate | 存放位置 | 说明 |
|-------------------|---------|------|
| `claw-orchestrator` | `orchestration/claw-orchestrator/` | 新建 |
| `claw-channels` | `interface/claw-channels/` | 编译时可选 feature |
| `zeroclaw-channel-wechat` | `interface/claw-channels/wechat/` | 可选依赖 |
| `zeroclaw-channel-telegram` | `interface/claw-channels/telegram/` | 可选依赖 |
| `zeroclaw-channel-discord` | `interface/claw-channels/discord/` | 可选依赖 |
| `zeroclaw-channel-slack` | `interface/claw-channels/slack/` | 可选依赖 |
| `claw-runtime` | `application/claw-runtime/` | 现有重构 |
| `claw-session` | `domain/claw-session/` | 新建 |
| `claw-memory` | `domain/claw-memory/` | 现有 crate 移入 Domain Layer |
| `claw-registry` | `infrastructure/claw-registry/` | 新建 |
| `claw-providers` | `infrastructure/claw-providers/` | 现有重构 |
| `claw-memory-storage` | `infrastructure/claw-memory-storage/` | 新建 |
| `claw-tools` | `infrastructure/claw-tools/` | 现有重构 |
| `claw-mcp` | `infrastructure/claw-mcp/` | 新建 |

---

## 四、模块清理清单

### 4.1 需要删除的模块

| 模块 | 路径 | 原因 |
|------|------|------|
| `agent/` | `src/agent/` | 逻辑混杂，需要按新架构拆分 |
| `zeroclaw-api` | `crates/zeroclaw-api/` | 旧 API 层，重写 |
| `zeroclaw-gateway` | `crates/zeroclaw-gateway/` | 重构为 Orchestrator |
| `zeroclaw-tui` | `crates/zeroclaw-tui/` | 独立应用，可选 |
| `zeroclaw-hardware` | `crates/zeroclaw-hardware/` | 与核心逻辑无关 |
| `robot-kit` | `crates/robot-kit/` | 实验性，可移除 |
| `aardvark-sys` | `crates/aardvark-sys/` | 硬件相关，暂不需要 |
| `zeroclaw-plugins` | `crates/zeroclaw-plugins/` | 旧插件系统，待定 |
| `zeroclaw-tool-call-parser` | `crates/zeroclaw-tool-call-parser/` | 功能已迁移 |
| `zeroclaw-macros` | `crates/zeroclaw-macros/` | 视需求保留 |

### 4.2 需要重构的模块

| 模块 | 当前状态 | 目标状态 |
|------|---------|---------|
| `claw-channels` | 聚合所有 channel | Interface Layer，编译时可选 |
| `claw-providers` | 混合实现 | 拆分 Provider trait + 各自独立实现 |
| `claw-memory` | 混在 src/memory | 独立 Domain Module |
| `zeroclaw-config` | 旧配置结构 | 新配置层 (ProviderConfig/ModelConfig) |
| `claw-runtime` | 运行时管理 | Application Layer 核心 |

### 4.3 新增模块

| 模块 | 职责 |
|------|------|
| `claw-orchestrator` | 消息路由、Channel 管理 |
| `claw-session` | Session Domain |
| `claw-memory-storage` | Memory 存储实现 (SQLite/PostgreSQL) |
| `claw-registry` | ServiceRegistry，能力路由 |
| `zeroclaw-channel-wechat` | 微信 Channel (独立 crate) |
| `zeroclaw-channel-telegram` | Telegram Channel (独立 crate) |

---

## 五、重构路径

> 严格按文档 Section 16 的四阶段执行

### Phase 1：修复问题，不改变 trait

- 删除 `glm.rs` 死代码
- `UsageInfo` 加 `completion_tokens_details`，修复 MiniMax reasoning_tokens 丢失
- 验证现有 provider 的 token usage 解析是否正确
- **风险：低。不改变外部接口。**

### Phase 2：引入 Provider + Model 两层 + ServiceRegistry

- 配置从 `provider → capabilities` 改为 `provider → models → capabilities`
- 新增 `Provider`、`ProviderModel` 结构体
- 新增 `ServiceRegistry`，按 model 查找 + provider 连接
- 路由从选 provider 改为选 model
- **风险：中。新旧可并存。**

### Phase 3：Provider trait 拆分为独立能力 trait + 每个 provider 一个 struct

- `Provider` trait 拆分为 `ChatProvider`、`SearchProvider`、`EmbeddingProvider`、`ImageGenerationProvider`、`TtsProvider`、`SttProvider`、`VideoGenerationProvider`
- 每个 provider 一个 struct，实现其支持的能力 trait
- `compatible.rs` → `providers/mod.rs`（共享网络函数）+ 每个 provider 独立文件
- `router.rs` 删除
- `lib.rs` 工厂简化为一个 match
- **风险：高。核心改造。**

### Phase 4：工具系统接入 Capability + 策略化路由

- `web_search_tool` 通过 registry 获取 SearchProvider 能力
- MCP 工具通过 registry 获取能力
- RoutingStrategy 接入（fallback、cheapest、fastest）
- 编排层支持策略化路由
- **风险：中。依赖 Phase 3 完成。**

---

## 六、配置层目标结构

> 来自文档 Section 12

```rust
// ProviderConfig: 单个 provider 的配置
pub struct ProviderConfig {
    pub name: String,
    pub provider_type: ProviderType,
    pub api_key: Secret,
    pub base_url: Option<String>,
    pub models: Vec<ModelConfig>,
}

// ModelConfig: 单个模型的配置
pub struct ModelConfig {
    pub model_id: String,
    pub capabilities: Vec<Capability>,
    pub endpoint: Option<String>,
    pub default_headers: Option<HashMap<String, String>>,
    // ... 其他模型级配置
}

// RoutingConfig: 路由配置
pub struct RoutingConfig {
    pub routes: Vec<RouteEntry>,
}

// RouteEntry: 单个路由规则
pub struct RouteEntry {
    pub hint: String,
    pub provider: String,
    pub models: Vec<String>,
}
```

---

## 七、关键决策点

1. **Channel 是否作为独立 crates？** 优点：减少不需要的依赖；缺点：版本管理复杂
2. **Memory 存储后端？** 当前 SQLite，可选 PostgreSQL
3. **是否保留 plugin 系统？** 当前 `zeroclaw-plugins` 状态
4. **TUI 作为独立应用？** 还是集成到主 binary

---

*最后更新: 2026-04-26*