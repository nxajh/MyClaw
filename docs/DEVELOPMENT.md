# MyClaw 开发计划

> 基于代码实际状态更新（2026-04-29）
>
> **好消息：大部分核心工作已完成！** 以下是当前真实进度和剩余任务。

---

## 一、代码现状

### 1.1 已完成 ✅

| 模块 | 文件 | 行数 | 状态 |
|------|------|------|------|
| **AgentLoop** | `src/agents/agent_impl.rs` | 504 | ✅ 完成 |
| **SystemPromptBuilder** | `src/agents/prompt.rs` | 371 | ✅ 完成 |
| **Orchestrator** | `src/agents/orchestrator.rs` | 304 | ✅ 完成 |
| **Wechat Channel** | `src/channels/wechat.rs` | 819 | ✅ 完成 |
| **Telegram Channel** | `src/channels/telegram.rs` | 765 | ✅ 完成 |
| **SkillsManager** | `src/agents/skills.rs` | 97 | ✅ 完成 |
| **SessionManager** | `src/agents/session_manager.rs` | 183 | ✅ 完成 |
| **ServiceRegistry trait** | `src/providers/service_registry.rs` | 24 | ✅ 完成 |
| **Registry impl** | `src/registry/mod.rs` | 423 | ✅ 完成 |
| **Capability traits** | `src/providers/capability*.rs` | 371 | ✅ 完成 |
| **OpenAI Provider** | `src/providers/openai.rs` | 486 | ✅ 完成 |
| **MiniMax Provider** | `src/providers/minimax.rs` | 427 | ✅ 完成 |
| **GLM Provider** | `src/providers/glm.rs` | 438 | ✅ 完成 |
| **Kimi Provider** | `src/providers/kimi.rs` | 104 | ✅ 完成 |
| **Anthropic Provider** | `src/providers/anthropic.rs` | 233 | ✅ 完成 |
| **Fallback Provider** | `src/providers/fallback.rs` | — | ✅ 完成 |
| **MCP Client + Protocol** | `src/mcp/` | 2824 | ✅ 完成 |
| **MCP Manager** | `src/agents/mcp_manager.rs` | 179 | ✅ 完成 |
| **Memory trait + 实现** | `src/storage/` | 1856 | ✅ 完成 |
| **SQLite Session Storage** | `src/storage/sqlite.rs` | 472 | ✅ 完成 |
| **Config 系统** | `src/config/` | 398 | ✅ 完成 |
| **Composition Root** | `src/daemon.rs` | 411 | ✅ 完成 |
| **SubAgentDelegator** | `src/agents/sub_agent.rs` | 144 | ✅ 完成 |
| **内置 Tools（13个）** | `src/tools/` | 1961 | ✅ 完成 |

**内置 Tools 清单（13个）：**
- 核心：`shell`, `file_read`, `file_write`, `file_edit`, `glob_search`, `content_search`
- Web：`web_fetch`, `http_request`, `web_search`
- Utility：`calculator`, `ask_user`
- Memory：`memory_store`, `memory_recall`, `memory_forget`
- Multi-Agent：`delegate_task`

**Provider 支持的能力：**

| Provider | Chat | Embedding | Image | TTS | STT | Video | Search |
|----------|------|-----------|-------|-----|-----|-------|--------|
| OpenAI | ✅ | ✅ | ✅ | ✅ | — | — | — |
| MiniMax | ✅ | — | — | — | — | — | — |
| GLM | ✅ | ✅ | — | — | — | — | ✅ |
| Kimi | ✅ | — | — | — | — | — | — |
| Anthropic | ✅ | — | — | — | — | — | — |

---

### 1.2 未完成 / 进行中

| 模块 | 说明 | 优先级 |
|------|------|--------|
| **LoopBreakerAgent** | AgentLoop 装饰器，检测循环并熔断 | P1 |
| **Doctor / Self-check** | 诊断工具，检查配置/Provider/存储等 | P2 |
| **Scheduler（Cron）** | 定时任务系统 | P2 |
| **MCP: SttProvider** | 语音转文字能力 | P2 |
| **MCP: VideoProvider** | 视频生成能力 | P2 |
| **Discord/Slack Channel** | 可选通道 | P3 |

---

## 二、剩余任务详情

### 2.1 LoopBreakerAgent（P1）— 唯一核心缺失

**文件**: `src/agents/loop_breaker.rs`

当前 AgentLoop 没有循环检测，需要添加装饰器。

**需要的实现**：
- [ ] `LoopBreakerAgent<A: AgentLoop>` — 包装 `Arc<Mutex<dyn AgentLoop>>`
- [ ] `CircuitBreaker` — 滑动窗口计数器
- [ ] 检测三种循环模式：
  - **Exact repeat**：同一 tool + 同一 args 连续调用 ≥3 次
  - **Ping-pong**：两个 tool 来回切换 ≥4 轮
  - **No progress**：同一 tool 调用 ≥5 次，args 不同但结果 hash 相同
- [ ] 配置参数：
  ```toml
  [agent.loop_breaker]
  max_tool_calls = 100      # 硬限制兜底
  window_size = 20           # 滑动窗口大小
  max_repeats = 3           # Exact repeat 阈值
  ```
- [ ] 日志：`LoopBreaker::triggered` / `recovered` 事件

**参考架构文档 §8.8**。

### 2.2 Doctor / Self-check（P2）

**文件**: `src/agents/doctor.rs`

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
myclaw doctor --quick # 快速检查（无网络）
```

### 2.3 Scheduler / Cron（P2）

**文件**: `src/agents/cron.rs`

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

**CLI**：
```bash
myclaw cron list
myclaw cron add '0 9 * * 1-5' 'Good morning' --agent
myclaw cron remove <task-id>
```

### 2.4 Provider 能力补全（P2）

| 任务 | 状态 |
|------|------|
| OpenAI Embedding 验证 | 待验证 |
| GLM Search 实现 | 部分可用 |
| Kimi Chat 验证 | 待验证 |
| Anthropic Thinking 处理 | 待验证 |
| STT Provider（Whisper 等） | ❌ 未实现 |
| Video Provider（CogVideoX 等） | ❌ 未实现 |

---

## 三、依赖关系图（已全部完成，剩余为装饰器）

```
Registry (Chat ✅, Embedding ✅, Image ✅, TTS ✅, Search ✅, Video ❌, STT ❌)
  ↓
SystemPromptBuilder ✅
  ↓
AgentLoop ✅
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

剩余：
  LoopBreakerAgent ← 装饰 AgentLoop（可后置）
  Doctor ← 独立诊断工具（可后置）
  Scheduler ← 定时任务（可后置）
```

---

## 四、当前阻断项

**无阻断项。** 所有 P0 核心链路已完成。

---

## 五、文件分工（与架构文档一致）

| 文件 | 职责 |
|------|------|
| `src/agents/agent_impl.rs` | AgentLoop（核心循环） |
| `src/agents/orchestrator.rs` | Orchestrator（编排层） |
| `src/agents/prompt.rs` | SystemPromptBuilder |
| `src/agents/skills.rs` | SkillsManager |
| `src/agents/session_manager.rs` | SessionManager + InMemoryBackend |
| `src/agents/mcp_manager.rs` | McpManager |
| `src/agents/sub_agent.rs` | SubAgentDelegator |
| `src/agents/loop_breaker.rs` | **待实现** |
| `src/channels/wechat.rs` | WechatChannel |
| `src/channels/telegram.rs` | TelegramChannel |
| `src/providers/service_registry.rs` | ServiceRegistry trait |
| `src/registry/mod.rs` | Registry 实现 |
| `src/storage/` | Memory + Session + SQLite |
| `src/config/` | 配置加载 |
| `src/daemon.rs` | Composition Root |
| `src/tools/` | 13 个内置工具 |
| `src/mcp/` | MCP Client + Protocol |

---

## 六、重构路径（已完成）

| Phase | 状态 |
|-------|------|
| Phase 1: 不改变 trait，修复问题 | ✅ |
| Phase 2: Provider + Model 两层 + ServiceRegistry | ✅ |
| Phase 3: Provider trait 拆分（按能力） | ✅ |
| Phase 4: 工具系统接入 Capability | ✅ |

---

## 七、关键设计决策（已确认）

| 决策 | 结论 |
|------|------|
| AgentLoop 循环检测 | 待实现 LoopBreakerAgent |
| Session 存储 | InMemory + SQLite（fallback） |
| Skills 安装 | 从 workspace/skills 扫描 |
| MCP vs Skills | MCP 是 Tool，MCP Server 动态发现 |
| Provider 路由 | ServiceRegistry 按 model 路由 |

---

*最后更新：2026-04-29*
