//! Session management slash commands: new, compact, history, sessions, switch, rename, delete.

use super::CommandContext;
use super::get_history;

pub async fn cmd_new(args: &str, ctx: CommandContext<'_>) -> String {
    let name = if args.trim().is_empty() { None } else { Some(args.trim()) };
    // Evict cached SessionContext so the next message creates fresh
    // state for the new session.
    ctx.session_contexts.remove(ctx.user_id);
    match ctx.session_manager.new_session(ctx.user_id, name) {
        Ok(info) => {
            let display = info.display_name.as_deref().unwrap_or("(未命名)");
            format!("🆕 新会话已创建：**{}** (`{}`)", display, info.id)
        }
        Err(e) => format!("❌ 创建会话失败: {}", e),
    }
}

pub async fn cmd_compact(ctx: CommandContext<'_>) -> String {
    let session_ctx = match ctx.session_ctx {
        Some(c) => c,
        None => return "ℹ️ 当前没有活跃会话，无需压缩。".to_string(),
    };
    let model_id = match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
        Ok((_, id)) => id,
        Err(e) => return format!("❌ 无法获取当前模型: {}", e),
    };
    // /compact runs an unconditional compaction. We mirror Agent.run's
    // compaction trigger but skip the should_compact gate. The
    // ContextEngine is constructed per-call here (matches Agent.run's
    // per-turn pattern). /compact mutates history so we need exclusive
    // access — if a turn is in flight, surface a busy message instead
    // of waiting through the LLM call.
    let mut session = match session_ctx.session.try_lock() {
        Ok(s) => s,
        Err(_) => return "⏳ 会话正在响应中，请等待响应完成后再执行 /compact。".to_string(),
    };
    let runtime_resources = crate::agents::resource_provider::ResourceProvider::new(
        std::sync::Arc::clone(ctx.agent.skills()),
        ctx.agent.sub_agent_configs().clone(),
        Vec::new(),
        std::path::PathBuf::new(),
        std::path::PathBuf::new(),
        String::new(),
        0,
    );
    let mut engine = crate::agents::context_engine::ContextEngine::new(
        &Default::default(),
        std::sync::Arc::clone(ctx.agent.registry()),
        runtime_resources,
        std::sync::Arc::clone(ctx.agent.tools()),
    );
    engine.init_from_history(ctx.agent.cached_system_prompt(), &session.history);

    let cfg = match ctx.registry.get_chat_model_config(&model_id) {
        Ok(c) => c,
        Err(e) => return format!("❌ 无法获取模型配置: {}", e),
    };
    let window = match cfg.context_window {
        Some(w) => w,
        None => return "❌ 模型未配置 context_window".to_string(),
    };

    let sys_tokens = (ctx.agent.cached_system_prompt().len() as u64).div_ceil(4);
    let tool_tokens: u64 = ctx.agent.tools().all_tools().iter().map(|t| {
        let spec = t.spec();
        let schema = spec.parameters.to_string();
        (spec.name.len() as u64).div_ceil(4)
            + (spec.description.len() as u64).div_ceil(4)
            + (schema.len() as u64).div_ceil(4)
            + 8
    }).sum();

    let boundary = match engine.compaction_boundary(&session.history, window, sys_tokens, tool_tokens) {
        Some(b) => b,
        None => return "ℹ️ 历史不足以压缩。".to_string(),
    };

    let history_snap: Vec<crate::providers::ChatMessage> = session.history.clone();
    match engine.execute_compaction(
        &history_snap,
        ctx.agent.cached_system_prompt(),
        &ctx.agent.tools().all_tools().iter().map(|t| {
            let s = t.spec();
            crate::providers::capability_chat::ToolSpec {
                name: s.name,
                description: Some(s.description),
                input_schema: s.parameters,
            }
        }).collect::<Vec<_>>(),
        boundary,
        &model_id,
        &session,
    ).await {
        Ok(result) => {
            let version = session.compact_version + 1;
            let summary_prefix = "[CONTEXT COMPACTION — REFERENCE ONLY] ";
            let summary_msg = crate::providers::ChatMessage::user_text(
                format!("{}{}", summary_prefix, result.summary)
            );
            let last_compacted_id = session.message_ids
                .get(boundary.saturating_sub(1))
                .copied()
                .unwrap_or(0);
            session.apply_compaction(
                result.compact_start,
                result.compact_end,
                summary_msg,
                version,
                last_compacted_id,
                result.summary_tokens,
            );
            engine.adjust_for_compaction(result.removed_tokens, result.summary_tokens);
            session.token_tracker = Default::default();
            session.token_tracker.update_from_usage(engine.token_total(), 0, 0);
            format!("✅ 上下文压缩完成，当前 token: {}", engine.token_total())
        }
        Err(e) => format!("❌ 压缩失败: {}", e),
    }
}

pub async fn cmd_history(ctx: CommandContext<'_>) -> String {
    let history = match get_history(&ctx).await {
        Some(h) => h,
        None => return "ℹ️ 当前会话为空。".to_string(),
    };

    let truncate = |s: &str, limit: usize| -> String {
        if s.chars().count() > limit {
            format!("{}...", s.chars().take(limit - 3).collect::<String>())
        } else {
            s.to_string()
        }
    };

    let mut lines = vec![format!("📜 **会话历史** ({}条消息)\n", history.len())];
    for (i, msg) in history.iter().enumerate() {
        let tag = match msg.role.as_str() {
            "user" => "👤",
            "assistant" => "🤖",
            "tool" => "🔧",
            "system" => "📋",
            _ => "❓",
        };
        let text = msg.text_content();

        // 跳过 <system-reminder> 前缀，找到用户实际输入
        let display_text = if msg.role == "user" {
            text.strip_prefix("<system-reminder>")
                .and_then(|s| s.find("</system-reminder>"))
                .map(|end| text[end + "</system-reminder>".len()..].trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(&text)
        } else {
            &text
        };
        let first_line = display_text.lines().find(|l| !l.is_empty()).unwrap_or("");

        // Build display: use text if present, otherwise show tool calls.
        let display = if !first_line.is_empty() {
            truncate(first_line, 80)
        } else if let Some(ref tool_calls) = msg.tool_calls {
            if tool_calls.is_empty() {
                "(无文本)".to_string()
            } else {
                tool_calls.iter()
                    .map(|tc| {
                        let args = truncate(&tc.arguments, 50);
                        format!("🔧{}({})", tc.name, args)
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            }
        } else {
            "(无文本)".to_string()
        };
        lines.push(format!("{} `[{}]` {}", tag, i, display));
    }
    lines.join("\n")
}

pub fn cmd_sessions(ctx: CommandContext<'_>) -> String {
    let sessions = ctx.session_manager.list_sessions(ctx.user_id);
    if sessions.is_empty() {
        return "ℹ️ 没有会话记录。".to_string();
    }

    let active_id = ctx.session_manager.active_session_id(ctx.user_id);

    let mut lines = vec!["📂 **会话列表**  \n\n".to_string()];
    for (i, s) in sessions.iter().enumerate() {
        let marker = if active_id.as_deref() == Some(&s.id) { " ← 当前" } else { "" };
        let name = s.display_name.as_deref().unwrap_or("(未命名)");
        let msg_count = s.message_count;
        lines.push(format!("{}. **{}**{} — {}条消息 — `{}`",
            i + 1, name, marker, msg_count, s.id));
    }
    lines.push("\n---\n_/new [名称] — 新建 | /switch <N> — 切换 | /rename <N> <名称> — 重命名 | /delete <N> — 删除_".to_string());
    lines.join("  \n")
}

pub async fn cmd_switch(args: &str, ctx: CommandContext<'_>) -> String {
    let n = match args.trim().parse::<usize>() {
        Ok(n) if n > 0 => n - 1,
        _ => return "⚠️ 用法: /switch <序号>\n用 /sessions 查看会话列表。".to_string(),
    };

    let sessions = ctx.session_manager.list_sessions(ctx.user_id);
    let target = match sessions.get(n) {
        Some(s) => s.clone(),
        None => return format!("⚠️ 序号 {} 无效，当前共 {} 个会话。", n + 1, sessions.len()),
    };

    // Evict cached SessionContext.
    ctx.session_contexts.remove(ctx.user_id);

    match ctx.session_manager.switch_session(ctx.user_id, &target.id) {
        Ok(info) => {
            let name = info.display_name.as_deref().unwrap_or("(未命名)");
            format!("✅ 已切换到：**{}** (`{}`)", name, info.id)
        }
        Err(e) => format!("❌ 切换失败: {}", e),
    }
}

pub fn cmd_rename(args: &str, ctx: CommandContext<'_>) -> String {
    let parts: Vec<&str> = args.trim().splitn(2, char::is_whitespace).collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "⚠️ 用法: /rename <序号> <名称>".to_string();
    }

    let n = match parts[0].parse::<usize>() {
        Ok(n) if n > 0 => n - 1,
        _ => return "⚠️ 序号必须是正整数。".to_string(),
    };

    let sessions = ctx.session_manager.list_sessions(ctx.user_id);
    let target = match sessions.get(n) {
        Some(s) => s,
        None => return format!("⚠️ 序号 {} 无效，当前共 {} 个会话。", n + 1, sessions.len()),
    };

    let new_name = parts[1].trim();
    match ctx.session_manager.rename_session(&target.id, new_name) {
        Ok(()) => format!("✅ 已重命名为：**{}**", new_name),
        Err(e) => format!("❌ 重命名失败: {}", e),
    }
}

pub async fn cmd_delete(args: &str, ctx: CommandContext<'_>) -> String {
    let n = match args.trim().parse::<usize>() {
        Ok(n) if n > 0 => n - 1,
        _ => return "⚠️ 用法: /delete <序号>\n用 /sessions 查看会话列表。".to_string(),
    };

    let sessions = ctx.session_manager.list_sessions(ctx.user_id);
    let target = match sessions.get(n) {
        Some(s) => s,
        None => return format!("⚠️ 序号 {} 无效，当前共 {} 个会话。", n + 1, sessions.len()),
    };

    match ctx.session_manager.delete_session(ctx.user_id, &target.id) {
        Ok(()) => {
            let name = target.display_name.as_deref().unwrap_or("(未命名)");
            format!("🗑️ 已删除会话：**{}**", name)
        }
        Err(e) => format!("❌ 删除失败: {}", e),
    }
}
