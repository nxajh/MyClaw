//! Typing keep-alive 任务生命周期（QQ Bot / Telegram / WeChat 共享）。
//!
//! 三个渠道的 typing 指示都会在几秒到几十秒后过期，因此各自 spawn 一个
//! 后台任务，按固定间隔重发，直到回复发出（或超时/熔断）后 abort。
//! 这套「按 recipient 管理任务 + 发送/睡眠循环」的骨架三家同构：
//!
//! - start：abort 同 recipient 旧任务 → spawn 循环 → 注册 handle
//! - 循环体：TTL 检查 → 发一次 typing → 睡眠 → （可选熔断计数）
//! - 自然退出：若 map 中该 recipient 的 handle 已结束则自我清理
//! - stop：移除并 abort
//!
//! 差异部分以参数注入：节奏（间隔/TTL/熔断阈值）走 [`TypingParams`]，
//! 「发一次 typing」的具体动作为 `prepare` 返回的闭包（每次调用返回一个
//! future）。渠道特有的日志（TTL 警告、熔断日志、退出回调）也通过
//! hook 注入，保证各渠道原有输出逐字不变。

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

/// 按 recipient 注册的 typing 保活任务表。
///
/// 各渠道结构体中原先的
/// `Arc<Mutex<HashMap<String, JoinHandle<()>>>>` 字段替换为本类型。
#[derive(Clone, Default)]
pub struct TypingKeepAlive {
    tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

/// TTL 超限 hook：参数为 (上限秒数, recipient)。
pub type TypingExpiredHook = Box<dyn Fn(u64, &str) + Send + Sync>;
/// 熔断 hook：参数为 (连续失败次数, recipient, 错误信息)。
pub type TypingBreakerHook = Box<dyn Fn(u32, &str, &str) + Send + Sync>;
/// 任务自我清理后的退出 hook（recipient）。
pub type TypingExitHook = Box<dyn Fn(&str) + Send + Sync>;

/// 一次保活循环的节奏与终止策略 + 日志 hook。
pub struct TypingParams {
    /// 两次 typing 发送之间的睡眠时长。
    pub interval: Duration,
    /// 任务总寿命上限；`None` = 无 TTL，循环直到被 stop/替换。
    pub max_duration: Option<Duration>,
    /// 连续发送失败 N 次后熔断退出（0 = 永不熔断）。
    pub max_consecutive_failures: u32,
    /// 超过 TTL 时触发。`None` = 静默退出。
    pub on_expired: Option<TypingExpiredHook>,
    /// 熔断时触发。
    pub on_breaker: Option<TypingBreakerHook>,
    /// 任务自我清理成功后触发（用于清理渠道侧的伴生状态，如
    /// Telegram 的 stall-watchdog 计时表）。
    pub on_exit: Option<TypingExitHook>,
}

impl TypingParams {
    /// 仅指定间隔、不带任何终止策略与 hook 的最简配置。
    pub fn interval_only(interval: Duration) -> Self {
        Self {
            interval,
            max_duration: None,
            max_consecutive_failures: 0,
            on_expired: None,
            on_breaker: None,
            on_exit: None,
        }
    }
}

impl TypingKeepAlive {
    pub fn new() -> Self {
        Self::default()
    }

    /// 为 `recipient` 启动（替换已有任务）保活循环。
    ///
    /// `prepare` 在任务内部被调用一次，返回"发一次 typing"的发送闭包；
    /// 发送闭包每次被调用应发起一次 typing 请求，返回 `Ok(())` 表示
    /// 成功（计入成功计数清零），`Err` 计入连续失败。
    pub fn start<P, F, Fut>(&self, recipient: &str, params: TypingParams, prepare: P)
    where
        P: FnOnce() -> F + Send + 'static,
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send,
    {
        // Abort existing task for this recipient.
        let mut tasks = self.tasks.lock();
        if let Some(handle) = tasks.remove(recipient) {
            handle.abort();
        }

        let tasks_shared = self.tasks.clone();
        let recipient_key = recipient.to_string();

        let handle = tokio::spawn(async move {
            let send = prepare();
            let start = tokio::time::Instant::now();
            let mut consecutive_failures: u32 = 0;

            loop {
                // TTL check
                if let Some(max_duration) = params.max_duration {
                    if start.elapsed() >= max_duration {
                        if let Some(on_expired) = &params.on_expired {
                            on_expired(max_duration.as_secs(), &recipient_key);
                        }
                        break;
                    }
                }

                // Send typing action
                match send().await {
                    Ok(()) => consecutive_failures = 0,
                    Err(e) => {
                        consecutive_failures += 1;
                        if params.max_consecutive_failures != 0
                            && consecutive_failures >= params.max_consecutive_failures
                        {
                            if let Some(on_breaker) = &params.on_breaker {
                                on_breaker(consecutive_failures, &recipient_key, &e);
                            }
                            break;
                        }
                    }
                }

                tokio::time::sleep(params.interval).await;
            }

            // Task exiting: clean up only if no new task has taken over.
            let mut tasks = tasks_shared.lock();
            if let Some(handle) = tasks.get(&recipient_key) {
                if handle.is_finished() {
                    tasks.remove(&recipient_key);
                    if let Some(on_exit) = &params.on_exit {
                        on_exit(&recipient_key);
                    }
                }
            }
        });
        tasks.insert(recipient.to_string(), handle);
    }

    /// 停止（abort 并移除）`recipient` 的保活任务；不存在则 no-op。
    pub fn stop(&self, recipient: &str) {
        let mut tasks = self.tasks.lock();
        if let Some(handle) = tasks.remove(recipient) {
            handle.abort();
        }
    }
}
