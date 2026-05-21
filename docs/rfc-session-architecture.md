# RFC: Session 架构重构

> 状态：草案
> 日期：2026-05-21
> 背景：WebUI sessions.switch bug 修复过程中发现的设计问题

## 问题

### 1. AgentLoop 不应持有 Session

当前 `AgentLoop` 通过 ownership 持有 `Session`（history + metadata），而 Session 的生命周期管理（创建/切换/删除）属于更上层的 `SessionManager`。这导致：

- `SessionManager.switch_session()` 更新了 active 指针，但 AgentLoop 仍持有旧 Session
- 需要 evict 机制（`LoopRegistry.remove()`）手动同步两层缓存
- 本质是把 SessionManager 的职责拆成了两个地方

### 2. 两层缓存的同步问题

```
SessionManager.cache   → 缓存"还没被 AgentLoop 认领"的 Session 数据
LoopRegistry.sessions  → 缓存运行中的 AgentLoop（内含 Session）
```

`get_or_create()` 将 Session 从第一层 move 到第二层后，两层数据分家，sync 只能靠人工 evict。

### 3. user_id 名不副实

当前 `user_id` 实际是路由 key（`"telegram:bot1:12345"`、`"client:default:ws_conn_abc"`），不是真正的用户标识。WebUI 开两个标签页就是两个"用户"，session 互不可见。

### 4. `get_or_create` 做了太多事

一个方法承担了：创建 Session → 构建 AgentLoop → 绑 ask_user/delegate handler → spawn actor task → 存缓存。所有初始化逻辑耦合在一起。

### 5. 不必要的 actor + channel 间接层

消息流转经过三层间接：

```
orchestrator → tx.send(msg) → actor task 从 rx 取出 → mutex.lock(AgentLoop) → run()
```

`run_session_actor` 只是一个 `while let` 循环 + `Mutex<AgentLoop>` 访问，纯粹为了串行化。Actor task 拿着 `Arc<Mutex<AgentLoop>>`，但同一时刻只有它自己在用这个 mutex。

## 方案

### 核心原则

- **AgentLoop 是无状态的执行器**：不持有 Session，turn 开始时拿到 history，turn 结束时写回结果
- **SessionContext 拥有一切**：Session 数据 + 执行能力 + 串行化保证，归属权单一
- **去掉 LoopRegistry**：不再有平行于 SessionManager 的缓存层

### 目标结构

```
SessionManager (全局单例)
  ├─ backend: Arc<dyn SessionBackend>    ← 持久化
  ├─ persist_hook: Arc<dyn PersistHook>  ← 全局共享，不再 per-AgentLoop 创建
  ├─ active: HashMap<UserId, SessionId>  ← 当前活跃 session
  └─ contexts: HashMap<SessionId, Arc<SessionContext>>

SessionContext (per session)
  ├─ session: Mutex<Session>             ← history + metadata
  ├─ channel: Arc<dyn Channel>           ← 发消息给用户
  ├─ mutex: tokio::sync::Mutex<()>       ← turn 串行化
  └─ agent: &Agent                       ← 无状态执行器引用

AgentLoop (无状态，全局共享或 per-turn 临时)
  ├─ registry: &ServiceRegistry
  ├─ config: &AgentConfig
  ├─ tool_executor: &DefaultToolExecutor
  └─ fn turn(session: &mut Session, msg: &str) → TurnResult
```

### 消息流转（重构后）

```
orchestrator → session_manager.get_context(session_id) → ctx.process_turn(msg)

process_turn:
  self.mutex.lock().await          ← 串行化
  agent.run(&mut self.session, &msg)  ← 无状态执行
```

一层调用，没有 channel/actor 间接层。

### session 操作

```rust
switch_session(user_id, session_id) {
    // 1. 切 active 指针
    self.active.insert(user_id, session_id);
    // 下次 process_turn 自动用新 context，无需 evict
}

delete_session(user_id, session_id) {
    // 1. drop SessionContext → 资源清理
    self.contexts.remove(session_id);
    // 2. 如果删的是 active，切到默认 session
}

create_session(user_id, name) {
    // 1. 创建 SessionContext
    // 2. 设为 active
}
```

天然一致，没有 evict，没有两层缓存同步。

### 关键改动点

| 组件 | 现在 | 重构后 |
|------|------|--------|
| `AgentLoop` | 持有 `Session` ownership | 无状态，接收 `&mut Session` |
| `LoopRegistry` | `DashMap<sk, SessionHandle>` 独立缓存 | 删除，职责回归 `SessionManager` |
| `SessionHandle` | `Arc<Mutex<AgentLoop>> + msc::Sender` | 删除，`SessionContext` 自身保证串行化 |
| `run_session_actor` | `while let` + channel 消费 | 删除，`process_turn` 直接调用 |
| `ask_user` handler | per-session 闭包捕获 Arc | `SessionContext` 的方法 |
| `delegate` handler | per-session 闭包捕获 Arc | `SessionContext` 的方法 |
| `persist_hook` | 每次 `get_or_create` 新建 | 全局共享单例 |
| `user_id` | 路由 key 混用 | 拆分为 `UserId` + `ChannelEndpoint`（远期） |

### 未覆盖：user_id 拆分

当前 `user_id` 实际是 `format!("{}:{}:{}", channel_type, account_id, sender)`，同时承担路由和身份两个职责。远期应拆分为：

```
user_id:      真实用户身份（"albert"）
channel_endpoint: 通道端点（"telegram:bot1:12345"）
```

这涉及通道层认证体系，不在本次重构范围内。

## 实施策略

### 阶段 1：AgentLoop 解耦 Session（核心）

1. `AgentLoop.run()` 改为接收 `&mut Session` 参数而非从 `self.session` 读取
2. `Session` 不再作为 `AgentLoop` 字段，改为外部传入
3. 验证所有 tool 对 session 的访问方式是否兼容

### 阶段 2：SessionContext + 去掉 LoopRegistry

1. 新建 `SessionContext` 结构体
2. 将 `LoopRegistry.get_or_create()` 的逻辑迁移到 `SessionManager/SessionContext`
3. 去掉 `run_session_actor`，用 `SessionContext.process_turn()` 替代
4. `ask_user`/`delegate` handler 从闭包改为 `SessionContext` 方法

### 阶段 3：清理

1. 删除 `SessionHandle`、`LoopRegistry`、`run_session_actor`
2. 清理 orchestrator 中的 `sessions: DashMap` 字段
3. 更新 `SharedSessions` 和所有消费方

### 当前 workaround

在阶段 1 完成前，session switch/create/delete 的 API handler 通过 evict `LoopRegistry` 保证一致性（commit `a7a31ff`）。
