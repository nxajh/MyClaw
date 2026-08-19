//! Agent delegate tool — allows the main agent to delegate tasks to sub-agents.
//!
//! This tool is the core of the multi-agent orchestration pattern.
//! The main agent calls `agent_delegate(agent="coder", task="...", mode="sync")` and the tool:
//! 1. Looks up the sub-agent by name
//! 2. Creates a temporary AgentLoop with the sub-agent's system prompt and tools
//! 3. Runs the sub-agent to completion (sync) or in background (async)
//! 4. Returns the result (sync) or session id (async) to the main agent
//!
//! H47: this tool now talks to [`AgentDelegator`] (the RFC v2 trait that
//! carries `&Session`); the legacy `TaskDelegator` trait was deleted.

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use crate::agents::AgentDelegator;
use crate::providers::{Tool, ToolResult};

/// The agent_delegate tool — injectable delegator for runtime dispatch.
pub struct AgentDelegateTool {
    delegator: Arc<dyn AgentDelegator>,
    /// Cached list of available sub-agents `(name, description)`.
    agents: Vec<(String, Option<String>)>,
}

impl AgentDelegateTool {
    pub fn new(delegator: Arc<dyn AgentDelegator>) -> Self {
        let agents = delegator.list_available();
        Self { delegator, agents }
    }
}

#[async_trait]
impl Tool for AgentDelegateTool {
    fn name(&self) -> &str {
        "agent_delegate"
    }

    fn description(&self) -> &str {
        "Delegate a task to a specialized sub-agent. Each sub-agent has its own system prompt and tool set. \
         Use this to break complex tasks into specialized sub-tasks that are handled by experts. \
         mode is REQUIRED: 'sync' blocks until the sub-agent finishes; 'async' returns the sub-agent's session id immediately \
         and the sub-agent runs in the background — you will be notified when it completes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let agent_names: Vec<&str> = self.agents.iter().map(|(n, _)| n.as_str()).collect();
        let agent_descs: Vec<String> = self
            .agents
            .iter()
            .map(|(n, d)| {
                format!(
                    "- {}: {}",
                    n,
                    d.as_deref().unwrap_or("No description")
                )
            })
            .collect();

        let agent_description = if agent_names.is_empty() {
            "Name of the sub-agent to delegate to.".to_string()
        } else {
            format!(
                "Name of the sub-agent to delegate to. Available agents:\n{}",
                agent_descs.join("\n")
            )
        };

        let mut schema = json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": agent_description
                },
                "task": {
                    "type": "string",
                    "description": "A clear description of the task to delegate."
                },
                "mode": {
                    "type": "string",
                    "enum": ["sync", "async"],
                    "description": "REQUIRED execution mode. 'sync' blocks until the sub-agent finishes and returns its result. 'async' spawns the sub-agent in the background and returns its session id immediately (results arrive later as a notification)."
                },
                "allowed_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of allowed tool names for the sub-agent. If provided, the sub-agent can only use tools in this list (intersected with its config)."
                },
                "workspace": {
                    "type": "string",
                    "description": "Optional workspace for the sub-agent: the git repository root (absolute path) where the sub-agent works. REQUIRED when the target agent uses worktree isolation — the sub-agent's worktree is created inside this repository and its branch is merged back here on completion. For shared-isolation agents, optional: the sub-agent's working directory is pointed at it."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Maximum wall-clock seconds for the sub-agent. Defaults to 1200s if omitted. Authoritative up to the hard ceiling of 1800s — nothing else can override or clamp it lower."
                }
            },
            "required": ["agent", "task", "mode"]
        });

        if !agent_names.is_empty() {
            schema["properties"]["agent"]["enum"] = json!(agent_names);
        }

        schema
    }

    fn max_output_tokens(&self) -> usize {
        20_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let agent_name = args["agent"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'agent' is required"))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'task' is required"))?;
        let mode = resolve_mode(&args)?;
        let timeout = args["timeout"].as_u64();
        let workspace = args["workspace"].as_str();
        let allowed_tools = args["allowed_tools"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });

        tracing::info!(agent = %agent_name, task_len = task.len(), mode = %mode, timeout = ?timeout, "delegating task to sub-agent");

        if mode == "async" {
            match self.delegator.delegate_async(agent_name, task, session, timeout, allowed_tools.clone(), workspace) {
                Ok(sub_session_id) => Ok(ToolResult {
                    success: true,
                    output: json!({
                        "ok": true,
                        "mode": "async",
                        "session_id": sub_session_id,
                        "message": "Sub-agent spawned in background. Use agent_list to check status."
                    })
                    .to_string(),
                    error: None,
                }),
                Err(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to spawn sub-agent '{}': {}", agent_name, e)),
                }),
            }
        } else {
            match self
                .delegator
                .delegate(agent_name, task, session, timeout, allowed_tools, workspace)
                .await
            {
                Ok(result) => Ok(ToolResult {
                    success: true,
                    output: result,
                    error: None,
                }),
                Err(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Sub-agent '{}' failed: {}", agent_name, e)),
                }),
            }
        }
    }
}

/// Resolve the required `mode` argument of `agent_delegate`.
///
/// 2026-08-14: `mode` became a mandatory parameter — there is no implicit
/// default anymore (omitting it used to silently mean `sync`, which made
/// model slips indistinguishable from an explicit choice). Values outside
/// `{"sync", "async"}` are rejected here as well, mirroring the JSON schema
/// enum at the execution layer.
fn resolve_mode(args: &serde_json::Value) -> anyhow::Result<&str> {
    let mode = args
        .get("mode")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow::anyhow!("'mode' is required ('sync' or 'async')"))?;
    match mode {
        "sync" | "async" => Ok(mode),
        other => anyhow::bail!("invalid mode '{}': must be 'sync' or 'async'", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mode_is_required_and_validated() {
        // Both legal values pass.
        assert_eq!(resolve_mode(&json!({"mode": "sync"})).unwrap(), "sync");
        assert_eq!(resolve_mode(&json!({"mode": "async"})).unwrap(), "async");
        // Omitted → error (no implicit default).
        assert!(resolve_mode(&json!({})).is_err());
        assert!(resolve_mode(&json!({"agent": "coder", "task": "x"})).is_err());
        assert!(resolve_mode(&json!({"mode": null})).is_err());
        // Values outside the enum → error.
        assert!(resolve_mode(&json!({"mode": "SYNC"})).is_err());
        assert!(resolve_mode(&json!({"mode": "background"})).is_err());
        assert!(resolve_mode(&json!({"mode": "auto"})).is_err());
    }

    /// Minimal delegator stub — only `list_available` is needed by the
    /// schema test; `delegate` is never called.
    struct StubDelegator;

    #[async_trait::async_trait]
    impl AgentDelegator for StubDelegator {
        async fn delegate(
            &self,
            _agent_name: &str,
            _task: &str,
            _parent_session: &crate::agents::session::Session,
            _timeout: Option<u64>,
            _allowed_tools: Option<Vec<String>>,
            _workspace: Option<&str>,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("stub"))
        }

        fn list_available(&self) -> Vec<(String, Option<String>)> {
            vec![]
        }
    }

    #[test]
    fn schema_requires_mode() {
        let tool = AgentDelegateTool::new(Arc::new(StubDelegator));
        let schema = tool.parameters_schema();

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required list")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            required.contains(&"mode"),
            "mode must be required: {required:?}"
        );
        assert!(required.contains(&"agent"));
        assert!(required.contains(&"task"));

        // No implicit default: `mode` has no "default" key, only the enum.
        assert!(schema["properties"]["mode"].get("default").is_none());
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            json!(["sync", "async"])
        );
        assert!(schema["properties"]["mode"]["description"]
            .as_str()
            .unwrap()
            .contains("REQUIRED"));
    }
}
