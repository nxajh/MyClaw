//! Configuration slash commands: config, settings, autonomy.

use crate::config::agent::PermissionMode;
use super::CommandContext;
use super::apply_and_persist_override;
use super::info::{cmd_tools, cmd_skill};

pub fn cmd_config(args: &str, ctx: CommandContext<'_>) -> String {
    let model_info = {
        let ov = ctx.session_manager.get_session_override(ctx.user_id);
        if let Some(ref m) = ov.model {
            format!("{} (会话覆盖)", m)
        } else {
            match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
                Ok((_, model_id)) => model_id,
                Err(_) => "未配置".to_string(),
            }
        }
    };
    if args.is_empty() {
        let tools = ctx.agent.tools();
        let skills = ctx.agent.skills();
        let skills_count = skills.read().skill_count();
        format!(
            "⚙️ **运行时配置**\n\n\
             模型: `{}`\n\
             工具数: {}\n\
             Skill数: {}\n\
             会话: `{}`\n\n\
             _使用 /settings 查看会话级参数覆盖。_",
            model_info,
            tools.tool_count(),
            skills_count,
            ctx.user_id,
        )
    } else {
        let key = args.trim().to_lowercase();
        match key.as_str() {
            "model" | "模型" => format!("🤖 模型: `{}`", model_info),
            "tools" | "工具" => cmd_tools(ctx),
            "skills" => cmd_skill(ctx),
            _ => format!("⚠️ 未知配置项: `{}`\n可查看: model, tools, skill", args),
        }
    }
}

pub async fn cmd_settings(ctx: CommandContext<'_>) -> String {
    let ov = ctx.session_manager.get_session_override(ctx.user_id);

    let model_str = ov.model.as_deref().unwrap_or("跟随路由配置");
    let thinking_str = match ov.thinking {
        Some(true) => format!("开启 (effort: {})", ov.effort.as_deref().unwrap_or("默认")),
        Some(false) => "强制关闭".to_string(),
        None => "跟随模型配置".to_string(),
    };
    let autonomy_str = match ov.permission_mode {
        Some(PermissionMode::Full) => "full",
        Some(PermissionMode::Default) => "default",
        Some(PermissionMode::ReadOnly) => "read_only",
        None => "跟随全局配置",
    };
    let max_tool_calls_str = ov.max_tool_calls
        .map(|v| if v == 0 { "无限制".to_string() } else { v.to_string() })
        .unwrap_or_else(|| "跟随全局配置".to_string());
    let compact_threshold_str = ov.compact_threshold
        .map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_else(|| "跟随全局配置".to_string());
    let retain_work_units_str = ov.retain_work_units
        .map(|v| v.to_string())
        .unwrap_or_else(|| "跟随全局配置".to_string());

    let all_default = ov.is_empty();
    let status_note = if all_default {
        "\n_所有参数均为全局默认值。_"
    } else {
        "\n_带 * 号的参数已被会话级覆盖，重新创建会话后恢复默认。_"
    };

    let m = |set: bool| if set { "* " } else { "  " };

    format!(
        "⚙️ **会话参数**\n\n\
         {}模型: {}\n\
         {}推理模式: {}\n\
         {}自主权: {}\n\
         {}最大工具调用: {}\n\
         {}压缩阈值: {}\n\
         {}保留工作单元: {}{}",
        m(ov.model.is_some()), model_str,
        m(ov.thinking.is_some()), thinking_str,
        m(ov.permission_mode.is_some()), autonomy_str,
        m(ov.max_tool_calls.is_some()), max_tool_calls_str,
        m(ov.compact_threshold.is_some()), compact_threshold_str,
        m(ov.retain_work_units.is_some()), retain_work_units_str,
        status_note
    )
}

pub async fn cmd_autonomy(args: &str, ctx: CommandContext<'_>) -> String {
    let level = args.trim().to_lowercase();
    if level.is_empty() {
        let current = ctx.session_manager.get_session_override(ctx.user_id);
        let state = match current.permission_mode {
            Some(PermissionMode::Full) => "full（全自主）",
            Some(PermissionMode::Default) => "default（默认）",
            Some(PermissionMode::ReadOnly) => "read_only（只读）",
            None => "跟随全局配置",
        };
        return format!(
            "🔐 **自主权级别**\n\n\
             当前状态: {}\n\n\
             用法: `/autonomy <level>`\n\n\
             可选值:\n\
             • `full` — 全自主，所有工具无需审批\n\
             • `default` — 安全工具自动批准，危险工具需审批\n\
             • `read_only` — 仅允许只读工具\n\
             • `auto` — 恢复跟随全局配置\n\n\
             _设置后持久生效，需新建会话才能重建系统提示词。_",
            state
        );
    }

    let mut ov = ctx.session_manager.get_session_override(ctx.user_id);
    let (autonomy, msg) = match level.as_str() {
        "full" => (Some(PermissionMode::Full), "✅ 自主权已设为 **full**（所有工具自动批准）"),
        "default" => (Some(PermissionMode::Default), "✅ 自主权已设为 **default**"),
        "read_only" | "readonly" => (Some(PermissionMode::ReadOnly), "✅ 自主权已设为 **read_only**（仅只读工具）"),
        "auto" => (None, "✅ 自主权已恢复为跟随全局配置"),
        _ => return format!("⚠️ 未知级别: `{}`\n可用: full, default, read_only, auto", level),
    };

    ov.permission_mode = autonomy;
    apply_and_persist_override(ov, &ctx).await;

    // Evict the cached agent loop so the system prompt is rebuilt with the new autonomy.
    ctx.sessions.remove(ctx.user_id);
    format!("{}\n_系统提示词将在下次请求时重建。_", msg)
}
