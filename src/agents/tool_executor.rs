use std::sync::Arc;
use std::time::Duration;

use super::ask_router::AskRouter;
use super::session::Session;
use super::tokens::is_write_tool;
use super::tool_registry::ToolRegistry;
use crate::channels::{ChannelMessageContent, ChannelOutboundMessage, InlineButton, MessageReceiver};
use crate::config::agent::PermissionMode;
use crate::providers::ToolCall;
use crate::providers::capability_tool::ToolResult;

/// Executes tool calls on behalf of `Agent::run`.
///
/// Per the RFC v2 target shape, the executor is a global singleton —
/// it carries only the timeout policy and dispatches against a
/// `&[Arc<dyn Tool>]` slice the caller (Agent::run) has already
/// filtered through the agent's `tools` / `skills` / `mcp` filters.
/// No `ToolRegistry` field; the executor is stateless w.r.t. which
/// tools an agent may call.
pub struct ToolExecutor {
    pub timeout_secs: u64,
    /// Shared `AskRouter` for per-operation approval in Default mode.
    /// When `Some`, write tools trigger an Inline Keyboard approval
    /// prompt before execution. `None` in CLI / test contexts.
    ask_router: Option<Arc<AskRouter>>,
}

impl ToolExecutor {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout_secs,
            ask_router: None,
        }
    }

    pub fn with_ask_router(mut self, router: Arc<AskRouter>) -> Self {
        self.ask_router = Some(router);
        self
    }

    /// Execute a single tool call against the pre-filtered `allowed`
    /// slice.
    ///
    /// `autonomy` controls write-tool blocking:
    /// - **ReadOnly**: hard-block all write tools.
    /// - **Default**: intercept write tools → send approval prompt →
    ///   wait for user decision via Inline Keyboard buttons.
    /// - **Full**: execute everything immediately.
    ///
    /// Special tools (`ask_user`, `agent_delegate`) are real `Tool`
    /// impls in `allowed`, dispatched through the same path as any
    /// other tool.
    pub(crate) async fn execute(
        &self,
        call: &ToolCall,
        session: &mut Session,
        autonomy: Option<&PermissionMode>,
        allowed: &[Arc<dyn crate::providers::Tool>],
    ) -> anyhow::Result<ToolResult> {
        // Autonomy enforcement.
        if let Some(autonomy) = autonomy {
            if matches!(autonomy, PermissionMode::ReadOnly) && is_write_tool(&call.name) {
                tracing::info!(tool = %call.name, "tool blocked by ReadOnly autonomy policy");
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Tool '{}' is not allowed in read-only mode (autonomy: ReadOnly).",
                        call.name
                    ),
                    error: Some("autonomy_policy: ReadOnly".to_string()),
                });
            }

            // Default mode: approval only for shell commands that contain
            // process/service management commands (kill, systemctl, etc.).
            // All other tools (file_write, file_edit, http, etc.) are
            // auto-approved — these operate within the workspace and pose
            // no system-level risk.
            if matches!(autonomy, PermissionMode::Default)
                && !Self::is_approval_exempt(&call.name)
                && Self::needs_approval(&call.name, &call.arguments)
            {
                match self.request_approval(call, session).await {
                    ApprovalDecision::Approved => {
                        tracing::info!(
                            tool = %call.name,
                            session = %session.id,
                            "tool approved by user"
                        );
                    }
                    ApprovalDecision::Denied => {
                        tracing::info!(
                            tool = %call.name,
                            session = %session.id,
                            "tool denied by user"
                        );
                        return Ok(ToolResult {
                            success: false,
                            output: format!(
                                "User denied execution of tool '{}'.",
                                call.name
                            ),
                            error: Some("user_denied".to_string()),
                        });
                    }
                    ApprovalDecision::Skipped => {
                        // No ask_router / no channel — fall through to execute.
                    }
                }
            }
        }

        // Low-memory guard: reject heavy build commands (cargo, rustc, make,
        // cmake) on hosts with <512MB available RAM to prevent OOM-killing
        // the daemon. Agents should use CI instead of local builds here.
        if call.name == "shell" {
            if let Some(rejection) = Self::check_memory_guard(&call.arguments) {
                tracing::warn!(tool = %call.name, "rejected by low-memory guard");
                return Ok(rejection);
            }
        }

        // Generic tool dispatch against the filtered slice. `ask_user` /
        // `agent_delegate` are real Tool impls in `allowed` (no
        // special-casing).
        let tool = allowed
            .iter()
            .find(|t| t.spec().name == call.name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: '{}'", call.name))?;
        let args = parse_tool_args(&call.arguments);
        self.run_tool(tool.as_ref(), &call.name, args, session)
            .await
    }

    /// Tools exempt from the Default-mode approval gate.
    /// `agent_delegate` and `agent_kill` manage sub-agent lifecycles —
    /// blocking them on approval would deadlock delegation.
    fn is_approval_exempt(name: &str) -> bool {
        matches!(name, "agent_delegate" | "agent_kill")
    }

    /// Determine whether a tool call needs user approval in Default mode.
    ///
    /// Only `shell` calls containing dangerous process/service-management
    /// commands require approval. Everything else (file I/O, http, etc.)
    /// is auto-approved.
    fn needs_approval(tool_name: &str, arguments: &str) -> bool {
        if tool_name != "shell" {
            return false;
        }
        // arguments is a JSON string like {"command": "...", "workdir": "..."}
        let command = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v.get("command")?.as_str().map(String::from))
            .unwrap_or_default();
        shell_has_dangerous_command(&command)
    }

    /// Send an approval prompt with Inline Keyboard buttons and wait
    /// for the user's decision. Reuses `AskRouter` (same mechanism as
    /// `ask_user`): the orchestrator's `AskReply` interceptor fulfills
    /// the wait when the next inbound message (button click → text)
    /// arrives for this session.
    async fn request_approval(
        &self,
        call: &ToolCall,
        session: &Session,
    ) -> ApprovalDecision {
        let router = match &self.ask_router {
            Some(r) => r,
            None => return ApprovalDecision::Skipped,
        };
        let channel = match session.channel.as_ref() {
            Some(c) => c,
            None => return ApprovalDecision::Skipped,
        };
        let reply_target = match session.reply_target() {
            Some(rt) => rt.to_string(),
            None => return ApprovalDecision::Skipped,
        };

        // Build a concise preview of the tool call arguments.
        let args_preview = Self::format_args_preview(&call.arguments, 500);
        let text = format!(
            "🔒 **Approval Required**\n\nTool: `{}`\nArgs: `{}`\n\nAllow this action?",
            call.name, args_preview
        );

        let mut receiver = MessageReceiver::new(reply_target);
        if let Some(ref last_msg) = session.last_message {
            receiver.reply_to_message_id = Some(
                last_msg
                    .receiver
                    .reply_to_message_id
                    .clone()
                    .unwrap_or_else(|| last_msg.id.clone()),
            );
            receiver.thread_id = last_msg.receiver.thread_id.clone();
        }

        let message = ChannelOutboundMessage {
            receiver,
            content: ChannelMessageContent {
                text,
                files: vec![],
                buttons: vec![
                    InlineButton {
                        label: "✅ Approve".to_string(),
                        callback_data: "approve".to_string(),
                    },
                    InlineButton {
                        label: "❌ Deny".to_string(),
                        callback_data: "deny".to_string(),
                    },
                ],
            },
            options: Default::default(),
        };

        if let Err(e) = channel.send_message(&message).await {
            tracing::warn!(tool = %call.name, err = %e, "approval prompt send failed, skipping gate");
            return ApprovalDecision::Skipped;
        }

        tracing::info!(
            tool = %call.name,
            session = %session.id,
            "approval prompt sent, waiting for user decision"
        );

        match router.wait_for_reply(&session.id).await {
            Ok(reply) => {
                let decision = reply.content.text.trim().to_lowercase();
                if decision.starts_with("deny") || decision.contains("拒绝") || decision.contains("❌") {
                    ApprovalDecision::Denied
                } else {
                    // Default to approved: "approve", "allow", "yes", "允许", etc.
                    ApprovalDecision::Approved
                }
            }
            Err(e) => {
                tracing::warn!(tool = %call.name, err = %e, "approval wait failed, denying");
                ApprovalDecision::Denied
            }
        }
    }

    /// Truncate tool arguments to `max_chars` for the approval prompt.
    fn format_args_preview(args: &str, max_chars: usize) -> String {
        if args.len() <= max_chars {
            args.to_string()
        } else {
            format!("{}…", &args[..max_chars])
        }
    }

    /// Low-memory guard: returns `Some(ToolResult)` (error) if the shell
    /// command contains heavy build tools (cargo, rustc, make, cmake) and
    /// the system has <512MB available RAM. Returns `None` to allow.
    fn check_memory_guard(arguments: &str) -> Option<ToolResult> {
        let command = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v.get("command")?.as_str().map(String::from))
            .unwrap_or_default();
        if !shell_has_heavy_build(&command) {
            return None;
        }
        let avail_kb = read_mem_available_kb()?;
        const MIN_AVAIL_KB: u64 = 512 * 1024; // 512MB
        if avail_kb >= MIN_AVAIL_KB {
            return None;
        }
        let avail_mb = avail_kb / 1024;
        tracing::warn!(avail_mb, "low-memory guard triggered for build command");
        Some(ToolResult {
            success: false,
            output: format!(
                "cargo/rustc rejected: insufficient memory ({}MB available, need 512MB) \
                 on this host. Use CI instead: commit, push, and let GitHub Actions build.",
                avail_mb
            ),
            error: Some("low_memory_guard".to_string()),
        })
    }

    /// Execute a tool with timeout and framework-level output truncation.
    async fn run_tool(
        &self,
        tool: &dyn crate::providers::Tool,
        name: &str,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let raw = if self.timeout_secs > 0 && !tool.blocks_on_human() {
            let effective_secs = tool
                .preferred_timeout_secs()
                .map(|p| p.max(self.timeout_secs))
                .unwrap_or(self.timeout_secs);
            let timeout = Duration::from_secs(effective_secs);
            tokio::time::timeout(timeout, tool.execute(args, session))
                .await
                .unwrap_or_else(|_| {
                    Ok(ToolResult {
                        success: false,
                        output: format!("Tool '{}' timed out after {}s", name, effective_secs),
                        error: Some("timeout".to_string()),
                    })
                })?
        } else {
            // No timeout: either disabled globally, or a human-blocking tool
            // (`ask_user`) whose wait is bounded by a real reply / interruption,
            // not an arbitrary timer.
            tool.execute(args, session).await?
        };

        let max_tokens = tool.max_output_tokens();
        let output = crate::tools::truncation::truncate_tool_result(&raw.output, max_tokens);
        if output.len() != raw.output.len() {
            tracing::debug!(
                tool = %name,
                original_len = raw.output.len(),
                truncated_len = output.len(),
                max_tokens,
                "tool output truncated by framework"
            );
        }
        Ok(ToolResult { output, ..raw })
    }
}

/// Outcome of the Default-mode approval gate.
enum ApprovalDecision {
    Approved,
    Denied,
    /// No router/channel available — skip the gate and execute.
    Skipped,
}

pub(crate) fn parse_tool_args(arguments: &str) -> serde_json::Value {
    if arguments.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({ "raw": arguments }))
    }
}

/// Restricted tool executor for the compaction summarizer.
///
/// Only allows file read/write/edit and shell — prevents the summarizer from
/// touching session state, triggering ask_user, or spawning sub-agents.
pub(crate) struct MemoryToolExecutor {
    tools: Arc<ToolRegistry>,
}

impl MemoryToolExecutor {
    /// Tools the summarizer is permitted to call during compaction: file
    /// read/write/edit, shell, and the dedicated `memory_manage` tool. The
    /// summarizer request still advertises the *full* tool list (for prefix-cache
    /// matching with the main request); this allow-list is the actual gate —
    /// any other tool call is blocked with an error result.
    const ALLOWED: &'static [&'static str] = &[
        "file_read",
        "file_write",
        "file_edit",
        "shell",
        "memory_manage",
    ];

    pub(crate) fn new(tools: Arc<ToolRegistry>) -> Self {
        Self { tools }
    }

    pub(crate) async fn execute(
        &self,
        call: &ToolCall,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        if !Self::ALLOWED.contains(&call.name.as_str()) {
            tracing::warn!(tool = %call.name, "summarizer tried to call restricted tool, blocking");
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "tool '{}' not available during compaction summarization",
                    call.name
                )),
            });
        }
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("tool '{}' not found in registry", call.name))?;
        let args = parse_tool_args(&call.arguments);
        tool.execute(args, session).await
    }
}

/// Check whether a shell command string contains process/service-management
/// commands that require user approval in Default permission mode.
///
/// Splits on shell control operators (`|`, `&`, `;`, newline) and checks
/// the first token (command basename) of each segment. This correctly
/// handles pipelines: `ps aux | grep kill` does NOT trigger (the segment
/// starting with `grep` is not `kill`).
fn shell_has_dangerous_command(command: &str) -> bool {
    for segment in command.split(['|', '&', ';', '\n']) {
        let segment = segment.trim();
        let first_token = match segment.split_whitespace().next() {
            Some(t) => t,
            None => continue,
        };
        // Strip path prefix (e.g. /usr/bin/kill → kill)
        let basename = first_token.rsplit('/').next().unwrap_or(first_token);
        if matches!(
            basename,
            "kill" | "pkill" | "killall"
                | "systemctl" | "service"
                | "reboot" | "shutdown" | "poweroff" | "halt"
        ) {
            return true;
        }
    }
    false
}

/// Check whether a shell command string contains heavy build commands
/// (cargo, rustc, make, cmake) that consume large amounts of RAM.
///
/// Uses the same segment-splitting logic as `shell_has_dangerous_command`.
fn shell_has_heavy_build(command: &str) -> bool {
    for segment in command.split(['|', '&', ';', '\n']) {
        let segment = segment.trim();
        let first_token = match segment.split_whitespace().next() {
            Some(t) => t,
            None => continue,
        };
        let basename = first_token.rsplit('/').next().unwrap_or(first_token);
        if matches!(basename, "cargo" | "rustc" | "make" | "cmake") {
            return true;
        }
    }
    false
}

/// Read MemAvailable from `/proc/meminfo` (Linux). Returns `None` on any
/// error or on non-Linux systems.
fn read_mem_available_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
                return Some(kb);
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}


#[cfg(test)]
mod danger_tests {
    use super::shell_has_dangerous_command;

    #[test]
    fn detects_plain_kill() {
        assert!(shell_has_dangerous_command("kill -9 12345"));
    }

    #[test]
    fn detects_pkill_in_pipeline() {
        assert!(shell_has_dangerous_command(
            "ps aux | grep python | pkill -f python"
        ));
    }

    #[test]
    fn detects_systemctl_restart() {
        assert!(shell_has_dangerous_command("systemctl restart nginx"));
    }

    #[test]
    fn detects_service_command() {
        assert!(shell_has_dangerous_command("service nginx stop"));
    }

    #[test]
    fn detects_full_path() {
        assert!(shell_has_dangerous_command("/usr/bin/kill 1234"));
    }

    #[test]
    fn does_not_trigger_on_grep_kill() {
        // `grep kill` — the command is `grep`, not `kill`
        assert!(!shell_has_dangerous_command("ps aux | grep kill"));
    }

    #[test]
    fn does_not_trigger_on_skill() {
        // `skill` contains `kill` substring but is a different command
        assert!(!shell_has_dangerous_command("skill --foo"));
    }

    #[test]
    fn does_not_trigger_on_normal_commands() {
        assert!(!shell_has_dangerous_command("ls -la /tmp"));
        assert!(!shell_has_dangerous_command("cat /etc/hostname"));
        assert!(!shell_has_dangerous_command("echo hello && sleep 1"));
        assert!(!shell_has_dangerous_command(
            "cargo build --release 2>&1 | head -20"
        ));
    }
}

#[cfg(test)]
mod heavy_build_tests {
    use super::shell_has_heavy_build;

    #[test]
    fn detects_cargo_build() {
        assert!(shell_has_heavy_build("cargo build --release"));
        assert!(shell_has_heavy_build("cargo check"));
    }

    #[test]
    fn detects_rustc() {
        assert!(shell_has_heavy_build("rustc --edition 2021 main.rs"));
    }

    #[test]
    fn detects_make_cmake() {
        assert!(shell_has_heavy_build("make -j4"));
        assert!(shell_has_heavy_build("cmake .. && make"));
    }

    #[test]
    fn detects_in_pipeline() {
        assert!(shell_has_heavy_build("echo hi | cargo build 2>&1"));
    }

    #[test]
    fn detects_full_path() {
        assert!(shell_has_heavy_build("/home/ubuntu/.cargo/bin/cargo check"));
    }

    #[test]
    fn does_not_trigger_on_other_commands() {
        assert!(!shell_has_heavy_build("ls -la"));
        assert!(!shell_has_heavy_build("echo cargo"));
        assert!(!shell_has_heavy_build("git commit -m 'cargo'"));
    }
}
