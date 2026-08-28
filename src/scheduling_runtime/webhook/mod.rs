//! Webhook server & dispatch — split from `scheduler.rs` (#151 Phase 8a).
//!
//! Owns the hyper-based webhook HTTP surface and per-request turn dispatch:
//!   - `WebhookContext` app state (holds the dependency-inverted
//!     [`OrchestratorHook`] wired by the daemon)
//!   - webhook job projection (`WebhookJobDef`/`WebhookFilter`) from the
//!     unified jobs store
//!   - `run_webhook_server` + request handling, HMAC auth, rate-limit /
//!     inflight guards
//!   - template rendering and delivery dispatch (`send_to_target`)
//!
//! Shares the unified jobs store and schedule helpers with `scheduler.rs`
//! (same module tree, `scheduling_runtime`).
//!
//! Directory layout (P2 split, pure move from the former 1533-line
//! webhook.rs; see `refactor/split-p2-engines`):
//!   - `types` — 类型与数据形态：`WebhookContext` / `WebhookJobDef` /
//!     `WebhookAuth` / `WebhookGuard` / `InflightGuard`、限流/体积常量、
//!     模板渲染（`render_template` / `navigate_json_value`）、
//!     `extract_event_type` / `filter_matches`、`is_route_slug` re-export
//!   - `server` — HTTP 服务器侧：`run_webhook_server` / `handle_request` /
//!     `pretty_payload` / `acceptable_content_type` /
//!     `verify_hmac_signature` / `collect_body(_capped)` / `ok_response`
//!   - `dispatch` — 任务与分发侧：`run_scheduled_task` / `send_to_target` /
//!     `dispatch_webhook_turn` / `handle_hooks_agent` / `handle_hooks_wake`
//!   - `tests` — 原测试模块（保持 `webhook::tests`）

use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use crate::scheduling_types::cron_types::{DeliveryConfig, RunRecord, RunStatus};
use crate::api::message::{
    ChannelMessageContent, ChannelOutboundMessage, MessageReceiver,
};
use crate::config::scheduler::WebhookConfig;

use super::scheduler::{
    OrchestratorHook, SharedScheduler, WebhookDef, WebhookFilter, parse_target_string,
};

mod dispatch;
mod server;
mod types;

#[cfg(test)]
mod tests;

// 子模块经 `use super::*` 复用上方共享导入（原单文件头部，纯移动）。
// 对外转发保持 `crate::scheduling_runtime::webhook::*` 既有路径零改动：
//   - agents/mod.rs：WebhookContext / run_webhook_server / send_to_target
//   - scheduling_runtime/scheduler/mod.rs：WebhookJobDef / is_route_slug
pub use self::dispatch::send_to_target;
pub use self::server::run_webhook_server;
pub use self::types::{WebhookContext, WebhookJobDef, is_route_slug};

// 仅测试消费的符号（tests 经 `use super::*` 取用；正常构建不编译）：
#[cfg(test)]
use self::dispatch::dispatch_webhook_turn;
#[cfg(test)]
use self::server::{pretty_payload, verify_hmac_signature};
#[cfg(test)]
use self::types::{
    WebhookAuth, WebhookGuard, extract_event_type, filter_matches, render_template,
};
