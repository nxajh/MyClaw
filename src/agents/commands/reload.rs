//! Reload/stop slash commands: stop, reload.

use super::CommandContext;

pub fn cmd_stop() -> String {
    "⏹️ 停止信号已发送。\n_注意：当前请求完成后才会生效。_".to_string()
}

pub async fn cmd_reload(ctx: CommandContext<'_>) -> String {
    let workspace_dir = ctx.runtime.workspace_dir();

    // 1. Re-scan skills
    let skills_dir = workspace_dir.join("skills");
    let new_defs = crate::agents::skill_loader::load_skills_from_dir(&skills_dir);
    let new_skills: Vec<crate::agents::Skill> =
        new_defs.iter().map(crate::agents::Skill::from_definition).collect();
    {
        let mut skills = ctx.runtime.skills().write();
        skills.reload(new_skills);
    }

    // 2. Re-scan agents
    let agents_dir = workspace_dir.join("agents");
    let agent_count = ctx.runtime.sub_agent_configs().reload_from_dir(&agents_dir);

    // 3. No need to reset attachment manager — next diff rebuilds from history.

    let skill_count = ctx.runtime.skills().read().skill_count();

    format!("🔄 已重新加载：{} 个 skills，{} 个 agents", skill_count, agent_count)
}
