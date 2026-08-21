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

    // 1. Re-scan skills — layered with the shared `~/.agents/skills`
    // library when enabled (issue #83), so a manual `/reload` doesn't drop
    // it from the live SkillManager.
    let skills_dir = base_dir.join("skills");
    let new_defs = crate::agents::skill_loader::load_skills_layered(
        &skills_dir,
        ctx.runtime.agents_skills_dir.as_deref(),
    );
    let new_skills: Vec<crate::agents::Skill> = new_defs
        .iter()
        .map(crate::agents::Skill::from_definition)
        .collect();
    {
        let mut skills = ctx.runtime.skills.write();
        skills.reload(new_skills);
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
