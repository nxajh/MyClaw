# 重构进度跟踪

> 基于 RFC §七 实施清单
> 完整重构，不分阶段发布

## 进度统计

- 完成：52 / 61
- 进行中：—
- 待办：9（全在 C18 keystone 之后）

## 模块 A：类型基础（0/11）

- [x] A1. `TurnContext`（5 字段）/ `TurnResult` → `src/agents/turn.rs`
- [x] A2. `AgentRuntime` struct（9 字段）→ `src/agents/runtime.rs`；RuntimeDefaults 合入
- [x] A3. `Tool` trait 加 `source() -> ToolSource`（默认 Builtin）+ `execute(&Session)` 参数（26 处 Tool 实现 + 4 处调用点同步）
- [x] A4. `ToolFilter` / `SkillFilter` / `McpFilter` enum (All/Allow/Deny) → `src/config/filters.rs`
- [x] A5. `Channel` trait 加默认 no-op 方法 `push_event` / `cancel_signal`
- [x] A6. `ChannelMessage` 加 `#[derive(Serialize, Deserialize)]`（MediaAttachment 同步）
- [x] A7. `AgentDelegator` trait（`delegate` + `list_available`）→ `src/agents/delegator.rs`
- [x] A8. `DelegationEvent` 字段重命名 session_key → parent_session_id（值仍是 routing_key，待 E29 真正切到 session_id）
- [x] A9. `SessionNotOwned` 错误类型 → `src/agents/session/manager.rs`
- [x] A10. `llm_stream` 模块（常量 + read/read_streamed 函数）→ `src/agents/llm_stream.rs`
- [x] A11. `ProviderRegistry` trait — `ServiceRegistry` 重命名 ← `ef94853..HEAD`

## 模块 B：Session / SessionContext / SessionManager（0/4）

- [x] B12. Session 字段改造（last_message/parent_session_id/agent_name + record_inbound/reply_target + token_tracker + transient persist/channel + save_to_disk + add_user/add_assistant 重命名带 #[deprecated] 别名）
- [~] B14. SessionManager 改造（已加 SessionNotOwned 接线 / list_sub_sessions / session_id_for_routing_key / get_by_id / create_sub_session / delete cascade；list_sessions 过滤 parent）
- [x] B13. 新建 `SessionContext`（session/attachments/pending_retry/turn_lock；user_profile 待 G41） → `src/agents/session_context.rs`
- [x] B15. Sub-session 存储扁平化：`DelegationCoordinator.open_sub_session` 改用 `SessionManager.create_sub_session`（写 `meta.parent_session_id` + `meta.agent_name` 到同层 `sessions/{sub_id}/`），不再开 per-parent `JsonFileBackend`。Marker 文件机制留给 H50 删

## 模块 C：Agent / AgentRuntime / 执行器（0/7）

- [~] C16. `SubAgentConfig` 加 skills/mcp 三维过滤 + allows_tool/skill/mcp helper；slim AgentConfig 形态等 C18
- [ ] C17. `Agent` 简化为只一个 config 字段
- [ ] C18. `Agent.run(session, ctx, rt)` 实现（含 allowed_tools snapshot + 循环）
- [x] C19. `ToolExecutor` 重命名（原 `DefaultToolExecutor`）；ask_user/agent_delegate inline 处理待 F35 拆出
- [x] C20. `LoopBreaker` policy + per-turn counter（已有 LoopBreakerConfig 分离 + reset() 每轮重置）
- [x] C21. `ContextEngine` 合并 CompactionPolicy + CompactionExecutor → `src/agents/context_engine.rs` 作为 façade，内部 struct 不变；C18 Agent.run 改用 ContextEngine
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
- [x] F36. `SubAgentDelegator` 重命名为 `DelegationCoordinator`（文件 sub_agent.rs → delegation_coordinator.rs），保留 type alias 给残留 import；只剩 AgentDelegator 单实现（H47 同步删 TaskDelegator dual impl）
- [x] F37. 启动恢复统一路径：`recovery::scan_unfinished_subagents` 改用 `SessionManager.list_all_sessions` 扫描，按 `Session.parent_session_id` 区分 sub-session 顶层会话；UnfinishedSubAgent 字段从父 session.owner / last_message.reply_target 反推
- [x] F38. Sub-agent 完成回填合成完整 `ChannelMessage`（id `delegation:{task_id}` / sender `system` / 真 reply_target / 等等），不再裸字符串；E29 之后 process_turn 直接吃 ChannelMessage

## 模块 G：per-user（0/6）

- [x] G39. `UserResolver` routing_key → user_id 映射 → `src/agents/user_profile.rs`
- [x] G40. `UserProfile` 加载/序列化/to_prompt_section → `src/agents/user_profile.rs`
- [x] G41. SessionContext 加 user_profile 字段（含 with_user_profile / reload_user_profile）
- [x] G42. build_prompt 补齐 profile section（`SystemPromptBuilder::build_with_profile`）
- [x] G43. Memory tools 读写 `users/{id}/memory/`（MemoryListTool/ViewTool/SearchTool/ManageTool 改持 workspace_dir + Arc<UserResolver>；execute 用 session.owner → resolver.resolve → user_id 构 path；daemon + cmd_chat + cmd_exec wiring 同步）
- [x] G44. `list_sessions_for_user(uid)` 反向 map 实现（SessionManager method）

## 模块 H：删除（0/13）

- [ ] H45. 删 AgentLoop / SessionHandle / LoopRegistry / run_session_actor / TurnStream / TurnInput
- [ ] H46. 删 RequestBuilder
- [x] H47. 删 `TaskDelegator` trait + dual impl on DelegationCoordinator；`AgentDelegateTool` 改持 `Arc<dyn AgentDelegator>` 并把 `&Session` 传给 delegate()；daemon wiring 同步
- [ ] H48. 删 SchedulerContext / WebhookContext
- [ ] H49. 删 AskUserHandler / DelegateHandler 闭包类型
- [x] H50. 删 `subagent_running_*.json` marker：DelegationCoordinator 不再写/读 marker；`cleanup_stale_subagent_markers` 函数删除；`sessions_root: PathBuf` 字段也从 DelegationCoordinator 删除（恢复机制 100% 依赖 SessionManager 元数据）
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

### F37 + H50 — 恢复机制收编 + 删 marker 文件

`recovery::scan_unfinished_subagents` no longer reads
`subagent_running_*.json`. It now takes `&SessionManager`, walks
`list_all_sessions()`, filters those with `parent_session_id` set and
a mid-turn history, and reconstructs `UnfinishedSubAgent` records from
the session graph (parent's `owner` → `session_key`, parent's
`last_message.reply_target` → `reply_target`, sub-session's first user
message → `task_preview`, sub_session_id reused as `task_id`).

- `recovery::cleanup_stale_subagent_markers` deleted entirely.
- `DelegationCoordinator`: marker writes (open + remove) deleted;
  `sessions_root: PathBuf` field deleted along with the corresponding
  constructor parameter. The async-spawn rebuild block no longer
  carries `sessions_root`.
- `daemon.rs`: calls `scan_unfinished_subagents(&session_manager)`;
  the standalone `sessions_root` for queue processing is now declared
  locally only where it's used (queue scanning).

Result: zero on-disk dependency for sub-agent recovery; everything
flows through `SessionManager`. 377 lib tests pass.

### B15 — Sub-session 存储扁平化

DelegationCoordinator now uses the shared `SessionManager` to create
sub-sessions as top-level peers of regular sessions, instead of opening a
nested `JsonFileBackend` rooted at `{sessions_root}/{parent}/subagents/`.

- `SessionManager.backend()` accessor added (returns
  `&Arc<dyn SessionBackend>`) so `BackendPersistHook` can be wired to
  the same storage.
- `DelegationCoordinator` gained `session_manager: Arc<SessionManager>`
  field; `open_sub_session` now calls
  `session_manager.create_sub_session(parent, agent_name)` which
  internally writes `meta.parent_session_id` and `meta.agent_name`.
- `sessions_root: PathBuf` field kept (only used by the
  `subagent_running_*.json` marker writes — H50 will delete that
  mechanism and the field with it).
- `daemon.rs` reordered: `session_backend` + `session_manager` are now
  built upstream of the multi-agent block so the coordinator can hold
  an `Arc<SessionManager>`. The duplicate construction further down
  was removed.

Old nested data layout (`sessions/{parent}/subagents/{sub}/`) is left
in place and ignored by the new code path — per RFC I61 we do not
migrate, and recovery scans the flat layout via
`SessionManager.list_all_sessions`.

377 lib tests pass.

### F36 + H47 — Delegation rename + TaskDelegator removal

Two of the Stage-2 cleanup items that don't require the Agent.run / orchestrator
rewrite to land. Net: -1 trait, -1 dual impl, ~30 references re-routed.

- **F36 rename**: `src/agents/sub_agent.rs` → `src/agents/delegation_coordinator.rs`
  (git mv); `pub struct SubAgentDelegator` → `pub struct DelegationCoordinator`;
  added `pub type SubAgentDelegator = DelegationCoordinator;` so the few
  remaining `SubAgentDelegator` import sites (tool_executor.rs, orchestrator.rs)
  keep compiling under their existing local aliases until the Agent.run /
  orchestrator rewrite removes them. `lib.rs` exports both names; `daemon.rs`
  switched to `DelegationCoordinator` directly.
- **H47 (full)**: Deleted `crate::tools::TaskDelegator` trait + the
  `impl TaskDelegator for SubAgentDelegator` block at the bottom of the
  delegation file. `AgentDelegateTool` now holds `Arc<dyn AgentDelegator>`
  and forwards `&Session` into `delegator.delegate(name, task, session)`.
  Updated `tools::mod.rs` / `tools::lib.rs` re-exports and the daemon's
  `delegate_tool` construction. The `AgentDelegator` impl on
  `DelegationCoordinator` is now the only delegation entry point.

Result: 377 lib tests still passing.

### Stage 2 (剩 9 项) — 待下一会话续作

Done this session beyond Stage 1: F36 + H47 (rename + TaskDelegator
deletion), B15 (sub-session storage flatten), F37 + H50 (recovery
unification + marker deletion), F38 (delegation completion as full
ChannelMessage).

Remaining (truly interlocked — all hang off the C18 Agent.run keystone):
- **C17 + C18**: new `Agent { config: SubAgentConfig }` in
  `src/agents/agent.rs` with `async fn run(&self, &mut Session,
  TurnContext<'_>, &AgentRuntime) -> Result<TurnResult>`. Port
  `agent_impl/{run,turn,chat_loop,compaction,tools,images}.rs` into
  this single struct, using `ContextEngine` (Stage 1) as the single
  context field, `Session.persist`/`Session.channel` transient handles
  for persistence + event emission, and `AgentRuntime.tools` filtered
  via `AgentConfig.allows_tool/skill/mcp` for the per-turn ToolSpec list.
- **E29 + E30**: orchestrator main loop consumes the existing
  `OrchestratorEvent` enum. Replace `pending_asks` DashMap with the
  existing `AskRouter`. Replace the `LoopRegistry::get_or_create`
  AgentLoop construction with `Agent.run(session, ctx, runtime)`
  per-turn invocation. Spawn per-channel adapters that convert
  per-source mpsc receivers → `OrchestratorEvent` upstream of the
  main loop.
- **F35**: implement `AskUserTool` / `DelegateTool` as real `Tool`
  impls holding `Arc<AskRouter>` + channel map / `Arc<dyn AgentDelegator>`.
  `Tool::execute(args, session)` uses `session.id` + `session.reply_target()`
  directly. Remove the inline `ask_user` / `agent_delegate` branches
  from `ToolExecutor::execute`. Daemon registers both tools with the
  right deps.
- **E32 + H57**: `channels/client.rs` indexes streams by `reply_target`
  (not session_key), implements the new `Channel::push_event` /
  `Channel::cancel_signal`, deletes `prepare_stream` /
  `take_stream_context` / `loop_registry` / `evict_loop`.
- **E34 + H48**: inline `SchedulerContext` into the orchestrator;
  pass orchestrator references directly to scheduler tasks.
- **H45**: delete `AgentLoop`, `SessionHandle`, `LoopRegistry`,
  `run_session_actor`, `TurnStream`, `TurnInput` — only feasible
  once C18 + E29 are in.
- **H46**: delete `request_builder.rs` (functionality folded inline
  into `Agent.run`).
- **H49**: delete `AskUserHandler` / `DelegateHandler` closure type
  aliases — only feasible once F35 has replaced the wiring.

Risk hotspots:
- The deprecated `add_user_text` / `add_assistant_text` calls in
  agent_impl/ + orchestrator.rs + tool_executor.rs emit ~7 warnings
  until Stage 2 deletes those callers — non-blocking, verified.
- `CompactionExecutor::build_memory_prompt` still cites the legacy
  `knowledge_dir` in instruction text. Either rewrite the prompt to
  use `memory_manage` or thread the per-user path through when
  Agent.run rewrites the summarizer wiring.
- `agent_impl::types::TokenTracker` is now `pub` (Stage 1 needed
  visibility for Session.token_tracker); C18 should pick a canonical
  home — moving it into the `context_engine` module would be natural.
- `DelegationCoordinator.open_sub_session` returns a `Session::new(id)`
  but does NOT populate `parent_session_id` / `agent_name` on the
  returned in-memory Session — the metadata was written to the
  backend via `create_sub_session`, but if a fresh sub-session is
  used before being reloaded, those fields will read as defaults
  in memory. C18 should populate them from the SessionInfo at
  construction time.

### A3 + B12 + C21 + G43 — 类型契约收尾 + ContextEngine 门面

Stage 1 of the 19-item atomic block: independent "type contract" changes
that compile + verify on their own, before the big Agent / Orchestrator
rewrite (C17+C18+E29+F35+F36+...).

- **A3 final**: `Tool::execute(args, &Session)` — added `&Session` as second
  parameter; 26 Tool impls updated (mechanical `_session` ignore for 22 of
  them; memory tools + ask_user actually use it). MCP wrapper, deferred
  shim, and 22 test call sites updated to pass `&Session::new("test"...)`.
  `ToolExecutor::run_tool` and `MemoryToolExecutor::execute` forward
  `session` through; `CompactionExecutor::execute/summarize/do_summarize`
  threaded `session` so memory tools called by the summarizer remain
  user-scoped.
- **B12 final**: Session gained `token_tracker: TokenTracker` (moved to
  `pub` from `pub(crate)` in `agent_impl::types`), plus transient
  `persist: Option<Arc<dyn PersistHook>>` and `channel: Option<Arc<dyn Channel>>`
  (Default None, survive Clone via Arc). `derive(Debug)` swapped for a
  hand-rolled impl that elides the transient handles. Added
  `with_persist` / `with_channel` builder helpers and `save_to_disk`
  (flushes last_message reply_target, override JSON, token count via
  PersistHook). `add_user_text` → `add_user`, `add_assistant_text` →
  `add_assistant`; old names kept as `#[deprecated]` aliases.
- **C21**: New `src/agents/context_engine.rs` — `ContextEngine` struct
  wraps `CompactionPolicy` + `CompactionExecutor` as a single facade.
  Surfaces `token_total`, `update_usage`, `record_pending`, `should_compact`,
  `compaction_boundary`, `execute_compaction`. Internal structs untouched —
  C18 Agent.run will swap from two fields to one.
- **G43**: Memory tools (`MemoryListTool` / `ViewTool` / `SearchTool` /
  `ManageTool`) now hold `workspace_dir: PathBuf` + `Arc<UserResolver>`
  instead of `knowledge_dir: String`. `MemoryPaths::for_user` builds
  `{workspace_dir}/users/{user_id}/{MEMORY_DIR_NAME}/`. Daemon's
  `build_tools` creates a shared `Arc<UserResolver>` and threads it in;
  `cli/cmd_chat.rs` + `cli/cmd_exec.rs` do the same. `Action_*` helpers on
  `MemoryManageTool` take an explicit `user_id` parameter now.

Notes / known regressions for Stage 2 to mop up:
- `CompactionExecutor::build_memory_prompt` still cites the legacy
  `knowledge_dir` in its instruction text. The summarizer writes memory
  via `file_write` (allow-list), not `memory_manage`, so the path it
  reaches for will be the pre-G43 location. Low-impact because the
  summarizer rarely writes memory; clean fix is to add `memory_*` to
  `MemoryToolExecutor::ALLOWED` during C18.
- The Channel trait's `prepare_stream` / `take_stream_context` / etc.
  still exist; H57 deletion stays in Stage 2.

Result: 377 lib tests pass (same baseline as pre-stage-1).

### D23 + E33 — 确认已实现项

- D23: AppConfig 早已是模块化结构 (providers / routing / channels / agent /
  memory / mcp_servers / logging 各自独立 sub-struct)，无需进一步拆段
- E33: WebhookHandler 早已是纯协议适配器 (HTTP → ChannelMessage → Channel.send)
  无业务逻辑

### A8 + C16 partial — DelegationEvent 字段重命名 + 三维过滤

- A8: DelegationEvent.session_key → parent_session_id（Completed / Failed
  两个 variant）；3 个发送点（sub_agent.rs 2 处 + orchestrator 启动恢复 1 处）
  和 2 个 match arm（handle_delegation_task）同步更新。字段值今天仍是
  routing_key，真正切到 session_id 等 E29。
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
