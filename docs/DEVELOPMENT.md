# MyClaw 开发计划

> 全新开发 MyClaw，基于 `docs/architecture.md` 架构设计文档
>
> **最后更新：2026-04-27**

---

## 一、代码现状

### 1.1 已完成（可用的模块）

| 模块 | 文件 | 说明 |
|------|------|------|
| Capability traits | `domain/capability/src/` | Chat/Embedding/Image/TTS/Video/Search/Stt trait + 类型定义完整 |
| Registry + Routing | `infrastructure/registry/src/` | Chat 路由已通；Embedding/Image/TTS/Video 尚为 "not implemented" |
| Provider 实现 | `infrastructure/providers/src/` | OpenAI/MiniMax/GLM/Kimi/Anthropic 的 ChatProvider 已实现；OpenAI 的 Image/TTS/Embedding trait 定义了但需验证实现 |
| MCP Client | `infrastructure/mcp/src/` | Client/Protocol/Transport/Tool 完整实现，有测试 |
| Session 类型 | `domain/session/src/lib.rs` | ChatMessage/SessionMetadata/SessionQuery/Backend trait 定义了，实现待做 |
| Memory 类型 | `domain/memory/src/` | BackendKind/Profile 定义了，Memory trait 待实现 |
| Channel trait | `interface/channels/src/lib.rs` | trait 已定义；wechat/telegram/discord/slack 为空 mod |
| Orchestrator | `orchestration/orchestrator/src/` | stub struct |

### 1.2 未完成（需要开发的模块）

| 模块 | 优先级 | 说明 |
|------|--------|------|
| **AgentLoop** | P0 | 核心循环，依赖 Registry/ChatProvider |
| **SystemPromptBuilder** | P0 | AgentLoop 的前置依赖 |
| **SkillsManager** | P1 | Skills 加载/管理 |
| **ServiceRegistry 其他能力** | P1 | Embedding/Image/TTS/Video provider 注册 + 获取 |
| **Wechat Channel** | P0 | 当前主力通道 |
| **Telegram Channel** | P2 | 可选通道 |
| **SessionManager** | P1 | Session 生命周期管理 |
| **Memory trait + 实现** | P1 | 记忆系统核心 |
| **Memory-Storage** | P1 | SQLite/embedding 存储 |
| **Tools** | P1 | 70+ 内置工具实现 |
| **Orchestrator** | P1 | Channel 管理 + 消息路由 |
| **McpManager** | P1 | MCP Server 连接管理 |
| **Scheduler (Cron)** | P2 | 定时任务 |
| **Doctor/Self-check** | P2 | 自检诊断 |
| **Discord/Slack Channel** | P3 | 可选通道 |
| **LoopBreakerAgent** | P2 | 装饰器 |

---

## 二、详细功能清单

### Phase A：核心链路打通（最小可运行）

#### A.1 AgentLoop（应用层核心）
**文件**: `application/runtime/src/agent/`

- [ ] `mod.rs` — 定义 `AgentLoop` struct 和 `run(msg: &str) -> String` 方法
- [ ] 实现与 Registry 的集成：通过 `registry.get_chat_provider()` 获取 ChatProvider
- [ ] 实现消息历史管理（Session 的 ChatMessage 列表）
- [ ] 实现 Tool 调用循环：
  - [ ] 从 `SkillsManager` 获取可用工具列表
  - [ ] 构建 ToolSpec 列表传给 ChatProvider
  - [ ] 处理 `StreamEvent::ToolCallStart/Delta/End`
  - [ ] 执行 Tool，返回结果给 LLM 继续
- [ ] 实现 `StopReason` 处理（EndTurn/MaxTokens/ToolUse/Timeout）
- [ ] 实现非流式响应收集（`ChatResponse::from_stream`）
- [ ] 实现 thinking/reasoning 处理（`StreamEvent::Thinking`）
- [ ] 单元测试：模拟 ChatProvider + 2轮对话

#### A.2 SystemPromptBuilder
**文件**: `application/runtime/src/prompt/`

- [ ] `mod.rs` — `SystemPromptBuilder` struct
- [ ] [ ] 从配置加载 system prompt 模板
- [ ] [ ] 注入 session 上下文（用户信息、当前时间）
- [ ] [ ] 注入可用 tools 描述（从 SkillsManager 获取）
- [ ] [ ] 支持 `{{variable}}` 模板替换
- [ ] [ ] 实现 `build(session, tools) -> String` 方法

#### A.3 Wechat Channel（主力接口）
**文件**: `interface/channels/src/wechat/`

- [ ] `mod.rs` — WechatChannel struct 实现 `Channel` trait
- [ ] [ ] HTTP webhook 接收消息（`axum` 或 `actix-web`）
- [ ] [ ] 消息签名验证（HTTP Signature）
- [ ] [ ] 消息解析：`Text/Image/Voice/Event` 类型
- [ ] [ ] 将 ChannelMessage 转换为统一格式并发送给 Orchestrator
- [ ] [ ] 响应回传：接收 Orchestrator 的回复，推送至微信
- [ ] [ ] 实现 `Channel::name()` → "wechat"
- [ ] [ ] 实现 `Channel::send()` — 被动回复（接收回复目标，主动推送）
- [ ] 配置文件支持：`app_id`, `app_secret`, `token`, `encoding_aes_key`

#### A.4 Orchestrator（编排层）
**文件**: `orchestration/orchestrator/src/orchestrator.rs`

- [ ] 重写 `Orchestrator` struct：
  - [ ] 持有 `Vec<Box<dyn Channel>>` — 所有启用的 channel
  - [ ] 持有 `Arc<dyn AgentLoop>` — 应用层 agent
  - [ ] 持有 `Arc<ServiceRegistry>` — 路由
- [ ] 实现 `start()` — 启动所有 channel listeners
- [ ] 实现消息路由：
  - [ ] 从 Channel 接收 `ChannelMessage`
  - [ ] 创建/获取 Session
  - [ ] 调用 `AgentLoop::run()`
  - [ ] 通过原 Channel 发送响应
- [ ] 实现 graceful shutdown（收到 SIGTERM 停止接收新消息，等待处理完成）
- [ ] 实现 `ChannelMessage` / `SendMessage` 类型定义

---

### Phase B：存储与记忆系统

#### B.1 SessionManager
**文件**: `domain/session/src/`

- [ ] `mod.rs` — 补充完整
- [ ] [ ] 实现 `SessionManager` struct
- [ ] [ ] 实现 `get_or_create(session_key) -> Session`
- [ ] [ ] 实现 `save_session(session)` — 持久化
- [ ] [ ] 实现 `list_sessions(query: SessionQuery) -> Vec<SessionMetadata>`
- [ ] [ ] 实现 `delete_session(key)`
- [ ] 与 `infrastructure/memory-storage` 集成（SQLite 后端）

#### B.2 Memory trait + 实现
**文件**: `domain/memory/src/`

- [ ] `mod.rs` — 导出 Memory trait + BackendKind
- [ ] [ ] 定义 `Memory` trait（`store/recall/forget/export`）
- [ ] [ ] 定义 `MemoryNamespace`（Shared/Private）
- [ ] [ ] 定义 `MemoryEntry` 类型

#### B.3 Memory-Storage（存储实现）
**文件**: `infrastructure/memory-storage/src/`

- [ ] `mod.rs` — 导出 StorageBackend trait
- [ ] `sqlite.rs`：
  - [ ] SQLite 连接管理
  - [ ] Session 历史表（messages）
  - [ ] Memory 向量存储表（可选 embedding）
  - [ ] 实现 `StorageBackend` trait
- [ ] `embedding.rs`：
  - [ ] 调用 Registry 获取 EmbeddingProvider
  - [ ] 实现文本分块（chunker）
  - [ ] 实现向量存储和相似度搜索

---

### Phase C：Skills 系统

#### C.1 SkillsManager
**文件**: `application/runtime/src/skills/`

- [ ] `mod.rs` — `SkillsManager` struct
- [ ] [ ] `Skill` struct（`name`, `description`, `tools: Vec<Box<dyn Tool>>`）
- [ ] [ ] `load_skills_from_dir(path) -> Vec<Skill>` — 从目录扫描加载
- [ ] [ ] `install(skill)` — 安装 + 安全审计
- [ ] [ ] `audit(skill) -> AuditResult` — 检查危险命令
- [ ] [ ] `get_tool(name) -> Option<Box<dyn Tool>>` — 按名查找工具
- [ ] [ ] `all_tools() -> Vec<Box<dyn Tool>>` — 获取所有工具
- [ ] [ ] 工具注册表：tool_name → Skill 映射
- [ ] 内置 Skills：
  - [ ] `builtin_core` — shell/file/glob/content_search 等核心工具
  - [ ] `builtin_memory` — memory_store/recall/forget 等
  - [ ] `builtin_web` — web_search/web_fetch/http_request 等
  - [ ] `builtin_ai` — llm_task/claude_code/delegate/model_switch 等
  - [ ] `builtin_cron` — cron_add/list/remove 等
  - [ ] `builtin_session` — sessions_list/history/send 等
  - [ ] `builtin_misc` — calculator/weather/pushover/poll 等

#### C.2 内置 Tools 实现
**文件**: `infrastructure/tools/src/`

- [ ] `mod.rs` — 导出所有 Tool 实现
- [ ] 核心工具（对应 SkillsManager 的 builtin_core）：
  - [ ] `ShellTool` — 执行 shell 命令
  - [ ] `FileReadTool` — 读文件
  - [ ] `FileWriteTool` — 写文件
  - [ ] `FileEditTool` — 编辑文件（精准替换）
  - [ ] `GlobSearchTool` — 文件名搜索
  - [ ] `ContentSearchTool` — 文件内容搜索（regex）
- [ ] Memory 工具：
  - [ ] `MemoryStoreTool` — 调用 MemoryManager store
  - [ ] `MemoryRecallTool` — 调用 MemoryManager recall
  - [ ] `MemoryForgetTool` — 调用 MemoryManager forget
  - [ ] `MemoryExportTool` — 导出记忆
- [ ] Web 工具：
  - [ ] `WebSearchTool` — 通过 Registry 获取 SearchProvider
  - [ ] `WebFetchTool` — HTTP GET
  - [ ] `HttpRequestTool` — 通用 HTTP
- [ ] AI 工具：
  - [ ] `LlmTaskTool` — 调用 Registry ChatProvider
  - [ ] `ModelSwitchTool` — 切换模型
  - [ ] `DelegateTool` — 委托子 agent
- [ ] Cron 工具：
  - [ ] `CronAddTool` / `CronListTool` / `CronRemoveTool` 等
- [ ] 通信工具：
  - [ ] `PushoverTool` — 推送通知
  - [ ] `PollTool` — 发起投票
  - [ ] `AskUserTool` — 向用户提问
  - [ ] `ReactionTool` — 添加表情反应

---

### Phase D：Provider 能力补全

#### D.1 ServiceRegistry 全能力注册
**文件**: `infrastructure/registry/src/registry.rs`

- [ ] [ ] `register_embedding(provider, model_id)` — 存储 embedding provider
- [ ] [ ] `register_image(provider, model_id)` — 存储 image provider
- [ ] [ ] `register_tts(provider, model_id)` — 存储 tts provider
- [ ] [ ] `register_video(provider, model_id)` — 存储 video provider
- [ ] [ ] `get_embedding_provider()` — 实现（非 "not implemented"）
- [ ] [ ] `get_image_provider()` — 实现
- [ ] [ ] `get_tts_provider()` — 实现
- [ ] [ ] `get_video_provider()` — 实现
- [ ] [ ] 配置加载时自动注册所有 provider

#### D.2 Provider 实现补全
**文件**: `infrastructure/providers/src/`

- [ ] **OpenAI Provider**（已部分实现）：
  - [ ] `impl EmbeddingProvider for OpenAiProvider` — 验证完整
  - [ ] `impl ImageGenerationProvider for OpenAiProvider` — 验证完整
  - [ ] `impl TtsProvider for OpenAiProvider` — 验证完整
- [ ] **GLM Provider**：
  - [ ] 删除 glm.rs 中的死代码
  - [ ] 实现 ChatProvider 流式响应
  - [ ] 验证 token usage 解析
- [ ] **Kimi Provider**：
  - [ ] 实现 ChatProvider
  - [ ] 验证与 MiniMax/GLM 差异处理
- [ ] **Anthropic Provider**（已有 220 行）：
  - [ ] 验证流式响应 SSE 解析
  - [ ] 验证 thinking/thinking block 处理
  - [ ] 验证 usage 解析
- [ ] **New: ElevenLabs Provider**：
  - [ ] 实现 TtsProvider
- [ ] **New: Jina Provider**：
  - [ ] 实现 EmbeddingProvider
- [ ] **New: Perplexity Provider**：
  - [ ] 实现 ChatProvider + SearchProvider

---

### Phase E：MCP 系统

#### E.1 McpManager
**文件**: `application/runtime/src/mcp/`

- [ ] `mod.rs` — `McpManager` struct
- [ ] [ ] `connect(configs: Vec<McpServerConfig>)` — 调用 `McpRegistry::connect_all`
- [ ] [ ] `all_tools() -> Vec<Box<dyn Tool>>` — 从 McpRegistry 获取所有 MCP 工具并包装为 `McpToolWrapper`
- [ ] [ ] `get_tool(name) -> Option<Box<dyn Tool>>`
- [ ] [ ] 与 SkillsManager 集成：MCP 工具作为动态工具注入

---

### Phase F：定时与诊断

#### F.1 Scheduler（Cron）
**文件**: `application/runtime/src/cron/`

- [ ] `mod.rs` — `Scheduler` struct
- [ ] [ ] `TaskSpec` 结构（id/name/expression/type/payload）
- [ ] [ ] `TaskType` enum（Agent/Shell/Notification）
- [ ] [ ] `add(spec) -> TaskId` — 解析 cron 表达式并调度
- [ ] [ ] `list() -> Vec<TaskStatus>`
- [ ] [ ] `remove(task_id)`
- [ ] [ ] `pause(task_id)` / `resume(task_id)`
- [ ] [ ] 使用 `tokio::time::interval` 或 `cron` crate 驱动
- [ ] [ ] Agent 类型任务：调用 AgentLoop
- [ ] [ ] Shell 类型任务：执行命令
- [ ] [ ] Notification 类型任务：通过 Channel 推送

#### F.2 Doctor / Self-check
**文件**: `application/runtime/src/doctor/`

- [ ] `mod.rs` — `Doctor` struct
- [ ] [ ] `run_all() -> DoctorReport` — 并行检查所有组件
- [ ] [ ] `check_config()` — 配置文件存在性 + 有效性
- [ ] [ ] `check_providers()` — 各 Provider API 连接测试（ping）
- [ ] [ ] `check_memory()` — Memory 读写测试
- [ ] [ ] `check_channels()` — Channel 连接测试
- [ ] [ ] `check_storage()` — SQLite 读写测试
- [ ] [ ] `check_network()` — 网络连通性
- [ ] [ ] `DoctorReport::summary() -> String` — 格式化输出

---

### Phase G：LoopBreaker 装饰器

#### G.1 LoopBreakerAgent
**文件**: `application/runtime/src/agent/loop_breaker.rs`（或并入 agent 模块）

- [ ] `LoopBreakerAgent<A: AgentLoop>` struct — 包装 `Arc<Mutex<dyn AgentLoop>>`
- [ ] `CircuitBreaker` struct — 计数 + 时间窗口
- [ ] [ ] 阈值配置（max_calls, window_secs, break_duration_secs）
- [ ] [ ] `should_break() -> bool` — 是否触发熔断
- [ ] `impl AgentLoop for LoopBreakerAgent` — 在 run 前检查 breaker
- [ ] 装饰器模式：AgentLoop 实例用 `LoopBreakerAgent::wrap(agent)` 包装
- [ ] 日志：`LoopBreaker::triggered` / `recovered` 事件

---

### Phase H：配置系统

#### H.1 配置加载
**文件**: `application/runtime/src/config.rs`（或新建 `infrastructure/config/`）

- [ ] `Config` 结构体（完整配置）
- [ ] [ ] `[providers]` — provider 列表 + api_key/base_url
- [ ] [ ] `[models]` — 每个 provider 下的 model 列表 + capabilities
- [ ] [ ] `[routing]` — capability → model 路由规则
- [ ] [ ] `[channels.wechat]` — 微信配置
- [ ] [ ] `[channels.telegram]` — Telegram 配置
- [ ] [ ] `[scheduler]` — 时区/最大并发等
- [ ] [ ] `[mcp]` — MCP server 配置列表
- [ ] [ ] `[skills]` — skills 目录路径
- [ ] [ ] `[security]` — dangerous_commands 黑名单
- [ ] [ ] `[logging]` — 日志级别/路径/轮转
- [ ] `Config::load(path) -> Self` — 从 TOML 文件加载
- [ ] 支持热重载：`Config::watch(callback)` — 监听文件变化，触发回调

---

## 三、依赖关系图（开发顺序）

```
Phase A（核心链路）
  Registry (chat OK, 其他 stub)
    ↓
  SystemPromptBuilder (依赖 Registry)
    ↓
  AgentLoop (依赖 Registry + SkillsManager)
    ↓
  Wechat Channel (依赖 AgentLoop)
    ↓
  Orchestrator (依赖 Channel + AgentLoop)
    ↓
  main.rs (启动 Orchestrator)

Phase B（可并行）
  SessionManager ← 独立
  Memory trait  ← 独立
  Memory-Storage ← 依赖 SessionManager

Phase C（Skills，可并行或后置）
  SkillsManager (依赖 Tools)
    ↓
  Tools 实现 (依赖 Registry)
    ↓
  内置 Tools → 注入 SkillsManager

Phase D（Provider，可并行）
  Registry 全能力注册
  Provider 实现补全

Phase E（MCP，可并行）
  McpManager (依赖 infrastructure/mcp)

Phase F（可后期）
  Scheduler
  Doctor

Phase G（可后期）
  LoopBreakerAgent
```

---

## 四、当前阻断项（P0 先做）

1. **AgentLoop 未实现** — 无法处理用户消息，是所有通道的终点
2. **Wechat Channel 未实现** — 无法接收/发送微信消息
3. **Orchestrator 未实现** — 无法协调 Channel 和 AgentLoop
4. **SkillsManager 未实现** — AgentLoop 没有工具可用
5. **ServiceRegistry 只有 Chat 可用** — 其他能力 stub

---

## 五、重构路径（保持兼容）

### 不改变 trait 的修复
- [ ] 删除 `glm.rs` 死代码
- [ ] 验证各 provider 的 `ChatUsage` 解析
- [ ] 确认 `ProviderConfig.api` 字段语义（当前是 provider_type 如 "openai"）

### 按架构文档重构
- [ ] Phase 2：配置层改为 provider → models → capabilities（已有雏形，需完善）
- [ ] Phase 3：每个 provider 独立 struct（已有雏形，需完善）
- [ ] Phase 4：工具系统接入 Registry capability 路由

---

## 六、关键设计决策（待确认）

1. **AgentLoop 消息循环模式**：推模式（Channel → Orchestrator → AgentLoop）还是拉模式（AgentLoop poll Channel）？
2. **Session 存储格式**：SQLite 直接存 messages JSON，还是用专门的表结构？
3. **Skills 安装方式**：从本地目录扫描还是支持远程 URL？
4. **MCP 工具 vs 内置工具**：是否完全平等？MCP 工具需要哪些特殊处理？
5. **LoopBreaker 阈值**：默认值多少？如何暴露给用户配置？

---

*最后更新：2026-04-27*
