# 重构进度跟踪

> 基于 RFC §七 实施清单
> 完整重构，不分阶段发布

## 进度统计

- 完成：47 / 61
- 进行中：C18（scaffold 已创建，完整实现等 E29+F36 原子落地）
- 待办：14

## 模块 A：类型基础（0/11）

- [x] A1. `TurnContext`（5 字段）/ `TurnResult` → `src/agents/turn.rs`
- [x] A2. `AgentRuntime` struct（9 字段）→ `src/agents/runtime.rs`；RuntimeDefaults 合入
- [x] A3. `Tool` trait 加 `source() -> ToolSource`（默认 Builtin）；execute 加 `&Session` 参数 ✅
- [x] A4. `ToolFilter` / `SkillFilter` / `McpFilter` enum (All/Allow/Deny) → `src/config/filters.rs`
- [x] A5. `Channel` trait 加默认 no-op 方法 `push_event` / `cancel_signal`
- [x] A6. `ChannelMessage` 加 `#[derive(Serialize, Deserialize)]`（MediaAttachment 同步）
- [x] A7. `AgentDelegator` trait（`delegate` + `list_available`）→ `src/agents/delegator.rs`
- [x] A8. `DelegationEvent` 字段重命名 session_key → parent_session_id（值仍是 routing_key，待 E29 真正切到 session_id）
- [x] A9. `SessionNotOwned` 错误类型 → `src/agents/session/manager.rs`
- [x] A10. `llm_stream` 模块（常量 + read/read_streamed 函数）→ `src/agents/llm_stream.rs`
- [x] A11. `ProviderRegistry` trait — `ServiceRegistry` 重命名 ← `ef94853..HEAD`

## 模块 B：Session / SessionContext / SessionManager（0/4）

- [x] B12. Session 字段改造完成：token_tracker + persist/channel transient + add_user/add_assistant + save_to_disk
- [~] B14. SessionManager 改造（已加 SessionNotOwned 接线 / list_sub_sessions / session_id_for_routing_key / get_by_id / create_sub_session / delete cascade；list_sessions 过滤 parent）
- [x] B13. 新建 `SessionContext`（session/attachments/pending_retry/turn_lock；user_profile 待 G41） → `src/agents/session_context.rs`
- [x] B15. Sub-session 存储扁平化（同级目录 + meta.json parent_session_id 关联）

## 模块 C：Agent / AgentRuntime / 执行器（0/7）

- [~] C16. `SubAgentConfig` 加 skills/mcp 三维过滤 + allows_tool/skill/mcp helper；slim AgentConfig 形态等 C18
- [~] C17. `Agent` 简化为只一个 config 字段（scaffold 已创建：agent_run.rs RfcAgent）
- [~] C18. `Agent.run(session, ctx, rt)` 实现（scaffold 已创建，完整实现等 E29+F36）
- [x] C19. `ToolExecutor` 重命名（原 `DefaultToolExecutor`）；ask_user/agent_delegate inline 处理待 F35 拆出
- [x] C20. `LoopBreaker` policy + per-turn counter（已有 LoopBreakerConfig 分离 + reset() 每轮重置）
- [x] C21. `ContextEngine` 合并 CompactionPolicy + CompactionExecutor → `src/agents/context_engine.rs`
- [x] C22. MCP tool wrapper 填 `ToolSource::Mcp { server }`；skill 单独 wrapper 未来再加

## 模块 D：配置与 Prompt（0/5）

- [x] D23. `GlobalConfig` 按模块拆段（已是模块化结构：providers/routing/channels/agent/memory/mcp/logging 各自独立 struct）
- [x] D24. `AgentRegistry` 实现为 `Arc<RwLock<HashMap>>`，含 reload_from_dir → `src/agents/agent_registry.rs`
- [x] D25. `WorkspaceWatcher` 加 `spawn_managed`：自持 AgentRegistry/SkillManager 并自动 reload
- [x] D26. `prompt.rs` 删 IDENTITY/SOUL/USER.md/RULES.md 读取（部分：build_prompt 参数化在 C18 完成）
- [x] D27. 启动校验 main/AGENT.md 缺失则报错（daemon::run 顶部）

## 模块 E：路由（0/7）

- [x] E28. `OrchestratorEvent` enum → `src/agents/orchestrator_event.rs`
- [ ] E29. Orchestrator 主循环（ask_router.fulfill 先于 process_turn）
- [ ] E30. Orchestrator 字段加 agent_runtime / ask_router
- [x] E31. `AskRouter` 实现（register/fulfill/cancel；indexed by session_id）→ `src/agents/ask_router.rs`
- [ ] E32. ClientChannel 改造（streams 按 reply_target 索引；push_event/cancel_signal；强制 auth token）
- [x] E33. WebhookHandler 已是协议适配器（接 HTTP → ChannelMessage → Channel.send；无业务逻辑）
- [ ] E34. Scheduler 路径收编（删 SchedulerContext）

## 模块 F：委派（0/4）

- [ ] F35. AskUserTool / DelegateTool 实现并注册
- [~] F36. SubAgentDelegator 现在也实现 `AgentDelegator` trait（共存 TaskDelegator 直到 H47 删除）
- [ ] F37. 启动恢复统一路径（list_all + parent_session_id 区分）
- [ ] F38. Sub-agent 完成回填合成 ChannelMessage 调父 process_turn

## 模块 G：per-user（0/6）

- [x] G39. `UserResolver` routing_key → user_id 映射 → `src/agents/user_profile.rs`
- [x] G40. `UserProfile` 加载/序列化/to_prompt_section → `src/agents/user_profile.rs`
- [x] G41. SessionContext 加 user_profile 字段（含 with_user_profile / reload_user_profile）
- [x] G42. build_prompt 补齐 profile section（`SystemPromptBuilder::build_with_profile`）
- [x] G43. Memory tools 读写 `users/{id}/memory/`（workspace_dir + session.owner → user_id）
- [x] G44. `list_sessions_for_user(uid)` 反向 map 实现（SessionManager method）

## 模块 H：删除（0/13）

- [ ] H45. 删 AgentLoop / SessionHandle / LoopRegistry / run_session_actor / TurnStream / TurnInput
- [ ] H46. 删 RequestBuilder
- [ ] H47. 删 SubAgentDelegator
- [ ] H48. 删 SchedulerContext / WebhookContext
- [ ] H49. 删 AskUserHandler / DelegateHandler 闭包类型
- [ ] H50. 删 subagent_running_*.json marker 文件机制
- [x] H51. 删 AgentConfig.max_history（死字段）
- [x] H52. 删 Session.last_reply_target（仅 Session struct；storage 层留字段用于旧元数据 backward read）
- [x] H53. 删 [defaults] config 段（CLI 改为读 routing.chat.models[0]）；[context] 段在 C21 删，[limits] 不存在
- [x] H54. 删 stream_first_chunk_timeout_secs / max_output_bytes 配置项；常量内联
- [x] H55. 删 IDENTITY.md / SOUL.md / RULES.md 读取（代码侧）
- [x] H56. 删 USER.md 读取（代码侧；G40 补 UserProfile 注入）
- [ ] H57. 删 ClientChannel.loop_registry / evict_loop

## 模块 I：数据迁移（0/4）

- [x] I58. scripts/migrate_main_agent.sh（含 IDENTITY/SOUL.md 折叠）
- [x] I59. scripts/migrate_memory.sh（含 user_id 参数）
- [x] I60. scripts/migrate_user_profile.sh（USER.md → profile.toml）
- [x] I61. Sub-session 旧数据直接丢弃（无需脚本）

---

## 实施日志

按时间倒序记录每次推进。

### A3 + B12 剩余 — Tool::execute &Session + Session 字段补丁

- A3 剩余: Tool trait execute 签名加 `session: &Session` 参数
  - 26 个 Tool impl + 3 个 test mock impl 全部更新
  - ToolExecutor.run_tool / MemoryToolExecutor.execute 透传 session
  - CompactionExecutor 用临时 session（compaction 不需要真 session）
  - 所有测试调用点补充 session 参数（test_session() helper）
- B12 剩余:
  - TokenTracker 从 agent_impl/types.rs 迁移到 session/types.rs（pub）
  - estimate_tokens / estimate_message_tokens 同步迁移
  - Session 新增 token_tracker / persist / channel 字段
  - add_user_text → add_user, add_assistant_text → add_assistant + deprecated 别名
  - 新增 save_to_disk() 方法
  - Session 手动实现 Debug（dyn PersistHook/Channel 不 impl Debug）
- 377 lib tests 全部通过

### C21 — ContextEngine façade

- 新建 src/agents/context_engine.rs
- 内部组合 CompactionPolicy + CompactionExecutor
- 暴露统一方法：should_compact / execute_compaction / token_total / update_usage 等
- 旧文件保留，C18 切换后清理

### C17 scaffold — 新 Agent struct

- 新建 src/agents/agent_run.rs
- Agent { config: SubAgentConfig }
- Agent::run() 桥接接口已定义（内部 todo，等 C18+E29+F36 原子落地）
- Agent::filtered_tool_specs() 按 config 三维过滤工具
- 导出为 RfcAgent（与旧 Agent factory 共存，H45 切换）

### G43 — Memory tools per-user

- memory_tool.rs: knowledge_dir → workspace_dir
- 新增 resolve_memory_dir()：session.owner 替换特殊字符 → user_id
- 路径：workspace/users/{user_id}/memory/
- daemon.rs + cmd_chat.rs + cmd_exec.rs 改用 workspace_dir 构造
- 377 lib tests 全部通过

### B15 — 子会话存储扁平化

- sub_agent.rs: open_sub_session 不再创建嵌套 sessions/{parent}/subagents/ 路径
- 子会话直接创建在 sessions/{sub}/ 同级目录
- parent_session_id 通过 backend.save_parent_session_id() 写入 meta.json
- 旧嵌套数据不迁移（I61 决议）
- 377 lib tests 全部通过

### D23 + E33 — 确认已实现项

- D23: AppConfig 早已是模块化结构 (providers / routing / channels / agent /
  memory / mcp_servers / logging 各自独立 sub-struct)，无需进一步拆段
- E33: WebhookHandler 早已是纯协议适配器 (HTTP → ChannelMessage → Channel.send)
  无业务逻辑

### A8 + C16 partial — DelegationEvent 字段重命名 + 三维过滤

- A8: DelegationEvent.session_key → parent_session_id（Completed / Failed
  两个 variant）；3 个发送点和 2 个 match arm 同步更新
- C16 partial: SubAgentConfig 加两个 NameFilter 字段：
  - `skills: SkillFilter`（默认 all）
  - `mcp: McpFilter`（默认 all）
  并加三个 helper：allows_tool / allows_skill / allows_mcp。agent_loader
  构造、5 处测试 fixture 同步更新。Agent.run 在 C18 用这些过滤器从全局
  ToolRegistry / SkillManager 切出 per-agent 视图。

### G41 + H53 — SessionContext.user_profile + 删 [defaults]

- G41: SessionContext 加 `user_profile: Arc<Mutex<UserProfile>>` 字段；
  新增 `with_user_profile(session, workspace_dir, user_id)` 构造函数
  和 `reload_user_profile()` async 方法。
- H53: 删 Config::Defaults struct、AppConfig.defaults 字段、RawConfig.defaults
  字段。4 个 CLI 文件（status / doctor / chat / config）改为从
  `routing.get(Capability::Chat).models.first()` 取默认模型。config init
  模板从 `[defaults] model = ...` 改为 `[routing.chat] models = [...]`。
- 375 lib tests 仍全部通过

### F36 partial + G44 + H52 — Delegator dual impl + 反向 list + 死字段删除

- F36: SubAgentDelegator 实现 `crate::agents::AgentDelegator`（除已有的
  TaskDelegator）。新 trait 的 delegate 拿 &Session，从 last_message 取
  reply_target 转给 delegate_with_parent；list_available 返回
  Vec<(name, Option<description>)>。TaskDelegator 留到 H47 删除。
- G44: SessionManager.list_sessions_for_user(resolver, user_id) 把
  UserResolver 反查的所有 routing_key 的 session 列表 dedup 合并。
- H52: Session.last_reply_target 字段删除。reply_target() 不再有 fallback；
  record_inbound 不再写。storage 层 meta.last_reply_target 保留用于读取
  pre-B12 元数据但不再被 Session 自动恢复——next inbound 会重建。

375 lib tests 仍全部通过。

### A2 + D25 + E28 + E31 + G39 + G40 + G42 — 一波 scaffolding

- A2 AgentRuntime: `src/agents/runtime.rs` 新建。9 字段：providers / tools /
  skills / agents (AgentRegistry) / loop_breaker_defaults / tool_timeout_secs /
  persist / workspace_dir / knowledge_dir。with_persist/with_dirs builder。
- D25 WorkspaceWatcher::spawn_managed: 新方法，spawns 一个 tokio 任务持有
  AgentRegistry + SkillManager；目录变化时直接 reload，不需要外部 polling。
  返回 ManagedWatcherGuard，drop 时取消任务。
- E28 OrchestratorEvent enum: 新文件 orchestrator_event.rs 定义统一事件枚举
  Inbound/Scheduled/Delegation/AskReply/Shutdown，主循环切换待 E29 落地。
- E31 AskRouter: `src/agents/ask_router.rs` 实现 register/fulfill/cancel；
  按 session_id 索引（支持子会话跨频道 ask_user）；带 4 个单元测试。
- G39 UserResolver: routing_key → user_id 默认恒等映射，可 override。
- G40 UserProfile: workspace/users/{uid}/profile.toml 加载 + 序列化 +
  is_empty + to_prompt_section（None when empty）。
- G42 SystemPromptBuilder.build_with_profile: 把 profile.to_prompt_section()
  插在 Runtime section 之前；build() 保持向后兼容（profile=None）。
- 全部 +13 测试通过

### C19 + C20 + B14 partial + D24 — ToolExecutor 重命名 + LoopBreaker 确认 + SessionManager 子会话 + AgentRegistry

- C19: `DefaultToolExecutor` → `ToolExecutor` 重命名（2 个文件，5 处）
- C20: 现状 LoopBreaker 已经满足 RFC：LoopBreakerConfig 分离、reset() 每轮在
  `turn.rs:37` 调用；确认无需重构
- B14 partial: SessionManager 加 5 个方法：
  - `list_sub_sessions(parent)` — 按 parent_session_id 过滤
  - `session_id_for_routing_key(rk)` — 别名（B14 路由键迁移占位）
  - `get_by_id(session_id)` — 无 routing_key 直查
  - `create_sub_session(parent, agent)` — 自动写 parent_session_id + agent_name
  - `list_sessions(user)` 过滤掉子会话；`delete_session` 级联删子会话
- D24: 新建 `src/agents/agent_registry.rs`：`AgentRegistry` struct 内部
  `Arc<RwLock<HashMap<String, SubAgentConfig>>>`，提供 get / values_cloned /
  names / replace_all / reload_from_dir 方法。已迁移所有
  `Arc<RwLock<Vec<SubAgentConfig>>>` 用法（sub_agent.rs / resource_provider.rs /
  agent_impl/mod.rs / daemon.rs / commands/reload.rs / request_builder.rs）。
  `commands/reload` 的 agent 重载从手写 swap 改为 `reload_from_dir` 一行。
- 363 lib tests 全部通过（+2 来自新 registry 测试）

### A3 + B13 + C22 + D27 — ToolSource enum + SessionContext + MCP source + main 校验

- A3：`ToolSource { Builtin, Skill { name }, Mcp { server } }` enum；
  Tool trait 加 `fn source() -> ToolSource` 默认返回 Builtin；从 providers
  re-export
- B13：`src/agents/session_context.rs` 新建 SessionContext struct：
  `session: Arc<Mutex<Session>>`、`attachments: Arc<AttachmentManager>`、
  `pending_retry: Arc<Mutex<Option<String>>>`、`turn_lock: Arc<Mutex<()>>`；
  user_profile 字段留到 G41
- C22：`McpToolWrapper::source()` 从 prefixed_name 解出 server 名返回
  `ToolSource::Mcp { server }`；skill 单独 wrapper 等真有 per-skill tool
  注册时再加
- D27：daemon::run 顶部加 `workspace/agents/main/AGENT.md` 缺失检查，
  缺失时 bail 并指向 migrate_main_agent.sh
- 361 lib tests 仍全部通过

### B12 部分 + B14 部分 — Session struct 加新字段 + SessionNotOwned 接线

- Session 加三个字段：last_message: Option<ChannelMessage>、
  parent_session_id: Option<String>、agent_name: String（默认 "main"）
- 加助手方法：record_inbound（同时更新 last_message 与 legacy last_reply_target）、
  reply_target（统一 getter）
- SessionBackend trait 加六个 default-no-op 方法：save_last_message / load_last_message /
  save_agent_name / load_agent_name / save_parent_session_id / load_parent_session_id
- JsonFileBackend 完整实现新字段持久化，meta.json 兼容旧格式
- Orchestrator 入站消息处理改用 session.record_inbound + save_last_message，
  legacy save_reply_target 保留为过渡
- SessionManager.switch_session 用 SessionNotOwned 包装 PermissionDenied 错误
- 还差：token_tracker 类型、transient persist/channel 字段、Session.add_* / save_*
  方法重命名 — 这些与 C18 Agent.run 重写一并完成
- 361 lib tests 全部通过

### I58 / I59 / I60 / I61 — 数据迁移脚本

- migrate_main_agent.sh：建 agents/main/AGENT.md，把 IDENTITY.md / SOUL.md 折叠
  进 body；原文件备份到 .migration_backup/
- migrate_memory.sh：workspace/memory/*.md → workspace/users/{uid}/memory/；
  原目录留 MIGRATED_TO_USERS.txt 标记
- migrate_user_profile.sh：workspace/USER.md → workspace/users/{uid}/profile.toml
  （内容写入 custom_instructions 三引号字符串）；原文件移到 .USER.md.migrated
- I61：Sub-session 旧嵌套数据不迁移，新代码扫描时直接忽略，符合 RFC 决议
- 三个脚本在临时 workspace 上端到端 smoke 测试通过

### H51 / H54 — 删 max_history / max_output_bytes / stream_first_chunk_timeout_secs

- AgentConfig.max_history 是死字段，从未读取，直接删除（config + AgentConfig + sub_agent + daemon）
- stream_first_chunk_timeout_secs / max_output_bytes 替换为 llm_stream 模块常量
  + 100 KiB 字面量；CompactionExecutor::new 从 4 参数退化为 3 参数
- daemon.rs 删 calculate_max_output_bytes 辅助函数（~30 行）
- 361 个 lib 测试全部通过

### A5 / A10 / D26 / H55 / H56 — 通道默认方法 + 流读取常量 + prompt 清理

- A5：Channel trait 加 `push_event(reply_target, event)` 和
  `cancel_signal(reply_target) -> Option<CancellationToken>` 默认方法
- A10：`src/agents/llm_stream.rs` 新建，定义 STREAM_FIRST_CHUNK_TIMEOUT
  (600s) / STREAM_CHUNK_INTERVAL_TIMEOUT (120s)，read_next / read_to_string 辅助
- D26 + H55 + H56：prompt.rs 删 RULES.md / IDENTITY.md / SOUL.md / USER.md
  读取代码；behavioral rules 退化为五个硬编码 section；workspace_dir 信息折入
  Runtime section；删 `bootstrap_max_chars` 配置字段
- 11 个 prompt 单元测试全部通过

### A6 / A7 — ChannelMessage 可序列化 + AgentDelegator trait

- A6：ChannelMessage / MediaAttachment 加 Serialize + Deserialize（为
  Session.last_message 持久化做准备）
- A7：src/agents/delegator.rs 定义 AgentDelegator trait（delegate + list_available）；
  与现有 TaskDelegator 共存，后续 F35 / F36 切换
- 构建通过

### A1 / A4 / A9 三个新增类型

- A1：`src/agents/turn.rs` 新建，定义 `TurnContext<'a>` (5 字段) + `TurnResult`
- A4：`src/config/filters.rs` 新建，定义 `NameFilter` (All/Allow/Deny) + 三个别名 `ToolFilter` / `SkillFilter` / `McpFilter`；带 3 个单元测试
- A9：`src/agents/session/manager.rs` 加 `SessionNotOwned` 错误类型；mod.rs 导出
- 构建通过，新增的 3 个 filter 测试也通过

### A11 ServiceRegistry → ProviderRegistry 重命名

- 文件 `src/providers/service_registry.rs` → `provider_registry.rs`
- trait 名 `ServiceRegistry` → `ProviderRegistry`
- 全文件 sed 替换 `ServiceRegistry` / `service_registry`
- 16 个文件 / 45 处 occurrence
- `cargo build` 通过
