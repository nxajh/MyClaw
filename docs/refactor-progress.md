# 重构进度跟踪

> 基于 RFC §七 实施清单
> 完整重构，不分阶段发布

## 进度统计

- 完成：6 / 61
- 进行中：0
- 待办：55

## 模块 A：类型基础（0/11）

- [x] A1. `TurnContext`（5 字段）/ `TurnResult` → `src/agents/turn.rs`
- [ ] A2. `AgentRuntime` struct（8 字段）+ `RuntimeDefaults` struct
- [ ] A3. `Tool` trait 加 `source() -> ToolSource`；execute 加 `&Session`；`ToolSource` enum
- [x] A4. `ToolFilter` / `SkillFilter` / `McpFilter` enum (All/Allow/Deny) → `src/config/filters.rs`
- [ ] A5. `Channel` trait 加默认 no-op 方法 `push_event` / `cancel_signal`
- [x] A6. `ChannelMessage` 加 `#[derive(Serialize, Deserialize)]`（MediaAttachment 同步）
- [x] A7. `AgentDelegator` trait（`delegate` + `list_available`）→ `src/agents/delegator.rs`
- [ ] A8. `DelegationEvent` enum（Completed / Failed，含 parent_session_id）
- [x] A9. `SessionNotOwned` 错误类型 → `src/agents/session/manager.rs`
- [ ] A10. `llm_stream` 模块（常量 + read/read_streamed 函数）
- [x] A11. `ProviderRegistry` trait — `ServiceRegistry` 重命名 ← `ef94853..HEAD`

## 模块 B：Session / SessionContext / SessionManager（0/4）

- [ ] B12. Session 字段改造（加 last_message/parent_session_id/agent_name/token_tracker；删 last_reply_target/max_history；加 transient persist/channel；新增 add_*/save_*/restore_token_count 方法）
- [ ] B13. 新建 `SessionContext`（session/agent/attachments/pending_retry/turn_lock；user_profile 在 G 加）
- [ ] B14. `SessionManager` 改造（单表 sessions；session_id_for_routing_key/get_by_id；switch SessionNotOwned；create_sub_session；list filter parent；delete cascade）
- [ ] B15. Sub-session 存储扁平化（删嵌套结构与 marker 文件）

## 模块 C：Agent / AgentRuntime / 执行器（0/7）

- [ ] C16. `AgentConfig` 简化（7 字段 + allows_tool 三维过滤）
- [ ] C17. `Agent` 简化为只一个 config 字段
- [ ] C18. `Agent.run(session, ctx, rt)` 实现（含 allowed_tools snapshot + 循环）
- [ ] C19. `ToolExecutor` 退化为 timeout 包装器
- [ ] C20. `LoopBreaker` 拆为 policy + per-turn counter
- [ ] C21. `ContextEngine` 合并 CompactionPolicy + CompactionExecutor
- [ ] C22. MCP / skill tool 注册时填正确 source()

## 模块 D：配置与 Prompt（0/5）

- [ ] D23. `GlobalConfig` 按模块拆段
- [ ] D24. `AgentRegistry` 实现为 `Arc<RwLock<HashMap>>`，含 reload_from_dir
- [ ] D25. `WorkspaceWatcher` 改为自维护
- [ ] D26. `prompt.rs` 删 IDENTITY/SOUL/USER.md 读取；改参数化 build_prompt
- [ ] D27. 启动校验 main/AGENT.md 缺失则报错

## 模块 E：路由（0/7）

- [ ] E28. `OrchestratorEvent` enum
- [ ] E29. Orchestrator 主循环（ask_router.fulfill 先于 process_turn）
- [ ] E30. Orchestrator 字段加 agent_runtime / ask_router
- [ ] E31. `AskRouter` 实现（wait_for_reply 返回 ChannelMessage）
- [ ] E32. ClientChannel 改造（streams 按 reply_target 索引；push_event/cancel_signal；强制 auth token）
- [ ] E33. WebhookHandler 退化为协议适配器
- [ ] E34. Scheduler 路径收编（删 SchedulerContext）

## 模块 F：委派（0/4）

- [ ] F35. AskUserTool / DelegateTool 实现并注册
- [ ] F36. `DelegationCoordinator` 实现 AgentDelegator
- [ ] F37. 启动恢复统一路径（list_all + parent_session_id 区分）
- [ ] F38. Sub-agent 完成回填合成 ChannelMessage 调父 process_turn

## 模块 G：per-user（0/6）

- [ ] G39. `UserResolver` routing_key → user_id 映射
- [ ] G40. `UserProfile` 加载/序列化/to_prompt_section
- [ ] G41. SessionContext 加 user_profile 字段
- [ ] G42. build_prompt 补齐 profile section
- [ ] G43. Memory tools 读写 `users/{id}/memory/`
- [ ] G44. `list_sessions_for_user(uid)` 反向 map 实现

## 模块 H：删除（0/13）

- [ ] H45. 删 AgentLoop / SessionHandle / LoopRegistry / run_session_actor / TurnStream / TurnInput
- [ ] H46. 删 RequestBuilder
- [ ] H47. 删 SubAgentDelegator
- [ ] H48. 删 SchedulerContext / WebhookContext
- [ ] H49. 删 AskUserHandler / DelegateHandler 闭包类型
- [ ] H50. 删 subagent_running_*.json marker 文件机制
- [ ] H51. 删 AgentConfig.max_history
- [ ] H52. 删 Session.last_reply_target
- [ ] H53. 删 [defaults]/[limits]/[context] config 段
- [ ] H54. 删 stream_first_chunk_timeout_secs / max_output_bytes 配置项
- [ ] H55. 删 IDENTITY.md / SOUL.md / RULES.md
- [ ] H56. 删 USER.md
- [ ] H57. 删 ClientChannel.loop_registry / evict_loop

## 模块 I：数据迁移（0/4）

- [ ] I58. scripts/migrate_main_agent.sh
- [ ] I59. scripts/migrate_memory.sh
- [ ] I60. scripts/migrate_user_profile.sh
- [ ] I61. Sub-session 旧数据直接丢弃（无需脚本）

---

## 实施日志

按时间倒序记录每次推进。

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
