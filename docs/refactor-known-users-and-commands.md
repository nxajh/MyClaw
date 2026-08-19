# 统一用户注册表 + 命令系统重构

## 目标

1. **KnownUsersRegistry**：将 QQBot 内部的 `KnownSenders` + `RateLimiter` 提升为全局注册表，所有 channel 共享
2. **统一命令系统**：删除 QQBot 的 `try_bot_command`，所有 `/bot-*` 命令迁移到 orchestrator 的 slash command 体系

---

## Phase 1：创建 KnownUsersRegistry 模块

**新建 `src/agents/known_users.rs`**

### 数据结构

```rust
/// 全局用户注册表 — 所有 channel 共享。
/// 记录每个 routing_key 对应的已知用户，兼做限流。
pub struct KnownUsersRegistry {
    users: DashMap<String, KnownUser>,
    rate_buckets: DashMap<String, (u32, u64)>,        // routing_key → (count, window_start_ms)
    global_count: AtomicU32,
    global_window_start: AtomicU64,
    data_path: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KnownUser {
    pub channel: String,       // "qqbot" / "telegram" / "wechat"
    pub account: String,       // "xiaoer" / "default"
    pub user_id: String,       // openid / chat_id / wxid
    pub message_count: u32,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub scope: String,         // "c2c" / "group:{group_id}"
}
```

### 接口

```rust
impl KnownUsersRegistry {
    pub fn new(base_dir: &Path) -> Self;
    
    /// 限流检查 + 用户登记。返回 false = 被限流，调用方应丢弃消息。
    /// 限流维度：per-routing_key (30/min) + global (300/min)。
    pub fn check_and_record(&self, channel: &str, account: &str, user_id: &str, scope: &str) -> bool;
    
    /// 仅登记不限流（用于 INTERACTION_CREATE 等不需要限流的事件）。
    pub fn record(&self, channel: &str, account: &str, user_id: &str, scope: &str);
    
    pub fn count(&self) -> usize;
    pub fn total_messages(&self) -> u32;
    pub fn users_for(&self, channel: &str, account: &str) -> Vec<KnownUser>;
    pub fn all_users(&self) -> Vec<KnownUser>;
    pub fn flush(&self);   // 落盘 known_users.json
    
    /// 从旧格式 qqbot_known_users_{account}.json 迁移。
    pub fn migrate_legacy(&self, base_dir: &Path);
}
```

### 持久化

- 文件：`{base_dir}/known_users.json`
- 格式：`{ "version": 1, "users": { routing_key: KnownUser, ... } }`
- 启动时：先尝试加载 `known_users.json`；不存在则扫描 `qqbot_known_users_*.json` 迁移

**修改 `src/agents/mod.rs`**：加 `pub mod known_users; pub use known_users::{KnownUsersRegistry, KnownUser};`

**验证**：`cargo check` 编译通过，单元测试覆盖 record/limit/flush/migrate。

---

## Phase 2：将 registry 注入 orchestrator

### `src/agents/orchestrator/ctx.rs`

```diff
 pub struct OrchestratorCtx {
     pub channels: ChannelRegistry,
     pub sessions: Arc<SessionManager>,
     pub ask: Arc<AskRouter>,
+    pub known_users: Arc<KnownUsersRegistry>,
     pub runtime: AgentRuntime,
     pub delegator: Option<Arc<DelegationCoordinator>>,
     pub scheduler: Option<SharedScheduler>,
     pub turn_tracker: SharedTurnTracker,
 }
```

### `src/agents/orchestrator/mod.rs`

- `OrchestratorParts` 加 `pub known_users: Arc<KnownUsersRegistry>`
- `Orchestrator::new()` 中 `ctx` 构造加 `known_users: parts.known_users`
- `run()` 中加 60s flush task（替换 QQBot channel 内的 flush task）

### `src/agents/orchestrator/inbound.rs`

`dispatch()` 函数（L62）在 `SessionKey::new` 之前加：

```rust
// 限流 + 用户登记（原散落在各 channel 内部）
let scope = if msg.receiver.id.starts_with("group:") {
    format!("group:{}", &msg.receiver.id[6..])
} else {
    "c2c".to_string()
};
if !ctx.known_users.check_and_record(&account.0, &account.1, &msg.sender.id, &scope) {
    tracing::warn!(
        channel = %account.0, account = %account.1,
        sender = %msg.sender.id, "rate limited, dropping"
    );
    return;
}
```

### `src/agents/orchestrator/test_support.rs`

构造 `OrchestratorCtx` 时加 `known_users: Arc::new(KnownUsersRegistry::new_in_memory())`

### `src/daemon.rs`

```rust
// 在创建 ask_router 附近（~L915）
let known_users = Arc::new(KnownUsersRegistry::new(&base_dir));
known_users.migrate_legacy(&base_dir);
```

`OrchestratorParts` 构造（~L1205）加 `known_users: Arc::clone(&known_users)`

### `src/channels/message.rs`

Channel trait 加方法：

```rust
/// 群组统计（仅 group-capable channel 有数据）。
fn group_stats(&self) -> Vec<GroupStat> { vec![] }
```

新增类型：

```rust
#[derive(Debug, Clone)]
pub struct GroupStat {
    pub group_id: String,
    pub name: Option<String>,
    pub buffered_messages: usize,
    pub history_limit: usize,
}
```

**验证**：`cargo check` + `cargo test`。此时 QQBot 内部的 KnownSenders/RateLimiter 仍然存在（双写），orchestrator 也开始记录。功能不破坏。

---

## Phase 3：扩展 CommandContext + 新增统一命令

### `src/agents/commands/mod.rs`

扩展 `CommandContext`：

```diff
 pub struct CommandContext<'a> {
     pub user_id: &'a str,
+    pub routing_key: &'a str,
+    pub channel_type: &'a str,
+    pub account_id: &'a str,
     pub registry: &'a Arc<dyn ProviderRegistry>,
     pub session_manager: &'a SessionManager,
     pub runtime: &'a AgentRuntime,
     pub session_ctx: Option<&'a Arc<SessionContext>>,
+    pub known_users: &'a Arc<KnownUsersRegistry>,
+    pub channel: Option<&'a Arc<dyn Channel>>,
 }
```

`dispatch()` 的构造处（inbound.rs L314）传入新字段：

```rust
let cmd_ctx = commands::CommandContext {
    user_id: &sk,
    routing_key: &sk,
    channel_type: &key.channel,
    account_id: &key.account,
    registry: &registry_cmd,
    session_manager: &sm_cmd,
    runtime: &runtime_cmd,
    session_ctx: session_ctx_cmd.as_ref(),
    known_users: &ctx.known_users,
    channel: ctx.channel(&key.account_key()).as_ref(),
};
```

### 新增命令

| 命令 | 文件 | 说明 |
|---|---|---|
| `/ping` | info.rs | 返回 `pong 🏓` |
| `/whoami` | info.rs | 显示 routing_key + scope |
| `/users` | info.rs | 从 known_users 读统计 |
| `/groups` | info.rs | 从 `ctx.channel.group_stats()` 读 |

`is_known_command` / `command_catalog` / `dispatch` 同步注册。

### `/status` 合并

当前 `cmd_status` 只显示 provider 状态。扩展为两段：

```
📊 Provider 瞬时状态
（现有 provider 表格）

📊 系统状态
• Version: myclaw 0.1.0 (abc1234)
• Known users: 42 (跨 3 channel)
• Total messages: 1,234
```

读取 `ctx.known_users.count()` / `total_messages()`。

### `/help` 扩展

在现有帮助文本末尾加新命令说明。

**验证**：`cargo check` + `cargo test`。新命令通过 orchestrator 可用。此时 QQBot 的 `/bot-*` 命令仍然存在（双路可用）。

---

## Phase 4：删除 QQBot 内部 KnownSenders / RateLimiter / try_bot_command

### `src/channels/qqbot/channel.rs`

**删除 struct + impl**：
- `KnownSenders` struct + impl（~L2511-2583）
- `RateLimiter` struct + impl（~L2588-2656）
- `UserEntry` struct（~L2517-2522）

**删除 field**（QQBotChannel struct ~L348-377）：
- `rate_limiter: Arc<Mutex<RateLimiter>>`
- `known_senders: Arc<Mutex<KnownSenders>>`
- `base_dir: std::path::PathBuf`

**删除 method**：
- `known_users_path()`（~L452）
- `save_known_users()`（~L458）

**删除 `new()` 中的逻辑**：
- base_dir 获取 + known_users JSON 加载（~L387-417）
- `rate_limiter` / `known_senders` / `base_dir` 字段初始化

**删除 `listen()` 中的 flush task**（~L2334-2343）

**删除入站消息中的调用**：
- C2C: `self.rate_limiter.lock().check()`（L653）→ 删
- C2C: `self.known_senders.lock().record()`（L660）→ 删
- GROUP: `self.rate_limiter.lock().check()`（L686）→ 删
- GROUP: `self.known_senders.lock().record()`（L708-710）→ 删

**删除 `try_bot_command` 方法整体**（L1789-1973）：
- 包括 `/bot-ping` `/bot-version` `/bot-help` `/bot-status` `/bot-me` `/bot-groups` `/bot-clear` `/bot-users` `/bot-approve`
- 包括底部的 sanitize + chunk + send 逻辑

**删除 `ws_loop` 中的调用**（~L3003-3015）：
```rust
// 删掉这段
if self.try_bot_command(...).await {
    return None;
}
```

**删除测试**：
- `rate_limiter_blocks_after_limit`（L3418-3431）

**修改 `test_channel`**（L3257-3280）：
- 删除 `known_senders` / `rate_limiter` / `base_dir` 字段

**实现 `group_stats()`**：
```rust
fn group_stats(&self) -> Vec<crate::channels::GroupStat> {
    let history = self.group_history.lock();
    history.iter().map(|(gid, msgs)| {
        crate::channels::GroupStat {
            group_id: gid.chars().take(12).collect(),
            name: self.config.group_config.get(gid)
                .and_then(|c| c.name.clone())
                .or_else(|| self.config.group_config.get("*").and_then(|c| c.name.clone())),
            buffered_messages: msgs.len(),
            history_limit: self.resolve_group_history_limit(gid),
        }
    }).collect()
}
```

**验证**：`cargo check` + CI。QQBot 编译通过，`/bot-*` 命令不再存在，统一命令接管。

---

## Phase 5：数据迁移 + 清理

### `src/agents/known_users.rs`

`migrate_legacy()` 实现：

1. 扫描 `{base_dir}/qqbot_known_users_*.json`
2. 每个文件解析为 `HashMap<String, UserEntry>`（旧格式）
3. account_id 从文件名提取（`qqbot_known_users_{account}.json`）
4. 转换为 `KnownUser`（channel="qqbot", account=提取值, user_id=旧 key）
5. 合并写入 `known_users.json`
6. **不删除旧文件**（用户确认后再删）

### flush task

`orchestrator/mod.rs run()` 中：

```rust
let registry = Arc::clone(&self.ctx.known_users);
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.tick().await; // 跳过首次
    loop {
        interval.tick().await;
        registry.flush();
    }
});
```

---

## 文件改动汇总

| 文件 | 操作 | 行数估算 |
|---|---|---|
| **新建** `src/agents/known_users.rs` | 创建 KnownUsersRegistry + KnownUser + 限流 + 持久化 + 迁移 | +300 |
| `src/agents/mod.rs` | 加 pub mod + re-export | +3 |
| `src/agents/orchestrator/ctx.rs` | 加 known_users 字段 | +3 |
| `src/agents/orchestrator/mod.rs` | OrchestratorParts 加字段 + flush task | +15 |
| `src/agents/orchestrator/inbound.rs` | dispatch 加 check_and_record + CommandContext 构造 | +20 |
| `src/agents/orchestrator/test_support.rs` | 测试 ctx 加 registry | +3 |
| `src/agents/commands/mod.rs` | CommandContext 扩展 + 注册新命令 | +30 |
| `src/agents/commands/info.rs` | cmd_ping / cmd_whoami / cmd_users / cmd_groups + cmd_status 扩展 | +100 |
| `src/daemon.rs` | 创建 registry + 传入 parts | +10 |
| `src/channels/message.rs` | Channel trait 加 group_stats() + GroupStat | +15 |
| `src/channels/qqbot/channel.rs` | 删 KnownSenders/RateLimiter/try_bot_command + 实现 group_stats | -400 +20 |

净变化：删除 ~400 行 QQBot 内部代码，新增 ~500 行全局统一代码。

---

## 命令映射对照

| 旧 QQBot 命令 | 新统一命令 | 数据来源 |
|---|---|---|
| `/bot-ping` | `/ping` | 无依赖 |
| `/bot-version` | 合并到 `/status` | env! MYCLAW_VERSION |
| `/bot-help` | `/help` | 已有 |
| `/bot-status` | `/status` | known_users + provider registry |
| `/bot-me` | `/whoami` | routing_key (CommandContext) |
| `/bot-groups` | `/groups` | channel.group_stats() |
| `/bot-clear` | `/new` | 已有 |
| `/bot-users` | `/users` | known_users |
| `/bot-approve` | `/autonomy` | 已有 |

---

## 依赖顺序

```
Phase 1 (新建模块，零破坏)
    ↓
Phase 2 (注入 orchestrator，双写期)
    ↓
Phase 3 (新命令就绪，双路可用)
    ↓
Phase 4 (删 QQBot 内部代码，切单路)
    ↓
Phase 5 (迁移 + flush)
```

每个 Phase 结束后 `cargo check` + `cargo test` + CI 绿灯才进下一步。
