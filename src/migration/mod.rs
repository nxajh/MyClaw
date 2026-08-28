//! RFC §6 数据迁移引擎（identity-id-rfc §6）。
//!
//! 启动自动迁移（[`run_auto`]）收敛 5 项格式 + 2 项清理：
//! - 6.1 `users.json`：v1 孤儿 `root` 条目 → uuidv7 FQID（Case A 原位升级 /
//!   Case B 丢弃孤儿、保留 `username=root` 的 FQID 条目），key/uid 归一化
//!   （含双重前缀污染值）；
//! - 6.2 `user_resolver.json`：覆盖值双重前缀 / 旧 `root` 段 → 规范 user.id
//!   （与 6.1 同事务，root FQID 用 6.1 结果）；
//! - 6.3 `sessions/`：8-hex 目录 → `<ns>/s/<uuidv7>`（目录重命名 + `meta.json`
//!   `id` 重写 + `active.json` session_id 值重写 + `_cron_<jobid>` 键改名）；
//!   备份策略：逐目录 `meta.json.bak` + `active.json.bak` + old→new manifest
//!   （不做全量目录拷贝——重命名本身原子，meta 备份覆盖全部变更面）；
//! - 6.4 `.state/tasks.json`：`task_{n}` → `<ns>/t/<uuidv7>`，`parent_id` 映射
//!   表重写；
//! - 6.5 `cron/jobs.json`：遗留 job id → `<ns>/job/<uuidv7>`，`run_logs` 文件
//!   同步改名，`active.json` 的 `_cron_` 键连带（键内含 job id，不 rename 则
//!   cron 会话历史断链）；
//! - 6.6 `users/` 遗留 rk 目录 → `users/.legacy-rk-archive/`（仅自动模式；
//!   不动内容，不归档新布局根目录）；
//! - B 清理：`sessions/*/meta.json` 遗留顶层 `task_id` 字段（寻址面迁移 B：
//!   agent 寻址从 task_id 迁到 session id 后的数据卫生；仅自动模式，备份
//!   策略同 6.3：`.migration-backups/<dir>/meta.json.bak`）。
//!
//! 手动命令 `myclaw migrate-namespace <new>`（RFC §6.7）复用同一 builder：
//! 目标 namespace 参数化，流程 备份 → 干跑 → 确认 → 执行。
//!
//! 设计：plan-based。[`build_plan`] 只读数据、产出备份 + 步骤清单（幂等——
//! 数据已符合目标形态则 plan 为空）；[`MigrationPlan::apply`] 先备份后执行。
//! JSON 一律经 `serde_json::Value` 改写（未知字段原样保留，避免结构漂移丢字段）。

mod jobs;
mod plan;
mod sessions;
mod types;
mod users;

pub use plan::{build_plan, default_base_dir, run_auto};
pub use types::{Backup, MigrationReport, MigrationPlan, Step};

// Test-import forwarding（P0 websocket/client 批 3 模式）：tests.rs 的
// `use super::*` 从本模块命名空间取绑定；cfg(test) 门控避免非测试构建
// 下的 unused import（clippy -D warnings）。
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
use crate::ids::dir_name;
#[cfg(test)]
use jobs::migrate_jobs;
#[cfg(test)]
use sessions::{migrate_sessions, migrate_tasks};
#[cfg(test)]
use users::{archive_legacy_user_dirs, migrate_resolver, migrate_users, normalize_user_id};

#[cfg(test)]
mod tests;
