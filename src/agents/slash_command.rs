//! Slash command system — intercepts `/command` messages in the orchestrator layer.
//!
//! Commands are parsed and dispatched before reaching the agent loop.
//! Each command returns a text response sent directly through the channel.

use crate::agents::agent_impl::{Agent, AgentLoop};
use crate::agents::mcp_manager::McpManager;
use crate::agents::session_manager::SessionManager;
use crate::providers::ServiceRegistry;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

/// Context available to all command handlers.
pub struct CommandContext<'a> {
    pub user_id: &'a str,
    pub registry: &'a Arc<dyn ServiceRegistry>,
    pub session_manager: &'a SessionManager,
    pub agent: &'a Agent,
    /// Access to the current session's agent loop (if it exists).
    pub agent_loop: Option<&'a Arc<TokioMutex<AgentLoop>>>,
    /// MCP manager (for /mcp command).
    pub mcp_manager: Option<&'a Arc<McpManager>>,
    /// Sessions cache — needed by /new to evict stale agent loops.
    pub sessions: &'a DashMap<String, Arc<TokioMutex<AgentLoop>>>,
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
        "new" | "reset" => Some(cmd_new(args, ctx).await),
        "compact" => Some(cmd_compact(ctx).await),
        "model" => Some(cmd_model(args, ctx)),
        "models" => Some(cmd_models(ctx)),
        "stop" => Some(cmd_stop()),
        // ── Batch 2: enhanced ──
        "tools" => Some(cmd_tools(ctx)),
        "config" => Some(cmd_config(args, ctx)),
        "think" => Some(cmd_think(args)),
        "mcp" => Some(cmd_mcp(ctx).await),
        "context" => Some(cmd_context(ctx).await),
        "btw" => Some(cmd_btw(args, ctx).await),
        "export" => Some(cmd_export(ctx).await),
        "history" => Some(cmd_history(ctx).await),
        // ── Batch 3 ──
        "skills" | "skill" => Some(cmd_skill(ctx)),
        "reload" => Some(cmd_reload(ctx).await),
        // ── Batch 4: session management ──
        "sessions" | "ss" => Some(cmd_sessions(ctx)),
        "switch" | "sw" => Some(cmd_switch(args, ctx).await),
        "rename" | "rn" => Some(cmd_rename(args, ctx)),
        "delete" | "del" => Some(cmd_delete(args, ctx).await),
        _ => None,
    }
}

/// Get session history: from active agent loop if available, otherwise from session_manager.
async fn get_history(ctx: &CommandContext<'_>) -> Option<Vec<crate::providers::ChatMessage>> {
    if let Some(loop_arc) = ctx.agent_loop {
        let guard = loop_arc.lock().await;
        if !guard.session().history.is_empty() {
            return Some(guard.session().history.clone());
        }
    }
    let session = ctx.session_manager.get_or_create(ctx.user_id);
    if session.history.is_empty() {
        None
    } else {
        Some(session.history)
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
     /new [名称] — 创建新会话\n\
     /sessions — 列出所有会话\n\
     /switch <序号> — 切换到指定会话\n\
     /rename <序号> <名称> — 重命名会话\n\
     /delete <序号> — 删除会话\n\
     /compact — 手动触发上下文压缩\n\
     /model [name] — 查看或切换当前模型\n\
     /models — 列出可用模型\n\
     /stop — 中断当前运行\n\n\
     **工具与配置**\n\
     /tools — 列出可用工具及说明\n\
     /skills — 列出已加载的 skill\n\
     /config [key] — 查看运行时配置\n\
     /think [on|off|minimal|low|medium|high] — 控制推理模式\n\n\
     **上下文**\n\
     /context — 上下文窗口使用详情\n\
     /history — 显示会话历史摘要\n\
     /export — 导出当前会话\n\n\
     **其他**\n\
     /mcp — 查看 MCP 服务器状态\n\
     /btw <问题> — 旁路提问，不影响上下文\n\n\
     _别名: /h=/help, /n=/new, /ss=/sessions, /sw=/switch, /rn=/rename, /del=/delete_".to_string()
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
            ctx.user_id, history_len, total_tokens
        )
    } else {
        format!("会话: `{}`\n状态: 新会话", ctx.user_id)
    };

    format!("📊 **状态**\n\n{}\n{}", model_info, session_info)
}

async fn cmd_new(args: &str, ctx: CommandContext<'_>) -> String {
    let name = if args.trim().is_empty() { None } else { Some(args.trim()) };
    // Evict cached agent loop so next message creates a fresh one.
    ctx.sessions.remove(ctx.user_id);
    match ctx.session_manager.new_session(ctx.user_id, name) {
        Ok(info) => {
            let display = info.display_name.as_deref().unwrap_or("(未命名)");
            format!("🆕 新会话已创建：**{}** (`{}`)", display, info.id)
        }
        Err(e) => format!("❌ 创建会话失败: {}", e),
    }
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
        let model_info = match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
            Ok((_, model_id)) => model_id,
            Err(_) => "未配置".to_string(),
        };
        let tools = ctx.agent.tools();
        let skills = ctx.agent.skills();
        let skills_count = skills.read().skill_count();
        format!(
            "⚙️ **运行时配置**\n\n\
             模型: `{}`\n\
             工具数: {}\n\
             Skill数: {}\n\
             会话: `{}`",
            model_info,
            tools.tool_count(),
            skills_count,
            ctx.user_id,
        )
    } else {
        let key = args.trim().to_lowercase();
        match key.as_str() {
            "model" | "模型" => cmd_model("", ctx),
            "tools" | "工具" => cmd_tools(ctx),
            "skills" => cmd_skill(ctx),
            _ => format!("⚠️ 未知配置项: `{}`\n可查看: model, tools, skill", args),
        }
    }
}

fn cmd_think(args: &str) -> String {
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

async fn cmd_mcp(ctx: CommandContext<'_>) -> String {
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

        let used_kb = total * 4 / 1024;
        let window_kb = context_window * 4 / 1024;

        format!(
            "📐 **上下文详情**\n\n\
             模型: `{}`\n\
             上下文窗口: {} token (~{}KB)\n\
             当前使用: {} token (~{}KB, {})\n\
             压缩阈值: {}\n\
             历史消息: {} 条\n\
             压缩状态: {}",
            model_id, context_window, window_kb, total, used_kb, usage_pct, threshold, history_len, summary_info
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

async fn cmd_btw(args: &str, ctx: CommandContext<'_>) -> String {
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

async fn cmd_export(ctx: CommandContext<'_>) -> String {
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

async fn cmd_history(ctx: CommandContext<'_>) -> String {
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
        let first_line = text.lines().next().unwrap_or("");

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

// ════════════════════════════════════════════════════════════════════════════════
// Batch 3: Skill
// ════════════════════════════════════════════════════════════════════════════════

fn cmd_skill(ctx: CommandContext<'_>) -> String {
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
        lines.push(format!("• **{}**{} — {}", name, kw, desc));
    }
    lines.join("\n")
}

async fn cmd_reload(ctx: CommandContext<'_>) -> String {
    let workspace_dir = ctx.agent.workspace_dir();

    // 1. Re-scan skills
    let skills_dir = std::path::Path::new(workspace_dir).join("skills");
    let new_defs = crate::agents::skill_loader::load_skills_from_dir(&skills_dir);
    let new_skills: Vec<crate::agents::Skill> =
        new_defs.iter().map(crate::agents::Skill::from_definition).collect();
    {
        let mut skills = ctx.agent.skills().write();
        skills.reload(new_skills);
    }

    // 2. Re-scan agents
    let agents_dir = std::path::Path::new(workspace_dir).join("agents");
    let new_agents = crate::agents::agent_loader::load_agents_from_dir(&agents_dir);
    {
        let mut configs = ctx.agent.sub_agent_configs().write();
        *configs = new_agents;
    }

    // 3. No need to reset attachment manager — next diff rebuilds from history.

    let skill_count = ctx.agent.skills().read().skill_count();
    let agent_count = ctx.agent.sub_agent_configs().read().len();

    format!("🔄 已重新加载：{} 个 skills，{} 个 agents", skill_count, agent_count)
}

// ════════════════════════════════════════════════════════════════════════════════
// Batch 4: Session management
// ════════════════════════════════════════════════════════════════════════════════

fn cmd_sessions(ctx: CommandContext<'_>) -> String {
    let sessions = ctx.session_manager.list_sessions(ctx.user_id);
    if sessions.is_empty() {
        return "ℹ️ 没有会话记录。".to_string();
    }

    let active_id = ctx.session_manager.active_session_id(ctx.user_id);

    let mut lines = vec!["📂 **会话列表**\n".to_string()];
    for (i, s) in sessions.iter().enumerate() {
        let marker = if active_id.as_deref() == Some(&s.id) { " ← 当前" } else { "" };
        let name = s.display_name.as_deref().unwrap_or("(未命名)");
        let msg_count = s.message_count;
        lines.push(format!("{}. **{}**{} — {}条消息 — `{}`",
            i + 1, name, marker, msg_count, s.id));
    }
    lines.push("\n/new [名称] — 新建  /switch <N> — 切换  /rename <N> <名称> — 重命名  /delete <N> — 删除".to_string());
    lines.join("\n")
}

async fn cmd_switch(args: &str, ctx: CommandContext<'_>) -> String {
    let n = match args.trim().parse::<usize>() {
        Ok(n) if n > 0 => n - 1,
        _ => return "⚠️ 用法: /switch <序号>\n用 /sessions 查看会话列表。".to_string(),
    };

    let sessions = ctx.session_manager.list_sessions(ctx.user_id);
    let target = match sessions.get(n) {
        Some(s) => s.clone(),
        None => return format!("⚠️ 序号 {} 无效，当前共 {} 个会话。", n + 1, sessions.len()),
    };

    // Evict cached agent loop.
    ctx.sessions.remove(ctx.user_id);

    match ctx.session_manager.switch_session(ctx.user_id, &target.id) {
        Ok(info) => {
            let name = info.display_name.as_deref().unwrap_or("(未命名)");
            format!("✅ 已切换到：**{}** (`{}`)", name, info.id)
        }
        Err(e) => format!("❌ 切换失败: {}", e),
    }
}

fn cmd_rename(args: &str, ctx: CommandContext<'_>) -> String {
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

async fn cmd_delete(args: &str, ctx: CommandContext<'_>) -> String {
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
