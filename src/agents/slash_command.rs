//! Slash command system — intercepts `/command` messages in the orchestrator layer.
//!
//! Commands are parsed and dispatched before reaching the agent loop.
//! Each command returns a text response sent directly through the channel.

use crate::agents::agent_impl::{Agent, AgentLoop};
use crate::agents::session_manager::SessionManager;
use crate::providers::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

/// Context available to all command handlers.
pub struct CommandContext<'a> {
    pub session_key: &'a str,
    pub registry: &'a Arc<dyn ServiceRegistry>,
    pub session_manager: &'a SessionManager,
    pub agent: &'a Agent,
    /// Access to the current session's agent loop (if it exists).
    pub agent_loop: Option<&'a Arc<TokioMutex<AgentLoop>>>,
}

/// Parse a slash command from message content.
/// Returns `(command_name, args)` if the content starts with `/`.
pub fn parse_command(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let rest = &trimmed[1..];
    if rest.is_empty() {
        return None;
    }
    let (cmd, args) = match rest.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    // Reject obviously non-command input (e.g. URLs, file paths).
    if cmd.contains('/') || cmd.contains('\\') || cmd.contains('.') {
        return None;
    }
    Some((cmd, args))
}

/// Dispatch a slash command. Returns the response text, or None if unrecognized.
pub async fn dispatch(cmd: &str, args: &str, ctx: CommandContext<'_>) -> Option<String> {
    match cmd {
        // ── Batch 1: core ──
        "help" | "h" | "?" => Some(cmd_help()),
        "status" => Some(cmd_status(ctx).await),
        "new" | "reset" => Some(cmd_new(ctx).await),
        "compact" => Some(cmd_compact(ctx).await),
        "model" => Some(cmd_model(args, ctx)),
        "models" => Some(cmd_models(ctx)),
        "stop" => Some(cmd_stop()),
        // ── Batch 2: enhanced ──
        "tools" => Some(cmd_tools(ctx)),
        "config" => Some(cmd_config(args, ctx)),
        "think" => Some(cmd_think(args, ctx)),
        "mcp" => Some(cmd_mcp(ctx)),
        "context" => Some(cmd_context(ctx).await),
        "btw" => Some(cmd_btw(args)),
        "export" => Some(cmd_export(ctx).await),
        "history" => Some(cmd_history(ctx).await),
        _ => None,
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Batch 1: Core commands
// ════════════════════════════════════════════════════════════════════════════════

fn cmd_help() -> String {
    "📦 **MyClaw Slash Commands**\n\n\
     **基础**\n\
     /help — 显示此帮助信息\n\
     /status — 当前会话状态（模型、token 用量）\n\
     /new — 清空会话，开始新对话\n\
     /compact — 手动触发上下文压缩\n\
     /model [name] — 查看或切换当前模型\n\
     /models — 列出可用模型\n\
     /stop — 中断当前运行\n\n\
     **工具与配置**\n\
     /tools — 列出可用工具及说明\n\
     /config [key] — 查看运行时配置\n\
     /think [on|off|minimal|low|medium|high] — 控制推理模式\n\n\
     **上下文**\n\
     /context — 上下文窗口使用详情\n\
     /history — 显示会话历史摘要\n\
     /export — 导出当前会话\n\n\
     **其他**\n\
     /mcp — 查看 MCP 服务器状态\n\
     /btw <问题> — 旁路提问，不影响上下文\n\n\
     _别名: /h=/help, /n=/new_".to_string()
}

async fn cmd_status(ctx: CommandContext<'_>) -> String {
    let model_info = match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
        Ok((_, model_id)) => {
            match ctx.registry.get_chat_model_config(&model_id) {
                Ok(cfg) => {
                    let cw = cfg.context_window
                        .map(|v| format!("{}K", v / 1000))
                        .unwrap_or_else(|| "未知".to_string());
                    format!("模型: `{}` (上下文: {})", model_id, cw)
                }
                Err(_) => format!("模型: `{}`", model_id),
            }
        }
        Err(_) => "模型: 未配置".to_string(),
    };

    let session_info = if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        let history_len = guard.session().history.len();
        let total_tokens = guard.token_total();
        format!(
            "会话: `{}`\n历史: {} 条消息\nToken: {}",
            ctx.session_key, history_len, total_tokens
        )
    } else {
        format!("会话: `{}`\n状态: 新会话", ctx.session_key)
    };

    format!("📊 **状态**\n\n{}\n{}", model_info, session_info)
}

async fn cmd_new(ctx: CommandContext<'_>) -> String {
    ctx.session_manager.reset(ctx.session_key);
    "🆕 会话已清空，开始新对话。".to_string()
}

async fn cmd_compact(ctx: CommandContext<'_>) -> String {
    if let Some(loop_arc) = ctx.agent_loop {
        let mut guard = loop_arc.lock().await;
        let model_id = match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
            Ok((_, id)) => id,
            Err(e) => return format!("❌ 无法获取当前模型: {}", e),
        };
        match guard.compact_now(&model_id).await {
            Ok(()) => {
                let tokens = guard.token_total();
                format!("✅ 上下文压缩完成，当前 token: {}", tokens)
            }
            Err(e) => format!("❌ 压缩失败: {}", e),
        }
    } else {
        "ℹ️ 当前没有活跃会话，无需压缩。".to_string()
    }
}

fn cmd_model(args: &str, ctx: CommandContext<'_>) -> String {
    if args.is_empty() {
        match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
            Ok((_, model_id)) => {
                match ctx.registry.get_chat_model_config(&model_id) {
                    Ok(cfg) => {
                        let cw = cfg.context_window
                            .map(|v| format!("{}K", v / 1000))
                            .unwrap_or_else(|| "未知".to_string());
                        format!("🤖 当前模型: `{}` (上下文: {})", model_id, cw)
                    }
                    Err(_) => format!("🤖 当前模型: `{}`", model_id),
                }
            }
            Err(e) => format!("❌ 无法获取模型信息: {}", e),
        }
    } else {
        match ctx.registry.get_chat_provider_by_model(args) {
            Some((_, model_id)) => {
                format!(
                    "✅ 已切换到模型: `{}`\n_注意：切换仅在当前请求生效。_",
                    model_id
                )
            }
            None => format!("❌ 未找到模型 `{}`。使用 /models 查看可用模型。", args),
        }
    }
}

fn cmd_models(ctx: CommandContext<'_>) -> String {
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

fn cmd_stop() -> String {
    "⏹️ 停止信号已发送。\n_注意：当前请求完成后才会生效。_".to_string()
}

// ════════════════════════════════════════════════════════════════════════════════
// Batch 2: Enhanced commands
// ════════════════════════════════════════════════════════════════════════════════

fn cmd_tools(ctx: CommandContext<'_>) -> String {
    let tools = ctx.agent.tools();
    let names = tools.tool_names_sorted();
    if names.is_empty() {
        return "⚠️ 没有注册的工具。".to_string();
    }
    let mut lines = vec![format!("🔧 **已注册工具 ({}个)**\n", names.len())];
    for name in &names {
        if let Some(tool) = tools.get(name) {
            let desc = tool.description();
            let short = desc.lines().next().unwrap_or(desc);
            let truncated = if short.chars().count() > 60 {
                format!("{}...", short.chars().take(57).collect::<String>())
            } else {
                short.to_string()
            };
            lines.push(format!("• **{}** — {}", name, truncated));
        }
    }
    lines.join("\n")
}

fn cmd_config(args: &str, ctx: CommandContext<'_>) -> String {
    if args.is_empty() {
        // Show key config summary.
        let model_info = match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
            Ok((_, model_id)) => model_id,
            Err(_) => "未配置".to_string(),
        };
        let tools = ctx.agent.tools();
        format!(
            "⚙️ **运行时配置**\n\n\
             模型: `{}`\n\
             工具数: {}\n\
             会话: `{}`",
            model_info,
            tools.tool_count(),
            ctx.session_key,
        )
    } else {
        // Specific key lookup — just report what we know.
        let key = args.trim().to_lowercase();
        match key.as_str() {
            "model" | "模型" => cmd_model("", ctx),
            "tools" | "工具" => cmd_tools(ctx),
            _ => format!("⚠️ 未知配置项: `{}`\n可查看: model, tools", args),
        }
    }
}

fn cmd_think(args: &str, _ctx: CommandContext<'_>) -> String {
    let level = args.trim().to_lowercase();
    if level.is_empty() {
        return "🧠 **推理模式**\n\n\
               用法: `/think <level>`\n\n\
               可选值:\n\
               • `on` / `high` — 深度推理\n\
               • `medium` — 标准推理\n\
               • `low` — 轻度推理\n\
               • `minimal` — 最小推理\n\
               • `off` — 关闭推理\n\n\
               _注意：需要模型支持推理模式。_".to_string();
    }
    match level.as_str() {
        "on" | "high" => "🧠 推理模式已设为 **高** (deep thinking).\n_下次请求生效。_".to_string(),
        "medium" => "🧠 推理模式已设为 **中等**.\n_下次请求生效。_".to_string(),
        "low" => "🧠 推理模式已设为 **低**.\n_下次请求生效。_".to_string(),
        "minimal" => "🧠 推理模式已设为 **最小**.\n_下次请求生效。_".to_string(),
        "off" => "🧠 推理模式已**关闭**.\n_下次请求生效。_".to_string(),
        _ => format!("⚠️ 未知推理级别: `{}`\n可用: on, high, medium, low, minimal, off", level),
    }
}

fn cmd_mcp(_ctx: CommandContext<'_>) -> String {
    // MCP server status is not directly queryable from this context yet.
    // Show what we can.
    "🔌 **MCP 服务器**\n\n\
     MCP 连接状态需要从 daemon 层查询。\n\
     请检查配置文件中的 `[mcp_servers]` 部分。".to_string()
}

async fn cmd_context(ctx: CommandContext<'_>) -> String {
    let (model_id, context_window) = match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
        Ok((_, id)) => {
            let cw = ctx.registry.get_chat_model_config(&id)
                .ok()
                .and_then(|cfg| cfg.context_window)
                .unwrap_or(0);
            (id, cw)
        }
        Err(_) => return "❌ 无法获取模型信息。".to_string(),
    };

    if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        let total = guard.token_total();
        let history_len = guard.session().history.len();
        let session = guard.session();

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

        let threshold = if context_window > 0 {
            let t = (context_window as f64 * 0.7) as u64;
            format!("{} token (70%)", t)
        } else {
            "未知".to_string()
        };

        format!(
            "📐 **上下文详情**\n\n\
             模型: `{}`\n\
             上下文窗口: {} token\n\
             当前使用: {} token ({})\n\
             压缩阈值: {}\n\
             历史消息: {} 条\n\
             压缩状态: {}",
            model_id, context_window, total, usage_pct, threshold, history_len, summary_info
        )
    } else {
        format!(
            "📐 **上下文详情**\n\n\
             模型: `{}`\n\
             上下文窗口: {} token\n\
             状态: 新会话，无历史",
            model_id, context_window
        )
    }
}

fn cmd_btw(args: &str) -> String {
    if args.is_empty() {
        return "💡 **旁路提问**\n\n\
               用法: `/btw 你的问题`\n\n\
               旁路提问不会影响会话上下文和未来对话。\n\
               注意：当前版本中 /btw 仍会经过 agent 处理。".to_string();
    }
    // In the current architecture, we can't easily do a one-shot query
    // without affecting the session. Return a hint for now.
    format!(
        "💡 旁路提问: *{}*\n\n\
         _当前版本中，旁路提问功能尚未完全实现。\
         请直接发送消息给 agent，或等待后续更新。_",
        args
    )
}

async fn cmd_export(ctx: CommandContext<'_>) -> String {
    if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        let history = &guard.session().history;
        if history.is_empty() {
            return "ℹ️ 当前会话为空，无法导出。".to_string();
        }

        let mut lines = vec![format!(
            "📤 **会话导出** — {}\n\n---\n",
            ctx.session_key
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
            // Truncate individual messages to keep output manageable.
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
    } else {
        "ℹ️ 当前没有活跃会话。".to_string()
    }
}

async fn cmd_history(ctx: CommandContext<'_>) -> String {
    if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        let history = &guard.session().history;
        if history.is_empty() {
            return "ℹ️ 当前会话为空。".to_string();
        }

        let mut lines = vec![format!("📜 **会话历史** ({}条消息)\n", history.len())];

        // Show a condensed view: role + first line of each message.
        for (i, msg) in history.iter().enumerate() {
            let tag = match msg.role.as_str() {
                "user" => "👤",
                "assistant" => "🤖",
                "tool" => "🔧",
                "system" => "📋",
                _ => "❓",
            };
            let text = msg.text_content();
            let first_line = text.lines().next().unwrap_or("");
            let display = if first_line.chars().count() > 80 {
                format!("{}...", first_line.chars().take(77).collect::<String>())
            } else if first_line.is_empty() {
                "(无文本)".to_string()
            } else {
                first_line.to_string()
            };
            lines.push(format!("{} `[{}]` {}", tag, i, display));
        }

        lines.join("\n")
    } else {
        "ℹ️ 当前没有活跃会话。".to_string()
    }
}
