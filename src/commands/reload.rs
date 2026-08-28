//! Reload/stop slash commands: stop, reload.

use super::CommandContext;

pub async fn cmd_stop(ctx: CommandContext<'_>) -> String {
    if let Some(session_ctx) = ctx.session_ctx {
        let token = session_ctx.turn_cancel.lock().unwrap();
        if !token.is_cancelled() {
            token.cancel();
            "⏹️ 已停止当前任务。".to_string()
        } else {
            "⏹️ 当前没有正在执行的任务。".to_string()
        }
    } else {
        "⏹️ 无法停止：未找到活动会话。".to_string()
    }
}

pub async fn cmd_reload(ctx: CommandContext<'_>) -> String {
    // Reloading requires base_dir for skills and agents
    let base_dir = std::path::PathBuf::from(&ctx.runtime.defaults.prompt.base_dir);

    // 1. Re-scan skills — all three layers (user > agent > shared), so a
    // manual `/reload` picks up user-layer edits too and never drops the
    // shared `~/.agents/skills` library (issue #83) from the live manager.
    let skills_dir = base_dir.join("skills");
    let users_dir = base_dir.join("users");
    let user_map = crate::agents::skill_loader::load_all_users_skills(&users_dir);
    let agent_defs = crate::agents::skill_loader::load_skills_from_dir(&skills_dir);
    let shared_defs = match ctx.runtime.agents_skills_dir.as_deref() {
        Some(d) => crate::agents::skill_loader::load_skills_from_dir(d),
        None => Vec::new(),
    };
    {
        let mut skills = ctx.runtime.skills.write();
        skills.reload_from_definitions(user_map, agent_defs, shared_defs);
    }

    // 2. Re-scan agents
    let agents_dir = base_dir.join("agents");
    let agent_count = ctx.runtime.agents.reload_from_dir(&agents_dir);

    // 3. No need to reset attachment manager — next diff rebuilds from history.

    let skill_count = ctx.runtime.skills.read().skill_count();

    format!(
        "🔄 已重新加载：{} 个 skills，{} 个 agents",
        skill_count, agent_count
    )
}
