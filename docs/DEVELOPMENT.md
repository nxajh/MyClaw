# MyClaw 开发计划

> 基于代码实际状态更新（2026-04-30）
>
> 核心系统已完成，LoopBreaker 熔断机制也已实现并集成。以下是当前真实进度和剩余任务。

---

## 一、代码现状

### 1.1 项目概览

| 指标 | 值 |
|------|-----|
| 语言/框架 | Rust (edition 2024) + Tokio |
| 总代码量 | **17,895 行**（83 个 `.rs` 文件） |
| Git 提交 | 122 次 |
| 开发周期 | 2026-04-26 ~ 2026-04-30 |

### 1.2 按模块统计

| 模块 | 文件数 | 行数 |
|------|--------|------|
| `src/agents/` | 11 | 2,894 |
| `src/channels/` | 4 | 2,176 |
| `src/config/` | 9 | 1,633 |
| `src/mcp/` | 9 | 2,824 |
| `src/providers/` | 21 | 3,523 |
| `src/registry/` | 2 | 481 |
| `src/storage/` | 12 | 1,856 |
| `src/tools/` | 12 | 1,996 |
| `src/daemon.rs` | 1 | 443 |
| `src/main.rs` | 1 | 39 |
| `src/lib.rs` | 1 | 30 |
| **合计** | **83** | **17,895** |

### 1.3 已完成模块详情 ✅

#### Agent 核心

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **AgentLoop** | `src/agents/agent_impl.rs` | 607 | 核心对话循环，集成 LoopBreaker |
| **SystemPromptBuilder** | `src/agents/prompt.rs` | 371 | 系统提示词构建器 |
| **Orchestrator** | `src/agents/orchestrator.rs` | 512 | 编排层，连接 Channel 与 AgentLoop |
| **LoopBreaker** | `src/agents/loop_breaker.rs` | 593 | 循环熔断检测（16 个单元测试） |
| **SkillsManager** | `src/agents/skills.rs` | 97 | 工具注册与管理 |
| **SessionManager** | `src/agents/session_manager.rs` | 209 | 会话生命周期管理 |
| **McpManager** | `src/agents/mcp_manager.rs` | 179 | MCP 服务器生命周期管理 |
| **SubAgentDelegator** | `src/agents/sub_agent.rs` | 229 | 子 Agent 委托（同步+异步） |
| **DelegationManager** | `src/agents/delegation.rs` | 68 | 异步委托事件系统 |

#### Channel 通道

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **Telegram Channel** | `src/channels/telegram.rs` | 1,234 | 长轮询、消息分块、Typing、去重、@提及 |
| **WeChat Channel** | `src/channels/wechat.rs` | 819 | iLink Bot API、QR 登录、加密通信 |
| **Channel 消息类型** | `src/channels/message.rs` | 112 | Channel trait + ChannelMessage + DedupState |

#### Provider 服务

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **OpenAI** | `src/providers/openai.rs` | 492 | Chat + Image + TTS + Embedding |
| **MiniMax** | `src/providers/minimax.rs` | 433 | OpenAI 兼容协议 |
| **GLM (智谱)** | `src/providers/glm.rs` | 444 | Chat + Embedding + Search + Thinking |
| **Kimi (月之暗面)** | `src/providers/kimi.rs` | 106 | OpenAI 兼容协议 |
| **Anthropic** | `src/providers/anthropic.rs` | 237 | Messages API |
| **Xiaomi MiMo** | `src/providers/xiaomi.rs` | 447 | Anthropic 兼容协议 |
| **Fallback Provider** | `src/providers/fallback.rs` | 119 | 多 Provider 故障切换 |
| **Capability traits** | `src/providers/capability*.rs` | 440 | Chat/Embedding/Tool 能力接口 |
| **Shared 工具** | `src/providers/shared.rs` | 472 | SSE 解析、Auth、Body 构建 |

#### MCP 客户端

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **MCP Client** | `src/mcp/client.rs` | 417 | stdio/HTTP/SSE 三种传输 |
| **MCP Protocol** | `src/mcp/protocol.rs` | 229 | JSON-RPC 2.0 协议类型 |
| **MCP Transport** | `src/mcp/transport.rs` | 1,283 | 传输层实现 |
| **MCP Tool Wrapper** | `src/mcp/tool.rs` | 230 | MCP 工具 → myclaw Tool 适配 |
| **MCP Deferred Loading** | `src/mcp/deferred.rs` | 548 | 延迟加载 MCP 工具 schema |

#### 存储层

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **Memory trait** | `src/storage/memory.rs` | 212 | Memory 接口定义 |
| **SharedMemory** | `src/storage/shared.rs` | 106 | 跨会话持久记忆 |
| **PrivateMemory** | `src/storage/private.rs` | 137 | 会话级私有记忆 |
| **SQLite Session** | `src/storage/sqlite.rs` | 472 | SQLite 会话持久化 |
| **Embedding** | `src/storage/embedding.rs` | 308 | 向量嵌入接口与实现 |
| **Vector** | `src/storage/vector.rs` | 209 | 余弦相似度、归一化、混合搜索 |
| **Policy** | `src/storage/policy.rs` | 198 | 内存操作策略引擎 |

#### 配置系统

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **AppConfig** | `src/config/mod.rs` | 408 | TOML 加载 + 环境变量展开 |
| **Config 核心** | `src/config/lib.rs` | 398 | 配置类型定义 |
| **Provider Config** | `src/config/provider.rs` | 159 | Provider 连接/认证/模型声明 |
| **Channel Config** | `src/config/channel.rs` | 163 | WeChat/Telegram 通道配置 |
| **Routing Config** | `src/config/routing.rs` | 127 | 模型选择策略 |
| **Agent Config** | `src/config/agent.rs` | 146 | 自治级别、循环熔断、提示词设置 |
| **MCP Config** | `src/config/mcp.rs` | 83 | MCP 服务器配置 |
| **Memory Config** | `src/config/memory.rs` | 84 | Memory 存储后端配置 |
| **SubAgent Config** | `src/config/sub_agent.rs` | 65 | 子 Agent 定义 |

#### 工具系统

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **Shell** | `src/tools/shell.rs` | 115 | Shell 命令执行 |
| **File Ops** | `src/tools/file_ops.rs` | 281 | 文件读/写/编辑 |
| **Search** | `src/tools/search.rs` | 303 | Glob 文件搜索 + 正则内容搜索 |
| **Web** | `src/tools/web.rs` | 146 | Web 页面抓取 |
| **HTTP** | `src/tools/http.rs` | 156 | 通用 HTTP 请求 |
| **Web Search** | `src/tools/web_search.rs` | 70 | Web 搜索 |
| **Calculator** | `src/tools/calculator.rs` | 336 | 数学表达式求值 |
| **Ask User** | `src/tools/ask_user.rs` | 71 | 暂停等待用户输入 |
| **Memory** | `src/tools/memory.rs` | 286 | 内存存/取/删 |
| **Delegate** | `src/tools/delegate.rs` | 101 | 多 Agent 任务委派 |

#### 基础设施

| 模块 | 文件 | 行数 | 说明 |
|------|------|------|------|
| **Registry** | `src/registry/mod.rs` | 423 | 能力路由中心，按 Capability 分发 |
| **Routing** | `src/registry/routing.rs` | 58 | 路由策略类型 |
| **Daemon** | `src/daemon.rs` | 443 | Composition Root，组装所有组件 |

---

### 1.4 Provider 能力矩阵

| Provider | Chat | Embedding | Image | TTS | STT | Video | Search | Thinking |
|----------|------|-----------|-------|-----|-----|-------|--------|----------|
| OpenAI | ✅ | ✅ | ✅ | ✅ | — | — | — | — |
| MiniMax | ✅ | — | — | — | — | — | — | — |
| GLM | ✅ | ✅ | — | — | — | — | ✅ | ✅ |
| Kimi | ✅ | — | — | — | — | — | — | — |
| Anthropic | ✅ | — | — | — | — | — | — | — |
| Xiaomi MiMo | ✅ | — | — | — | — | — | — | ✅ |

> **注意**：Xiaomi MiMo Provider 代码已完成（447 行），但尚未在 `daemon.rs` 中注册组装。

### 1.5 LoopBreaker 熔断机制 ✅（已完成）

**文件**：`src/agents/loop_breaker.rs`（593 行，含 16 个单元测试）

| 特性 | 说明 |
|------|------|
| Exact repeat | 同一 tool + 同一 args 连续调用 ≥3 次 → 熔断 |
| Ping-pong | 两个 tool 来回交替 ≥4 轮 → 熔断 |
| No progress | 同一 tool ≥5 次，args 不同但结果 hash 相同 → 熔断 |
| Max calls | 总 tool 调用硬限制（默认 100） |
| 滑动窗口 | 默认 20 次调用窗口 |
| 集成状态 | ✅ 已集成到 `agent_impl.rs` AgentLoop 中 |

---

## 二、未完成任务

### 2.1 Doctor / Self-check（P2）

**目标文件**: `src/agents/doctor.rs`

诊断工具，验证安装完整性。

**检查项**：
- [ ] `check_config()` — 配置文件存在性 + 有效性
- [ ] `check_providers()` — 各 Provider API 连接测试
- [ ] `check_memory()` — Memory 读写测试
- [ ] `check_channels()` — Channel 连接测试
- [ ] `check_storage()` — SQLite 读写测试
- [ ] `DoctorReport::summary() -> String` — 格式化输出

**CLI**：
```bash
myclaw doctor          # 完整诊断
myclaw doctor --quick  # 快速检查（无网络）
```

### 2.2 Scheduler / Cron（P2）

**目标文件**: `src/agents/cron.rs`

定时任务系统。

**任务类型**：

| 类型 | 说明 |
|------|------|
| Agent | 调用 AgentLoop 执行 |
| Shell | 执行命令 |
| Notification | 通过 Channel 推送通知 |

**配置**：
```toml
[scheduler]
enabled = true
timezone = "Asia/Shanghai"
max_concurrent = 10
```

### 2.3 STT Provider（P2）

**文件**: `src/providers/stt.rs`（已有 trait 定义，39 行）

需要实现具体 Provider（如 Whisper）。

### 2.4 Video Provider（P2）

**文件**: `src/providers/video.rs`（已有 trait 定义，45 行）

需要实现具体 Provider（如 CogVideoX）。

### 2.5 Discord/Slack Channel（P3）

可选扩展通道，未开始。

### 2.6 Xiaomi MiMo 注册组装

Provider 代码已完成，需在 `daemon.rs` Composition Root 中添加注册逻辑。

---

## 三、依赖关系图

```
Registry (Chat ✅, Embedding ✅, Image ✅, TTS ✅, Search ✅, Video trait-only, STT trait-only)
  ↓
SystemPromptBuilder ✅
  ↓
AgentLoop ✅ + LoopBreaker ✅
  ↓
SkillsManager ✅ + Tools ✅
  ↓
McpManager ✅
  ↓
Orchestrator ✅
  ↓
Wechat/Telegram Channel ✅
  ↓
daemon.rs (Composition Root) ✅

剩余可选：
  Doctor ← 独立诊断工具（可后置）
  Scheduler ← 定时任务（可后置）
  Xiaomi MiMo 注册 ← daemon.rs 中一行代码
```

---

## 四、当前阻断项

**无阻断项。** 所有 P0/P1 核心链路已完成。

---

## 五、文件分工

| 文件 | 职责 |
|------|------|
| `src/agents/agent_impl.rs` | AgentLoop（核心循环，607 行） |
| `src/agents/orchestrator.rs` | Orchestrator（编排层，512 行） |
| `src/agents/prompt.rs` | SystemPromptBuilder（371 行） |
| `src/agents/loop_breaker.rs` | LoopBreaker 循环熔断（593 行） |
| `src/agents/skills.rs` | SkillsManager（97 行） |
| `src/agents/session_manager.rs` | SessionManager + InMemoryBackend（209 行） |
| `src/agents/mcp_manager.rs` | McpManager（179 行） |
| `src/agents/sub_agent.rs` | SubAgentDelegator（229 行） |
| `src/agents/delegation.rs` | DelegationManager 异步事件（68 行） |
| `src/channels/telegram.rs` | TelegramChannel（1,234 行） |
| `src/channels/wechat.rs` | WechatChannel（819 行） |
| `src/providers/openai.rs` | OpenAI Provider（492 行） |
| `src/providers/minimax.rs` | MiniMax Provider（433 行） |
| `src/providers/glm.rs` | GLM Provider（444 行） |
| `src/providers/kimi.rs` | Kimi Provider（106 行） |
| `src/providers/anthropic.rs` | Anthropic Provider（237 行） |
| `src/providers/xiaomi.rs` | Xiaomi MiMo Provider（447 行） |
| `src/providers/fallback.rs` | Fallback Provider（119 行） |
| `src/providers/shared.rs` | SSE/Auth/Body 共享工具（472 行） |
| `src/providers/service_registry.rs` | ServiceRegistry trait（23 行） |
| `src/providers/capability*.rs` | Capability traits（440 行） |
| `src/registry/mod.rs` | Registry 实现（423 行） |
| `src/storage/` | Memory + Session + SQLite + Vector + Policy（1,856 行） |
| `src/config/` | 全部配置（1,633 行） |
| `src/mcp/` | MCP Client + Protocol + Transport + Deferred（2,824 行） |
| `src/tools/` | 14 个内置工具（1,996 行） |
| `src/daemon.rs` | Composition Root（443 行） |

---

## 六、重构路径（已完成）

| Phase | 状态 |
|-------|------|
| Phase 1: 不改变 trait，修复问题 | ✅ |
| Phase 2: Provider + Model 两层 + ServiceRegistry | ✅ |
| Phase 3: Provider trait 拆分（按能力） | ✅ |
| Phase 4: 工具系统接入 Capability | ✅ |

---

## 七、关键设计决策

| 决策 | 结论 | 状态 |
|------|------|------|
| AgentLoop 循环检测 | LoopBreaker（3 种模式 + 硬限制） | ✅ 已实现 |
| Session 存储 | InMemory + SQLite（fallback） | ✅ |
| Skills 安装 | 从 workspace/skills 扫描 | ✅ |
| MCP vs Skills | MCP 是 Tool，MCP Server 动态发现 | ✅ |
| Provider 路由 | ServiceRegistry 按 Capability 路由 | ✅ |
| MCP 工具延迟加载 | Deferred Loading，按需拉取 schema | ✅ |
| 多 Agent 委托 | SubAgentDelegator + 异步 DelegationManager | ✅ |

---

*最后更新：2026-04-30*
