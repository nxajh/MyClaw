//! Model-related slash commands: model, models, think.

use super::CommandContext;
use super::apply_and_persist_override;

pub async fn cmd_model(args: &str, ctx: CommandContext<'_>) -> String {
    let current_override = ctx.session_manager.get_session_override(ctx.user_id);

    if args.is_empty() {
        // Show current model (override or routing default).
        let active_model = if let Some(ref m) = current_override.model {
            format!("会话覆盖: `{}`", m)
        } else {
            match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
                Ok((_, id)) => format!("路由默认: `{}`", id),
                Err(_) => "未配置".to_string(),
            }
        };
        let hint = if current_override.model.is_some() {
            "\n_使用 `/model off` 恢复路由默认。_"
        } else {
            "\n_使用 `/model <名称>` 覆盖会话模型。_"
        };
        return format!("🤖 **当前模型**\n\n{}{}", active_model, hint);
    }

    if args.trim().to_lowercase() == "off" {
        // Clear model override.
        let mut ov = current_override;
        ov.model = None;
        apply_and_persist_override(ov, &ctx).await;
        return "🔄 会话模型覆盖已清除，恢复路由默认。".to_string();
    }

    // Set model override.
    match ctx.registry.get_chat_provider_by_model(args) {
        Some((_, model_id)) => {
            let mut ov = current_override;
            ov.model = Some(model_id.clone());
            apply_and_persist_override(ov, &ctx).await;
            match ctx.registry.get_chat_model_config(&model_id) {
                Ok(cfg) => {
                    let cw = cfg.context_window
                        .map(|v| format!(", 上下文: {}K", v / 1024))
                        .unwrap_or_default();
                    format!("✅ 会话模型已覆盖为: `{}`{}\n_本会话后续所有请求均使用此模型。_", model_id, cw)
                }
                Err(_) => format!("✅ 会话模型已覆盖为: `{}`", model_id),
            }
        }
        None => format!("❌ 未找到模型 `{}`。使用 /models 查看可用模型。", args),
    }
}

pub fn cmd_models(ctx: CommandContext<'_>) -> String {
    match ctx.registry.get_chat_fallback_chain(crate::providers::Capability::Chat) {
        Ok(chain) => {
            if chain.is_empty() {
                return "⚠️ 没有可用的 chat 模型。".to_string();
            }
            let mut lines = vec!["📋 **可用模型**\n".to_string()];
            for (i, (_, model_id)) in chain.iter().enumerate() {
                let marker = if i == 0 { " ← 当前" } else { "" };
                lines.push(format!("{}. `{}`{}", i + 1, model_id, marker));
            }
            lines.join("\n")
        }
        Err(e) => format!("❌ 无法获取模型列表: {}", e),
    }
}

pub async fn cmd_think(args: &str, ctx: CommandContext<'_>) -> String {
    let level = args.trim().to_lowercase();
    if level.is_empty() {
        let current = ctx.session_manager.get_session_override(ctx.user_id);
        let state = match current.thinking {
            Some(true) => format!("开启 (effort: {})", current.effort.as_deref().unwrap_or("默认")),
            Some(false) => "强制关闭".to_string(),
            None => "跟随模型配置".to_string(),
        };
        return format!(
            "🧠 **推理模式**\n\n\
             当前状态: {}\n\n\
             用法: `/think <level>`\n\n\
             可选值:\n\
             • `on` / `high` — 开启深度推理\n\
             • `medium` — 开启中等推理\n\
             • `low` — 开启轻度推理\n\
             • `off` — 强制关闭推理\n\
             • `auto` — 恢复跟随模型配置\n\n\
             _设置后持久生效，直到 `/think auto` 或新建会话。_",
            state
        );
    }

    let mut ov = ctx.session_manager.get_session_override(ctx.user_id);
    let msg = match level.as_str() {
        "on" | "high" => {
            ov.thinking = Some(true);
            ov.effort = Some("high".to_string());
            "🧠 推理模式已设为 **高** (deep thinking)".to_string()
        }
        "medium" => {
            ov.thinking = Some(true);
            ov.effort = Some("medium".to_string());
            "🧠 推理模式已设为 **中等**".to_string()
        }
        "low" => {
            ov.thinking = Some(true);
            ov.effort = Some("low".to_string());
            "🧠 推理模式已设为 **低**".to_string()
        }
        "off" => {
            ov.thinking = Some(false);
            ov.effort = None;
            "🧠 推理模式已**关闭**（强制）".to_string()
        }
        "auto" => {
            ov.thinking = None;
            ov.effort = None;
            "🧠 推理模式已恢复为**跟随模型配置**".to_string()
        }
        _ => return format!("⚠️ 未知推理级别: `{}`\n可用: on, high, medium, low, off, auto", level),
    };

    apply_and_persist_override(ov, &ctx).await;
    format!("{}\n_本会话后续所有请求生效。_", msg)
}
