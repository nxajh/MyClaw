# 多 API Key 轮转接通方案

## 问题

同一 provider 配置多个 API key 时，基础设施代码（CredentialPool）已完整但未接线：
- `effective_api_keys()` 从未被调用（只调单数版）
- `SharedCredentialPool` 从未被实例化
- `FallbackEntry.credential_pool` 硬编码 `None`
- provider 在构造时固化 key，运行时无法替换

## 设计概述

```
daemon.rs
  │  effective_api_keys() → ["key1", "key2", "key3"]
  │  创建 SharedApiKey (Arc<RwLock<String>>)
  │  创建 SharedCredentialPool (含全部 key)
  │
  ├─→ BuildChatProviderRequest { api_key: SharedApiKey }
  │     └─→ ProviderFactory → GlmProvider { api_key: SharedApiKey }
  │                                ↑ .get() 每次请求读最新 key
  │
  └─→ registry.attach_credential_pool(model_id, pool, shared_key)
        └─→ maybe_wrap_chat_fallback()
              └─→ FallbackEntry { pool, shared_key }

fallback.rs 内层循环:
  RateLimit → mark_exhausted(old_key) → next_credential() → shared_key.set(new_key) → 同 provider 重试
  全部 key 耗尽 → failover 到下一个 provider
```

## 新增类型

### `SharedApiKey` — 共享可变 API key

放在 `credential_pool.rs` 末尾（与 pool 同文件）:

```rust
use std::sync::RwLock;

/// 线程安全的共享 API key，允许 credential pool 在运行时替换 key。
#[derive(Clone)]
pub struct SharedApiKey(Arc<RwLock<String>>);

impl SharedApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(Arc::new(RwLock::new(key.into())))
    }

    /// 读取当前 key（每次 HTTP 请求前调用）。
    pub fn get(&self) -> String {
        self.0.read().unwrap().clone()
    }

    /// 写入新 key（credential 轮转时调用）。
    pub fn set(&self, key: impl Into<String>) {
        *self.0.write().unwrap() = key.into();
    }
}

impl From<String> for SharedApiKey {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl std::fmt::Debug for SharedApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = self.get();
        let masked = if key.len() > 8 {
            format!("{}...{}", &key[..4], &key[key.len()-4..])
        } else {
            "***".to_string()
        };
        f.debug_tuple("SharedApiKey").field(&masked).finish()
    }
}
```

## 改动清单（按文件）

### 1. `credential_pool.rs` — 新增 SharedApiKey + re-export

在文件末尾（tests 之前）添加上面的 `SharedApiKey` 定义。

### 2. `providers/mod.rs` — 导出 SharedApiKey

```rust
pub use credential_pool::{
    CredentialEntry, CredentialPool, CredentialStatus, RotationStrategy,
    SharedCredentialPool, SharedApiKey,  // ← 新增
};
```

### 3. 协议客户端 — `String` → `SharedApiKey`

**`protocols/openai/chat_completions.rs`** (3 处):

```rust
// 字段
api_key: SharedApiKey,          // 原: api_key: String

// 构造函数
pub fn new(api_key: impl Into<SharedApiKey>, base_url: String) -> Self {  // 原: api_key: String
    Self { api_key: api_key.into(), ... }
}

// auth() — 唯一读取点
fn auth(&self) -> String {
    format!("Bearer {}", self.api_key.get())  // 原: self.api_key
}
```

**`protocols/anthropic/messages.rs`** (3 处):

```rust
// 字段
api_key: SharedApiKey,

// 构造函数
pub fn new(api_key: impl Into<SharedApiKey>, base_url: String) -> Self { ... }

// chat_with_body() — 唯一读取点
let api_key = self.api_key.get();  // 原: self.api_key.clone()
```

**`protocols/google/generate_content.rs`** (3 处):

```rust
// 字段
api_key: SharedApiKey,

// 构造函数
pub fn new(api_key: impl Into<SharedApiKey>, base_url: String) -> Self { ... }

// URL 构建 — 唯一读取点
let url = format!("{}?key={}", full_path, self.api_key.get());  // 原: self.api_key
```

### 4. Provider wrappers — `String` → `SharedApiKey`

**`glm.rs`** (3 处):

```rust
api_key: SharedApiKey,

pub fn with_base_url(api_key: impl Into<SharedApiKey>, base_url: String) -> Self { ... }

fn auth(&self) -> String {
    crate::providers::shared::build_auth(
        &AuthStyle::Bearer,
        &self.api_key.get(),        // 原: &self.api_key
    )
}
```

**`xiaomi.rs`** (3 处):

```rust
api_key: SharedApiKey,

pub fn with_base_url(api_key: impl Into<SharedApiKey>, base_url: String) -> Self { ... }

// chat() 内部创建 inner client 时:
let api_key = self.api_key.get();   // 原: self.api_key.clone()
AnthropicMessagesClient::new(api_key, self.base_url.clone())
```

### 5. `provider_factory.rs` — 请求类型改用 SharedApiKey

```rust
pub struct BuildChatProviderRequest {
    pub provider_key: String,
    pub provider_id: ProviderId,
    pub protocol: Option<Protocol>,
    pub base_url: String,
    pub api_key: SharedApiKey,       // 原: pub api_key: String
    pub auth_style: AuthStyle,
    pub user_agent: Option<String>,
}
```

工厂内部各分支无需改动——它们已经将 `request.api_key` 传给构造函数，
而构造函数现在接受 `impl Into<SharedApiKey>`，`SharedApiKey` 本身实现 `From` 恒等传递。

> **注意**: embedding/image/tts/search 的 Request 暂不改（这些 capability 限流较少），
> 后续可按同样模式扩展。

### 6. `fallback.rs` — 内层 credential 重试循环（核心改动）

`FallbackEntry` 新增字段:

```rust
pub struct FallbackEntry {
    pub provider: Arc<dyn ChatProvider>,
    pub model_id: String,
    pub credential_pool: Option<SharedCredentialPool>,
    pub shared_api_key: Option<SharedApiKey>,    // ← 新增
}
```

`chat()` 方法中，将每个 entry 的处理改为可内层重试:

```rust
for entry in &chain {
    // ── Cooldown gate（不变）──
    { ... skip if on cooldown ... }
    any_attempted = true;

    let max_retries = entry.credential_pool.as_ref().map(|p| p.len()).unwrap_or(1);

    let mut credential_exhausted = false;

    for _attempt in 0..max_retries {
        let req = ChatRequest { model: &entry.model_id, ... };
        let stream = match entry.provider.chat(req) {
            Ok(s) => s,
            Err(e) => { ... 不变 ... }
        };

        let mut should_failover = false;
        let mut should_rotate = false;

        while let Some(event) = inner_stream.next().await {
            match &event {
                StreamEvent::HttpError { status, message } => {
                    let classified = ClassifiedError::classify(...);
                    if classified.should_rotate_credential
                        && entry.credential_pool.is_some()
                    {
                        should_rotate = true;
                        break;  // 跳出 stream drain，进入 credential 轮转
                    }
                    // 原有逻辑: is_provider_error → failover, etc.
                    ...
                }
                StreamEvent::Error(msg) => { ... 同上 ... }
                _ => { tx.send(event).await; }
            }
        }

        if should_rotate {
            // ── Credential 轮转 ──
            if let (Some(ref pool), Some(ref key)) = (&entry.credential_pool, &entry.shared_api_key) {
                let old_key = key.get();
                pool.mark_exhausted(&old_key, &classified.reason);
                match pool.next_credential() {
                    Some(new_key) => {
                        key.set(new_key);
                        tracing::info!(
                            model = %entry.model_id,
                            "credential rotated, retrying same provider"
                        );
                        continue;  // ← 内层循环：同 provider 重试
                    }
                    None => {
                        tracing::warn!(
                            model = %entry.model_id,
                            "all credentials exhausted, failing over"
                        );
                        credential_exhausted = true;
                        break;  // 全部 key 耗尽，跳到下一个 provider
                    }
                }
            }
        }

        if !should_failover && !credential_exhausted {
            // 正常完成
            return;
        }
        break;  // 跳到下一个 provider
    }

    // 记录 cooldown（不变）
    if credential_exhausted {
        record_cooldown(&cooldowns, &entry.model_id, &classified);
    }
}
```

### 7. `daemon.rs` — 创建 pool 并接线

替换现有的 chat provider 构建段落:

```rust
// ── Chat ──────────────────────────────────────────────────────
if let Some(ref chat) = provider_cfg.chat {
    let api_keys = provider_cfg.effective_api_keys(chat.api_key.as_deref());
    anyhow::ensure!(!api_keys.is_empty(), "no API key for '{}'", provider_key);

    // 创建共享 key cell（所有 key 共用一个 cell）
    let shared_key = SharedApiKey::new(api_keys[0].clone());

    // 如果有多个 key，创建 credential pool
    let pool = if api_keys.len() > 1 {
        let p = CredentialPool::new(
            provider_key.clone(),
            api_keys.clone(),
            provider_cfg.rotation_strategy,
        );
        let shared = SharedCredentialPool::new(p);
        tracing::info!(
            provider = %provider_key,
            key_count = api_keys.len(),
            strategy = ?provider_cfg.rotation_strategy,
            "multi-key credential pool created"
        );
        Some(shared)
    } else {
        None
    };

    let auth_style = provider_cfg.effective_auth_style(chat.auth_style);
    let user_agent = chat.user_agent.clone();

    for (model_id, model_cfg) in &chat.models {
        let request = BuildChatProviderRequest {
            provider_key: provider_key.clone(),
            provider_id: provider_id.clone(),
            protocol: chat.protocol,
            base_url: chat.base_url.clone(),
            api_key: shared_key.clone(),        // ← 共享 cell
            auth_style: auth_style.into(),
            user_agent: user_agent.clone(),
        };

        let chat_provider = factory.build_chat_provider(request)?;
        registry.register_chat(chat_provider, model_id.clone(), model_cfg.clone(),
            Some(provider_id.clone()), chat.protocol);

        // 注册 credential pool 供 fallback chain 使用
        if let Some(ref pool) = pool {
            registry.attach_credential_pool(model_id, pool.clone(), shared_key.clone());
        }
    }
}
```

### 8. `registry/mod.rs` — 存储 pool + 传递给 FallbackEntry

Registry 新增字段和方法:

```rust
pub struct Registry {
    // ...existing fields...
    credential_pools: HashMap<String, (SharedCredentialPool, SharedApiKey)>,
}

impl Registry {
    pub fn attach_credential_pool(
        &mut self,
        model_id: &str,
        pool: SharedCredentialPool,
        key: SharedApiKey,
    ) {
        self.credential_pools.insert(model_id.to_string(), (pool, key));
    }
}
```

`maybe_wrap_chat_fallback` 中构建 FallbackEntry 时查找 pool:

```rust
chain.push(FallbackEntry {
    provider: Arc::clone(provider),
    model_id: model_id.clone(),
    credential_pool: self.credential_pools.get(model_id).map(|(p, _)| p.clone()),
    shared_api_key: self.credential_pools.get(model_id).map(|(_, k)| k.clone()),
});
```

### 9. `config/mod.rs` — `api_keys` 环境变量展开

在 `expand_env_vars()` 中新增:

```rust
// 展开 provider-level api_keys
for key in &mut provider.api_keys {
    *key = Self::expand_string(key);
}
```

## 配置示例

```toml
[providers.glm]
api_keys = [
    "8f85d63bbf2e4d1bab130d4f11de0f2e.bWJCa91UogKQIqPk",
    "9a96e74ccg3f5e2cbc241e5g22ef1g3f.cXKDp02VpHxRJrQl",
    "${GLM_BACKUP_KEY}",        # 环境变量也支持
]
rotation_strategy = "fill_first"   # 默认: fill_first | round_robin | random | least_used

[providers.glm.chat]
base_url = "https://open.bigmodel.cn/api/paas/v4"
# 单 key 向后兼容: api_key = "xxx" 等价于 api_keys = ["xxx"]
```

## 向后兼容

| 场景 | 行为 |
|------|------|
| 单 key (`api_key = "xxx"`) | `effective_api_keys()` 返回 `["xxx"]`，不创建 pool，行为不变 |
| 多 key (`api_keys = [...]`) | 创建 pool，按 strategy 轮转 |
| 两者都没配 | `effective_api_keys()` 返回 `[]`，报错（同现有行为） |
| 同时配了 `api_key` 和 `api_keys` | `api_keys` 优先（已有逻辑） |

## 不改动的文件

- `openai.rs`, `anthropic.rs`, `kimi.rs`, `minimax.rs` — 这些 wrapper 仅用于 embedding/image/tts/search，
  chat 走的是 protocol client 直接构造。后续可按同一模式扩展。
- `provider_factory.rs` 中 embedding/image/tts/search 的 Request 类型 — 暂不需要。
- `credential_pool.rs` 的 `CredentialPool` / `SharedCredentialPool` 核心逻辑 — 已经完整，无需改动。

## 涉及文件统计

| 文件 | 改动类型 | 工作量 |
|------|---------|--------|
| `credential_pool.rs` | 新增 SharedApiKey | 小 |
| `providers/mod.rs` | 导出 | 1 行 |
| `protocols/openai/chat_completions.rs` | String→SharedApiKey | 3 处机械改 |
| `protocols/anthropic/messages.rs` | String→SharedApiKey | 3 处机械改 |
| `protocols/google/generate_content.rs` | String→SharedApiKey | 3 处机械改 |
| `glm.rs` | String→SharedApiKey | 3 处机械改 |
| `xiaomi.rs` | String→SharedApiKey | 3 处机械改 |
| `provider_factory.rs` | 字段类型 | 1 处 |
| `daemon.rs` | 创建 pool + 接线 | 中 |
| `fallback.rs` | 内层重试循环 | 大（核心） |
| `registry/mod.rs` | 存储 + 传递 pool | 中 |
| `config/mod.rs` | 环境变量展开 | 3 行 |
| **合计 12 个文件** | | |
