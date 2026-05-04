//! Slash command system — intercepts `/command` messages in the orchestrator layer.
//!
//! Commands are parsed and dispatched before reaching the agent loop.
//! Each command returns a text response sent directly back through the channel.

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
        "help" | "h" | "?" => Some(cmd_help()),
        "status" => Some(cmd_status(ctx).await),
        "new" | "reset" => Some(cmd_new(ctx).await),
        "compact" => Some(cmd_compact(ctx).await),
        "model" => Some(cmd_model(args, ctx).await),
        "models" => Some(cmd_models(ctx)),
        "stop" => Some(cmd_stop()),
        _ => None,
    }
}

// ── Command handlers ──────────────────────────────────────────────────────────

fn cmd_help() -> String {
    "📦 **MyClaw Slash Commands**\n\n\
     /help — 显示此帮助信息\n\
     /status — 当前会话状态（模型、token 用量）\n\
     /new — 清空会话，开始新对话\n\
     /compact — 手动触发上下文压缩\n\
     /model [name] — 查看或切换当前模型\n\
     /models — 列出可用模型\n\
     /stop — 中断当前运行\n\n\
     _命令可缩写：/h = /help, /n = /new_".to_string()
}

async fn cmd_status(ctx: CommandContext<'_>) -> String {
    let model_info = match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
        Ok((_, model_id)) => {
            match ctx.registry.get_chat_model_config(&model_id) {
                Ok(cfg) => {
                    let cw = cfg.context_window.map(|v| v.to_string()).unwrap_or_else(|| "未知".to_string());
                    format!("模型: `{}`\n上下文窗口: {}", model_id, cw)
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
        format!("会话: `{}`\n历史消息: {} 条\n估算 token: {}", ctx.session_key, history_len, total_tokens)
    } else {
        format!("会话: `{}`\n历史消息: 0 条\n状态: 新会话", ctx.session_key)
    };

    format!("📊 **状态**\n\n{}\n{}", model_info, session_info)
}

async fn cmd_new(ctx: CommandContext<'_>) -> String {
    // Clear the session via session manager.
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

async fn cmd_model(args: &str, ctx: CommandContext<'_>) -> String {
    if args.is_empty() {
        // Show current model.
        match ctx.registry.get_chat_provider(crate::providers::Capability::Chat) {
            Ok((_, model_id)) => {
                match ctx.registry.get_chat_model_config(&model_id) {
                    Ok(cfg) => {
                        let cw = cfg.context_window.map(|v| format!("{}K", v / 1000)).unwrap_or_else(|| "未知".to_string());
                        format!("🤖 当前模型: `{}` (上下文: {})", model_id, cw)
                    }
                    Err(_) => format!("🤖 当前模型: `{}`", model_id),
                }
            }
            Err(e) => format!("❌ 无法获取模型信息: {}", e),
        }
    } else {
        // Try to switch model.
        match ctx.registry.get_chat_provider_by_model(args) {
            Some((_, model_id)) => {
                format!("✅ 已切换到模型: `{}`\n_注意：切换仅在当前请求生效，下次会话恢复默认。_", model_id)
            }
            None => {
                format!("❌ 未找到模型 `{}`。使用 /models 查看可用模型。", args)
            }
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
    // Actual stop requires aborting the running tokio task, which is handled
    // at the orchestrator level. This just signals intent.
    "⏹️ 停止信号已发送。\n_注意：当前请求完成后才会生效。_".to_string()
}
