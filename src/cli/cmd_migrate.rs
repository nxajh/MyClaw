//! `myclaw migrate-namespace <new>` — RFC §6.7 手动迁移命令。
//!
//! 场景：管理员把 `[system] namespace` 改为品牌名后，把存量持久化数据的
//! namespace 段一并改写。流程：daemon 运行检查 → 构建计划（干跑）→ 展示 →
//! 确认（或 `--yes`）→ 执行。复用 [`myclaw::migration`] 引擎（plan-based：
//! 备份 → 步骤，幂等——数据已符合目标 namespace 则 plan 为空）。

use anyhow::{Context, Result};

use crate::cli::{load_config, Cli};

/// 执行 `myclaw migrate-namespace <new>`。
pub fn run(cli: &Cli, to_namespace: &str, yes: bool) -> Result<()> {
    // 1. daemon 运行中拒绝——迁移需要独占数据文件（sessions 目录重命名等）。
    if let Ok(pid) = myclaw::signal::find_daemon_pid() {
        anyhow::bail!(
            "daemon 正在运行（PID {pid}）。请先 `myclaw stop` 再执行迁移——\
             迁移需要独占数据文件（sessions/ 目录重命名、users.json 重写等）"
        );
    }

    // 2. 校验目标 namespace 形态（与 id 层一致：非空、无 `/`）。
    let to_ns = to_namespace.trim();
    if to_ns.is_empty() || to_ns.contains('/') || to_ns.contains(char::is_whitespace) {
        anyhow::bail!(
            "目标 namespace 非法：{to_namespace:?}（须为非空、不含 `/` 或空白的单段名称，如 brand）"
        );
    }

    // 3. 配置 → workspace_dir / base_dir（迁移入口参数）。
    let cfg = load_config(cli)?;
    let workspace_dir = cfg.workspace_dir.clone();
    let base_dir = myclaw::migration::default_base_dir();
    let from_ns = cfg.system.namespace.clone();

    // 4. 干跑：构建计划并展示。
    let plan = myclaw::migration::build_plan(&workspace_dir, &base_dir, to_ns, false)
        .context("构建迁移计划失败")?;
    if plan.is_empty() {
        println!("✅ 无需迁移：数据已符合目标命名空间 {to_ns}");
        return Ok(());
    }
    println!(
        "迁移计划：{from_ns} → {to_ns}（备份 {} 项、步骤 {} 项）",
        plan.backups.len(),
        plan.steps.len()
    );
    for b in &plan.backups {
        println!("  备份: {}", b.label);
    }
    for s in &plan.steps {
        println!("  {}", s.label());
    }

    // 5. 确认。
    if !yes {
        print!("确认执行？[y/N] ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") && !line.trim().eq_ignore_ascii_case("yes") {
            println!("已取消（未做任何修改）");
            return Ok(());
        }
    }

    // 6. 执行。
    plan.apply().context("迁移执行失败（.bak 备份已保留，可手动恢复）")?;
    println!(
        "✅ 迁移完成：{} 个备份、{} 个步骤已应用。建议重启 daemon 验证。",
        plan.backups.len(),
        plan.steps.len()
    );
    Ok(())
}
