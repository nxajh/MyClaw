# MyClaw 三轨调度设计：Heartbeat + Cron + Webhook

## 1. 设计目标

让 MyClaw Daemon 成为常驻自主 Agent。三种触发源共享同一套执行基础设施：

| 触发源 | 触发方式 | 典型场景 |
|--------|---------|---------|
| **Heartbeat** | 固定间隔（默认 30 分钟） | 定期巡检、状态监控、待办检查 |
| **Cron** | cron 表达式精确定时 | "每天 9:00 发日报"、"每周一汇总" |
| **Webhook** | HTTP POST 外部事件 | GitHub push、邮件到达、CI 完成 |

三者最终都注入同一条路径：`构造 prompt → AgentLoop::run() → 处理响应`。

## 2. 统一抽象：ScheduledEvent

```
                    ┌──────────────────┐
                    │   Orchestrator   │
                    │                  │
  Channel Listener ─┤                  │
                    │   tokio::select! │──→ AgentLoop::run()
  Heartbeat Timer ──┤                  │
                    │                  │
  Cron Scheduler ───┤   + last_channel │──→ channel.send()
                    │     tracking     │
  Webhook Server ───┤                  │
                    │                  │
                    └──────────────────┘
```

### 为什么不复用 Orchestrator 事件循环？

Orchestrator 的 `run()` 用 `tokio::select!` 做 `msg_rx.recv()` + `delegation_rx.recv()` + `shutdown`。Heartbeat/Cron 是定时器，Webhook 是 HTTP server。如果把它们全塞进 select，Orchestrator 会变得很复杂，每个分支的处理逻辑也不同（heartbeat 需要静默判断、webhook 需要返回 HTTP response）。

**方案：独立 task，共享 Agent/Session/Channel 资源。**

```
daemon::run()
    ├── Orchestrator::run()          // 处理用户消息（现有逻辑）
    ├── tokio::spawn(heartbeat)      // 独立 task
    ├── tokio::spawn(cron_scheduler) // 独立 task
    └── tokio::spawn(webhook_server) // 独立 task
```

三个 task 通过 `Arc` 共享 `Agent`、`SessionManager`、`channels`、`sessions`。不通过消息传递，直接调用 `AgentLoop::run()`。

## 3. 配置

### 3.1 TOML 结构

```toml
[agent.heartbeat]
enabled = true
every = "30m"                    # interval，"0" 禁用
target = "last"                  # "last" | "none" | channel name
active_hours = "08:00-24:00"    # 活动时段（空或省略 = 全天）
prompt = ""                      # 自定义 prompt（空 = 默认 heartbeat prompt）

[agent.cron]
enabled = false

[[agent.cron.jobs]]
schedule = "0 9 * * *"           # cron 表达式（分 时 日 月 周）
prompt = "生成昨天的日报"
target = "telegram"

[[agent.cron.jobs]]
schedule = "0 */2 * * *"         # 每 2 小时
prompt = "检查 CI 状态"
target = "last"

[agent.webhook]
enabled = false
port = 18789
secret = "${MYCLAW_WEBHOOK_SECRET}"  # HMAC 验证
```

### 3.2 Rust 配置结构

```rust
// src/config/scheduler.rs

/// Heartbeat 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_every")]
    pub every: String,                   // "30m" | "1h" | "5m"
    #[serde(default = "default_target")]
    pub target: String,                  // "last" | "none" | channel name
    #[serde(default)]
    pub active_hours: Option<String>,    // "08:00-24:00"
    #[serde(default)]
    pub prompt: Option<String>,          // 自定义 prompt
}

/// Cron 任务
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub schedule: String,                // "0 9 * * *"
    pub prompt: String,                  // 触发时发送给 agent 的内容
    #[serde(default = "default_target")]
    pub target: String,
}

/// Cron 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

/// Webhook 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_webhook_port")]
    pub port: u16,
    #[serde(default)]
    pub secret: Option<String>,          // HMAC-SHA256 验证密钥
}

/// 统一调度配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
}
```

### 3.3 AppConfig 集成

在 `AgentConfig` 中新增 `scheduler` 字段（放在 agent section 下，因为调度是 agent 行为）：

```rust
pub struct AgentConfig {
    // ... 现有字段 ...
    pub scheduler: SchedulerConfig,
}
```

## 4. 共享资源：SchedulerContext

三个调度 task 需要共享的资源，打包为一个 struct：

```rust
// src/agents/scheduler.rs

/// 调度器共享上下文 — 由 daemon 构造，Arc 共享给三个 task
pub struct SchedulerContext {
    pub agent: Agent,
    pub channels: Arc<DashMap<String, Arc<dyn Channel>>>,
    pub sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>>,
    pub session_manager: SessionManager,
    pub persist_backend: Arc<dyn SessionBackend>,
    pub timezone_offset: i32,
    /// 最后收到用户消息的 channel name
    pub last_channel: Arc<Mutex<Option<String>>>,
    pub sub_delegator: Option<Arc<SubAgentDelegator>>,
    pub delegation_manager: Option<Arc<DelegationManager>>,
    pub change_rx: Option<tokio::sync::watch::Receiver<ChangeSet>>,
}
```

### last_channel 追踪

Orchestrator 处理用户消息时更新 `last_channel`。在 `Orchestrator` 结构体新增：

```rust
pub last_channel: Arc<Mutex<Option<String>>>,
```

在 `ChannelEvent::UserMessage` 处理分支中更新：

```rust
*last_channel.lock().await = Some(channel_name.clone());
```

## 5. Heartbeat

### 5.1 核心逻辑

```rust
pub async fn run_heartbeat(ctx: Arc<SchedulerContext>, config: HeartbeatConfig) {
    let interval = match parse_interval(&config.every) {
        Some(d) => d,
        None => return,
    };

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;

        // 检查活动时段
        if !is_active_hours(&config.active_hours, ctx.timezone_offset) {
            continue;
        }

        // 检查 HEARTBEAT.md
        if !std::path::Path::new("HEARTBEAT.md").exists() {
            continue;
        }

        let prompt = config.prompt.as_deref().unwrap_or(HEARTBEAT_PROMPT);
        let session_key = "_heartbeat";

        let result = run_scheduled_task(&ctx, session_key, prompt).await;

        match result {
            Ok(response) if is_silent_ok(&response) => {
                tracing::info!("heartbeat: nothing needs attention");
            }
            Ok(response) => {
                send_to_target(&ctx, &config.target, &response).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "heartbeat run failed");
            }
        }
    }
}

const HEARTBEAT_PROMPT: &str =
    "read heartbeat.md if it exists. follow it strictly. \
     if nothing needs attention, reply heartbeat_ok.";
```

### 5.2 静默判断

```rust
fn is_silent_ok(response: &str) -> bool {
    let trimmed = response.trim().to_lowercase();
    trimmed == "heartbeat_ok"
        || trimmed == "heartbeat ok"
        || trimmed.contains("heartbeat_ok")
}
```

## 6. Cron

### 6.1 Cron 表达式解析

需要 `cron` crate 解析标准 5 字段 cron 表达式（分 时 日 月 周）。

```rust
use cron::Schedule;

pub async fn run_cron_scheduler(ctx: Arc<SchedulerContext>, config: CronConfig) {
    let mut jobs: Vec<(Schedule, CronJob)> = Vec::new();
    for job in &config.jobs {
        match job.schedule.parse::<Schedule>() {
            Ok(schedule) => jobs.push((schedule, job.clone())),
            Err(e) => tracing::warn!(schedule = %job.schedule, error = %e, "invalid cron expression, skipping"),
        }
    }

    if jobs.is_empty() {
        return;
    }

    // 最小 tick = 1 分钟（cron 最小粒度也是分钟）
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        let now = chrono::Local::now();

        for (schedule, job) in &jobs {
            // 检查当前分钟是否匹配 cron 表达式
            if schedule.matches(now) {
                let session_key = format!("_cron_{}", sanitize_session_key(&job.schedule));
                let result = run_scheduled_task(&ctx, &session_key, &job.prompt).await;

                match result {
                    Ok(response) => {
                        send_to_target(&ctx, &job.target, &response).await;
                    }
                    Err(e) => {
                        tracing::warn!(cron = %job.schedule, error = %e, "cron job failed");
                    }
                }
            }
        }
    }
}
```

### 6.2 依赖

```toml
# Cargo.toml
cron = "0.15"
```

## 7. Webhook

### 7.1 HTTP Server

用 `hyper`（tokio 生态，不引入新 async runtime）。MyClaw 已经有 `reqwest`（hyper-based），但 hyper server 端需要显式添加。

```toml
# Cargo.toml
hyper = { version = "1", features = ["server", "http1"] }
hyper-util = "0.1"
http-body-util = "0.1"
```

### 7.2 核心逻辑

```rust
pub async fn run_webhook_server(ctx: Arc<SchedulerContext>, config: WebhookConfig) {
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));

    let ctx_clone = ctx.clone();
    let secret = config.secret.clone();

    let service = move |req: Request<Incoming>| {
        let ctx = ctx_clone.clone();
        let secret = secret.clone();
        async move {
            // 1. 验证 HMAC 签名（如果配置了 secret）
            if let Some(ref secret) = secret {
                if !verify_hmac(&req, secret) {
                    return Response::builder()
                        .status(401)
                        .body("unauthorized".into())
                        .unwrap();
                }
            }

            // 2. 解析请求 body
            let body = collect_body(req).await;
            let prompt = format!(
                "A webhook event was received:\n```\n{}\n```\n\n\
                 Analyze this event and take appropriate action if needed. \
                 If nothing needs attention, reply webhook_ok.",
                body
            );

            // 3. 运行 agent
            let session_key = "_webhook";
            let result = run_scheduled_task(&ctx, session_key, &prompt).await;

            match result {
                Ok(response) if is_webhook_ok(&response) => {
                    Response::builder()
                        .status(200)
                        .body("ok".into())
                        .unwrap()
                }
                Ok(response) => {
                    // 有输出 → 发送到 channel，同时返回 HTTP response
                    send_to_target(&ctx, "last", &response).await;
                    Response::builder()
                        .status(200)
                        .body(response.into())
                        .unwrap()
                }
                Err(e) => {
                    Response::builder()
                        .status(500)
                        .body(format!("error: {}", e).into())
                        .unwrap()
                }
            }
        }
    };

    let listener = TcpListener::bind(addr).await.unwrap();
    tracing::info!(port = config.port, "webhook server listening");

    // 逐连接处理（webhook 频率低，不需要并发）
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let service = service.clone();
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });
    }
}
```

### 7.3 Webhook URL 格式

```
POST http://localhost:18789/webhook
Content-Type: application/json
X-MyClaw-Signature: sha256=<hmac_hex>

{
    "event": "push",
    "repository": "myclaw/myclaw",
    "ref": "refs/heads/main",
    ...
}
```

HMAC 签名验证（跟 GitHub webhook 签名机制一致）：
```rust
fn verify_hmac(req: &Request<Incoming>, secret: &str) -> bool {
    let sig = req.headers()
        .get("X-MyClaw-Signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // HMAC-SHA256(secret, body) == sig
}
```

## 8. 共享执行函数

三轨调度共用同一个 `run_scheduled_task`：

```rust
/// 创建/获取调度专用 AgentLoop 并运行一次。
async fn run_scheduled_task(
    ctx: &SchedulerContext,
    session_key: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let loop_ = get_or_create_scheduler_loop(ctx, session_key);

    let mut guard = loop_.lock().await;
    guard.run(prompt, None, None).await
}

/// 获取或创建调度专用的 AgentLoop。
fn get_or_create_scheduler_loop(
    ctx: &SchedulerContext,
    session_key: &str,
) -> Arc<TokioMutex<AgentLoop>> {
    ctx.sessions.entry(session_key.to_string())
        .or_insert_with(|| {
            let session = ctx.session_manager.open(session_key);
            let agent_loop = ctx.agent.loop_for(session);
            Arc::new(TokioMutex::new(agent_loop))
        })
        .clone()
}

/// 发送响应到目标 channel。
async fn send_to_target(
    ctx: &SchedulerContext,
    target: &str,
    content: &str,
) {
    let channel_name = match target {
        "none" => return,
        "last" => ctx.last_channel.lock().await.clone(),
        name => Some(name.to_string()),
    };

    let Some(ch_name) = channel_name else {
        tracing::warn!("no target channel for scheduled response");
        return;
    };

    let channel = match ctx.channels.get(&ch_name) {
        Some(ch) => ch.clone(),
        None => {
            tracing::warn!(channel = %ch_name, "target channel not found");
            return;
        }
    };

    let msg = SendMessage {
        recipient: None,
        content: content.to_string(),
        subject: None,
        thread_ts: None,
        cancellation_token: None,
        attachments: vec![],
        image_urls: None,
    };

    if let Err(e) = channel.send(&msg).await {
        tracing::warn!(channel = %ch_name, error = %e, "failed to send scheduled response");
    }
}
```

## 9. 集成到 daemon

### 9.1 Orchestrator 新增 last_channel

```rust
pub struct Orchestrator {
    // ... 现有字段 ...
    pub last_channel: Arc<Mutex<Option<String>>>,
}
```

在 `ChannelEvent::UserMessage` 处理中：
```rust
// 记录最后收到消息的 channel
if let Some(lc) = self.last_channel.as_ref() {
    *lc.lock().await = Some(channel_name.clone());
}
```

### 9.2 daemon::run() 启动调度 task

```rust
// ── 构造 SchedulerContext ────────────────────────────────────
let scheduler_ctx = Arc::new(SchedulerContext {
    agent: agent.clone(),
    channels: channels_map.clone(),  // 从 Orchestrator 拿
    sessions: sessions.clone(),
    session_manager: session_manager.clone(),
    persist_backend: session_backend.clone(),
    timezone_offset: config.agent.prompt.timezone_offset,
    last_channel: last_channel.clone(),
    sub_delegator: sub_agent_delegator_arc.clone(),
    delegation_manager: delegation_manager.clone(),
    change_rx: Some(change_rx.clone()),
});

// ── 启动调度 task ─────────────────────────────────────────────
let scheduler_config = config.agent.scheduler.clone();

if scheduler_config.heartbeat.enabled {
    let hb_ctx = scheduler_ctx.clone();
    let hb_config = scheduler_config.heartbeat.clone();
    tokio::spawn(async move {
        run_heartbeat(hb_ctx, hb_config).await;
    });
    tracing::info!(every = %scheduler_config.heartbeat.every, "heartbeat scheduler started");
}

if scheduler_config.cron.enabled && !scheduler_config.cron.jobs.is_empty() {
    let cron_ctx = scheduler_ctx.clone();
    let cron_config = scheduler_config.cron.clone();
    tokio::spawn(async move {
        run_cron_scheduler(cron_ctx, cron_config).await;
    });
    tracing::info!(job_count = cron_config.jobs.len(), "cron scheduler started");
}

if scheduler_config.webhook.enabled {
    let wh_ctx = scheduler_ctx.clone();
    let wh_config = scheduler_config.webhook.clone();
    tokio::spawn(async move {
        run_webhook_server(wh_ctx, wh_config).await;
    });
    tracing::info!(port = wh_config.port, "webhook server started");
}
```

### 9.3 问题：sessions 从哪来？

当前 `sessions` 在 `Orchestrator` 内部创建和管理。SchedulerContext 需要访问同一个 `sessions` map。

**方案**：在 daemon 中提前创建 `sessions` Arc，传给 Orchestrator 和 SchedulerContext：

```rust
let sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>> = Arc::new(DashMap::new());

// 传给 Orchestrator
let (mut orchestrator, _msg_tx) = Orchestrator::new_with_sessions(parts, sessions.clone());

// 传给 SchedulerContext
let scheduler_ctx = Arc::new(SchedulerContext {
    sessions,
    ...
});
```

这需要把 `Orchestrator::new()` 改为接受外部 sessions map，或者让 Orchestrator 暴露 `sessions()` getter。

## 10. Session 管理问题

### 10.1 调度 session vs 用户 session

| 维度 | 用户 session | 调度 session |
|------|-------------|-------------|
| Key | `"telegram:12345"` | `"_heartbeat"`, `"_cron:0_9_*_*_*"`, `"_webhook"` |
| History | 用户对话历史 | 调度执行历史 |
| System prompt | 完整 | 完整（相同） |
| Compaction | 正常触发 | 正常触发 |
| 并发 | 用户消息串行 | 与用户 session 独立，不冲突 |

### 10.2 Session 隔离

调度 session 用 `_` 前缀，不会跟用户 session 冲撞。每个调度 session 有独立的 history 和 compaction 状态。

### 10.3 AgentLoop 共享问题

`AgentLoop` 不是 `Clone`，且包含 `&mut self` 方法。每个 session 对应一个 `Arc<Mutex<AgentLoop>>`。调度 task 和 Orchestrator 可能同时访问不同的 AgentLoop，没问题。但同一个 AgentLoop 不能并发访问——`Mutex` 保证。

**潜在问题**：如果 heartbeat 触发时，`_heartbeat` session 的 AgentLoop 正在执行上一次心跳（如上次心跳的 tool call 还没完），`loop_.lock().await` 会阻塞直到上次执行完成。这是正确行为——不会并发执行。

## 11. 改动清单

### 11.1 新增文件

| 文件 | 内容 |
|------|------|
| `src/config/scheduler.rs` | `HeartbeatConfig`, `CronConfig`, `CronJob`, `WebhookConfig`, `SchedulerConfig` |
| `src/agents/scheduler.rs` | `SchedulerContext`, `run_heartbeat()`, `run_cron_scheduler()`, `run_webhook_server()`, `run_scheduled_task()`, `send_to_target()` |

### 11.2 修改文件

| 文件 | 改动 |
|------|------|
| `Cargo.toml` | 新增 `cron = "0.15"`, `hyper = "1"`, `hyper-util = "0.1"`, `http-body-util = "0.1"` |
| `src/config/mod.rs` | `RawConfig` 和 `AppConfig` 新增 `scheduler` 字段 |
| `src/config/agent.rs` | `AgentConfig` 新增 `scheduler: SchedulerConfig` 字段 |
| `src/agents/orchestrator.rs` | 新增 `last_channel` 字段，消息处理时更新；暴露 `sessions()` getter |
| `src/daemon.rs` | 构造 `SchedulerContext`，启动调度 task |

### 11.3 不改动的文件

| 文件 | 原因 |
|------|------|
| `agent_impl.rs` | `AgentLoop::run()` 接口不变，调度只是调用方不同 |
| `attachment.rs` | 调度 session 也走 `build_messages()`，日期注入等自动生效 |
| `session_manager.rs` | session key 只是字符串，调度 key 用 `_` 前缀即可 |

## 12. 实施顺序

```
Phase 1: 基础设施（~150 行）
  ├── SchedulerConfig 配置结构
  ├── SchedulerContext 共享资源
  ├── run_scheduled_task() 共享执行函数
  ├── send_to_target() 发送函数
  └── Orchestrator 暴露 sessions + last_channel

Phase 2: Heartbeat（~80 行）
  ├── parse_interval()
  ├── is_active_hours()
  ├── run_heartbeat()
  └── daemon 启动集成

Phase 3: Cron（~80 行）
  ├── cron crate 集成
  ├── run_cron_scheduler()
  └── daemon 启动集成

Phase 4: Webhook（~120 行）
  ├── hyper server
  ├── HMAC 验证
  ├── run_webhook_server()
  └── daemon 启动集成
```

Phase 1 + 2 可以先做，Heartbeat 立刻可用。Cron 和 Webhook 可以后续独立提交。

## 13. HEARTBEAT.md 示例

```markdown
# Heartbeat Checklist

## Routine Checks
- Quick scan: any urgent matters in recent memory files?
- Check if there are pending tasks from previous conversations
- If it's morning (before 10:00), briefly check if there are tasks planned for today

## If nothing needs attention
- Reply heartbeat_ok

## Notes
- Keep this file concise to minimize token usage
- Do not include sensitive information
```

## 14. 安全考虑

1. **Webhook HMAC 验证**：必须配置 secret，所有请求验证 HMAC-SHA256 签名
2. **Webhook prompt 注入**：外部输入作为 prompt 的一部分，LLM 可能被误导。prompt 中明确标注"webhook event data"，模型应区分指令和数据
3. **调度 session 隔离**：`_heartbeat` 等 session 不能被用户消息访问
4. **资源限制**：调度 task 使用独立的 session，compaction 正常工作，不会撑爆 context
