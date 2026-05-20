//! Informational slash commands: help, status, tools, context, mcp, skill, btw, export.

use super::CommandContext;
use super::get_history;

pub fn cmd_help() -> String {
    "📦 **MyClaw Slash Commands**\n\n\
     **基础**  \n\
     /help — 显示此帮助信息  \n\
     /status — 显示 provider 实时状态  \n\
     /new [名称] — 创建新会话  \n\
     /sessions — 列出所有会话  \n\
     /switch <序号> — 切换到指定会话  \n\
     /rename <序号> <名称> — 重命名会话  \n\
     /delete <序号> — 删除会话  \n\
     /compact — 手动触发上下文压缩  \n\
     /model [名称|off] — 查看或覆盖当前会话模型  \n\
     /models — 列出可用模型  \n\
     /stop — 中断当前运行  \n\n\
     **会话参数**  \n\
     /think [on|off|low|medium|high] — 控制推理模式（会话级持久生效）  \n\
     /autonomy [full|default|read_only] — 控制自主权级别（会话级持久生效）  \n\
     /settings — 查看当前会话所有参数  \n\n\
     **工具与配置**  \n\
     /tools — 列出可用工具及说明  \n\
     /skills — 列出已加载的 skill  \n\
     /config [key] — 查看运行时配置  \n\n\
     **上下文**  \n\
     /context — 上下文窗口使用详情  \n\
     /history — 显示会话历史摘要  \n\
     /export — 导出当前会话  \n\n\
     **其他**  \n\
     /mcp — 查看 MCP 服务器状态  \n\
     /btw <问题> — 旁路提问，不影响上下文  \n\n\
     _别名: /h=/help, /n=/new, /ss=/sessions, /sw=/switch, /rn=/rename, /del=/delete_".to_string()
}

pub async fn cmd_status(ctx: CommandContext<'_>) -> String {
    let summaries = ctx.registry.get_all_provider_summaries();
    if summaries.is_empty() {
        return "⚠️ 没有已注册的 provider。".to_string();
    }

    let mut lines = vec!["📊 **Provider 实时状态**\n".to_string()];

    // Header
    lines.push(format!(
        "{:<16} {:>10} {:>10}   {}",
        "Provider", "Chat 模型", "搜索模型", "状态"
    ));
    lines.push("─".repeat(56));

    for s in &summaries {
        let total_models = s.chat_models.len() + s.search_models.len();

        // Determine status.
        let status = if total_models == 0 {
            "❌ 无模型".to_string()
        } else if let Some(cooldown) = &ctx.search_cooldown {
            // Check if any search model's provider is in cooldown.
            // SearchProviderCooldown tracks by provider name.
            if cooldown.is_cooled_down(&s.name) {
                "⏱️ 冷却中".to_string()
            } else {
                "✅ 正常".to_string()
            }
        } else {
            "✅ 正常".to_string()
        };

        lines.push(format!(
            "{:<16} {:>10} {:>10}   {}",
            s.name,
            s.chat_models.len(),
            s.search_models.len(),
            status,
        ));
    }

    // Also show credential pool status for chat providers (if available).
    lines.push(String::new());
    lines.push("_模型详情请使用 /models_".to_string());

    lines.join("\n")
}

pub fn cmd_tools(ctx: CommandContext<'_>) -> String {
    let tools = ctx.agent.tools();
    let names = tools.tool_names_sorted();
    if names.is_empty() {
        return "⚠️ 没有注册的工具。".to_string();
    }
    let mut lines = vec![format!("🔧 **已注册工具 ({}个)**\n", names.len())];
    for name in &names {
        if let Some(tool) = tools.get(name) {
            let desc = tool.description();
            let short = desc.trim().lines().next().unwrap_or(desc);
            let truncated = if short.chars().count() > 60 {
                format!("{}...", short.chars().take(57).collect::<String>())
            } else {
                short.to_string()
            };
            lines.push(format!("- **{}** — {}", name, truncated));
        }
    }
    lines.join("\n")
}

pub async fn cmd_context(ctx: CommandContext<'_>) -> String {
    // Prefer session-level model_override if available, otherwise fall back to registry default.
    let (model_id, context_window) = if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        let model = guard.session_override().model.clone()
            .unwrap_or_else(|| {
                ctx.registry.get_chat_provider(crate::providers::Capability::Chat)
                    .ok()
                    .map(|(_, id)| id)
                    .unwrap_or_default()
            });
        let cw = ctx.registry.get_chat_model_config(&model)
            .ok()
            .and_then(|cfg| cfg.context_window)
            .unwrap_or(0);
        (model, cw)
    } else {
        match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
            Ok((_, id)) => {
                let cw = ctx.registry.get_chat_model_config(&id)
                    .ok()
                    .and_then(|cfg| cfg.context_window)
                    .unwrap_or(0);
                (id, cw)
            }
            Err(_) => return "❌ 无法获取模型信息。".to_string(),
        }
    };

    if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        let tracker_total = guard.token_total();
        let history_len = guard.session().history.len();
        let session = guard.session();

        // Estimate actual context size from current history (system prompt + all messages).
        let estimated_total: u64 = session.history.iter()
            .map(crate::agents::agent_impl::estimate_message_tokens)
            .sum();

        // Use the larger of tracker and estimate for display.
        let total = std::cmp::max(tracker_total, estimated_total);

        let summary_info = if let Some(ref meta) = session.summary_metadata {
            format!(
                "压缩版本: v{}\n压缩到消息: #{}\n摘要估算 token: {}",
                meta.version, meta.up_to_message, meta.token_estimate
            )
        } else {
            "尚未压缩".to_string()
        };

        let usage_pct = if context_window > 0 {
            format!("{:.1}%", (total as f64 / context_window as f64) * 100.0)
        } else {
            "未知".to_string()
        };

        // Use actual config threshold instead of hardcoded 0.7.
        let compact_threshold = guard.compact_threshold();
        let threshold = if context_window > 0 {
            let t = (context_window as f64 * compact_threshold) as u64;
            format!("{} token ({:.0}%)", t, compact_threshold * 100.0)
        } else {
            "未知".to_string()
        };

        let (input, cached, output) = guard.last_usage();
        let used_kb = total * 4 / 1024;
        let window_kb = context_window * 4 / 1024;

        let usage_detail = if cached > 0 {
            let total_input = input.saturating_add(cached);
            let cache_rate = if total_input > 0 {
                format!(" | 缓存命中率: {:.0}%", (cached as f64 / total_input as f64) * 100.0)
            } else {
                String::new()
            };
            format!("(输入: {} | 缓存: {} | 输出: {}){}", input, cached, output, cache_rate)
        } else {
            format!("(输入: {} | 输出: {})", input, output)
        };

        format!(
            "📐 **上下文详情**\n\n\
             模型: `{}`\n\
             上下文窗口: {} token (~{}KB)\n\
             当前使用: {} token {} (~{}KB, {})\n\
             压缩阈值: {}\n\
             历史消息: {} 条\n\
             压缩状态: {}",
            model_id, context_window, window_kb, total, usage_detail, used_kb, usage_pct, threshold, history_len, summary_info
        )
    } else {
        // agent_loop is None: restart or session switch before first message.
        let session = ctx.session_manager.get_or_create(ctx.user_id);
        if session.history.is_empty() {
            format!(
                "📐 **上下文详情**\n\n\
                 模型: `{}`\n\
                 上下文窗口: {} token\n\
                 状态: 新会话，无历史",
                model_id, context_window
            )
        } else if let Some(total) = session.last_total_tokens {
            let usage_pct = if context_window > 0 {
                format!("{:.1}%", (total as f64 / context_window as f64) * 100.0)
            } else {
                "未知".to_string()
            };
            let compact_threshold = ctx.agent.compact_threshold();
            let threshold = if context_window > 0 {
                let t = (context_window as f64 * compact_threshold) as u64;
                format!("{} token ({:.0}%)", t, compact_threshold * 100.0)
            } else {
                "未知".to_string()
            };
            let used_kb = total * 4 / 1024;
            let window_kb = context_window * 4 / 1024;
            let summary_info = if let Some(ref meta) = session.summary_metadata {
                format!(
                    "压缩版本: v{}\n压缩到消息: #{}\n摘要估算 token: {}",
                    meta.version, meta.up_to_message, meta.token_estimate
                )
            } else {
                "尚未压缩".to_string()
            };
            format!(
                "📐 **上下文详情**\n\n\
                 模型: `{}`\n\
                 上下文窗口: {} token (~{}KB)\n\
                 当前使用: {} token (~{}KB, {})\n\
                 压缩阈值: {}\n\
                 历史消息: {} 条\n\
                 压缩状态: {}",
                model_id, context_window, window_kb,
                total, used_kb, usage_pct,
                threshold, session.history.len(), summary_info
            )
        } else {
            // History exists but no stored token count (e.g. session predates
            // token persistence). Don't estimate — just report as unknown.
            format!(
                "📐 **上下文详情**\n\n\
                 模型: `{}`\n\
                 上下文窗口: {} token\n\
                 当前使用: 暂无记录（发送一条消息后获取精确值）\n\
                 历史消息: {} 条\n\
                 压缩状态: {}",
                model_id, context_window,
                session.history.len(),
                if let Some(ref meta) = session.summary_metadata {
                    format!("已压缩 v{}", meta.version)
                } else {
                    "尚未压缩".to_string()
                }
            )
        }
    }
}

pub async fn cmd_btw(args: &str, ctx: CommandContext<'_>) -> String {
    if args.is_empty() {
        return "💡 **旁路提问**\n\n\
               用法: `/btw 你的问题`\n\n\
               旁路提问使用独立请求回答，不影响当前会话上下文。".to_string();
    }

    // Run a one-shot query using the same model, without touching session history.
    match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
        Ok((provider, model_id)) => {
            let messages = vec![
                crate::providers::ChatMessage::system_text(
                    "你是一个简洁有用的助手。用中文简要回答以下问题，不超过200字。"
                ),
                crate::providers::ChatMessage::user_text(args.to_string()),
            ];
            let req = crate::providers::ChatRequest {
                model: &model_id,
                messages: &messages,
                temperature: None,
                max_tokens: Some(800),
                thinking: None,
                stop: None,
                seed: None,
                tools: None,
                stream: true,
            };
            match provider.chat(req) {
                Ok(stream) => {
                    // Collect the stream.
                    use futures_util::StreamExt;
                    let mut text = String::new();
                    let mut rx = stream;
                    while let Some(event) = rx.next().await {
                        match event {
                            crate::providers::StreamEvent::Delta { text: delta } => text.push_str(&delta),
                            crate::providers::StreamEvent::Error(e) => {
                                return format!("❌ 旁路提问失败: {}", e);
                            }
                            crate::providers::StreamEvent::Done { .. } => break,
                            _ => {}
                        }
                    }
                    if text.trim().is_empty() {
                        "⚠️ 旁路提问返回空结果。".to_string()
                    } else {
                        format!("💡 *（旁路提问，不影响上下文）*\n\n{}", text)
                    }
                }
                Err(e) => format!("❌ 旁路提问请求失败: {}", e),
            }
        }
        Err(e) => format!("❌ 无法获取模型: {}", e),
    }
}

pub async fn cmd_export(ctx: CommandContext<'_>) -> String {
    let history = match get_history(&ctx).await {
        Some(h) => h,
        None => return "ℹ️ 当前会话为空，无法导出。".to_string(),
    };
    let sk_display = ctx.user_id.to_string();

    let mut lines = vec![format!(
        "📤 **会话导出** — {}\n\n---\n",
        sk_display
    )];
    for (i, msg) in history.iter().enumerate() {
        let role_emoji = match msg.role.as_str() {
            "user" => "👤",
            "assistant" => "🤖",
            "tool" => "🔧",
            "system" => "📋",
            _ => "❓",
        };
        let text = msg.text_content();
        let display = if text.chars().count() > 200 {
            format!("{}...", text.chars().take(197).collect::<String>())
        } else if text.is_empty() {
            "(无文本内容)".to_string()
        } else {
            text.clone()
        };
        lines.push(format!("**{}[{}]** {}\n", role_emoji, i, display));
    }
    lines.push(format!("\n---\n_共 {} 条消息_", history.len()));
    lines.join("\n")
}

pub async fn cmd_mcp(ctx: CommandContext<'_>) -> String {
    match ctx.mcp_manager {
        Some(mgr) => {
            let connected = mgr.is_connected().await;
            let servers = mgr.server_count().await;
            let tools = mgr.tool_count().await;
            if connected {
                format!(
                    "🔌 **MCP 状态**\n\n\
                     状态: ✅ 已连接\n\
                     服务器: {} 个\n\
                     MCP 工具: {} 个",
                    servers, tools
                )
            } else {
                "🔌 **MCP 状态**\n\n状态: ❌ 未连接\n\n\
                 请检查配置文件中的 `[mcp_servers]` 部分。".to_string()
            }
        }
        None => "🔌 **MCP 状态**\n\n未配置 MCP 服务器。".to_string(),
    }
}

pub fn cmd_skill(ctx: CommandContext<'_>) -> String {
    let skills = ctx.agent.skills().read();
    let count = skills.skill_count();
    if count == 0 {
        return "📚 没有加载任何 skill。".to_string();
    }

    let mut lines = vec![format!("📚 **已加载 Skill ({}个)**\n", count)];
    let mut entries: Vec<_> = skills.skills_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (name, skill) in entries {
        let desc = if skill.description.is_empty() {
            "（无描述）".to_string()
        } else if skill.description.chars().count() > 80 {
            format!("{}...", skill.description.chars().take(77).collect::<String>())
        } else {
            skill.description.clone()
        };
        let kw = if skill.keywords.is_empty() {
            String::new()
        } else {
            let kw_str: Vec<&str> = skill.keywords.iter().map(|s| s.as_str()).take(5).collect();
            format!(" `[{}]`", kw_str.join(", "))
        };
        let ver = skill.version.as_deref()
            .map(|v| format!(" v{}", v))
            .unwrap_or_default();
        let invocable_mark = match (skill.user_invocable, skill.agent_invocable) {
            (true, true)   => String::new(),
            (true, false)  => " 👤".to_string(),
            (false, true)  => " 🤖".to_string(),
            (false, false) => " 🚫".to_string(),
        };
        lines.push(format!("- **{}**{}{}{} — {}", name, ver, invocable_mark, kw, desc));
    }
    lines.join("\n")
}
