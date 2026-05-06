//! McpManager — manages MCP server lifecycle and exposes tools to the agent.
//!
//! Responsibilities:
//! - Connect to all configured MCP servers on startup
//! - Wrap discovered tools as `dyn Tool` so the ToolRegistry + ToolRegistry + SkillManager can use them
//! - Expose connection status for health checks
//!
//! DDD: McpManager depends on `crate::providers::Tool` (Domain trait) and
//! `crate::mcp::McpRegistry` (Infrastructure). It does NOT depend on the
//! Composition Root or any config concrete types.

use std::sync::Arc;

/// McpManager — MCP server lifecycle manager.
#[derive(Clone)]
pub struct McpManager {
    /// Connected registry. None until `connect()` is called.
    registry: Arc<tokio::sync::RwLock<Option<Arc<crate::mcp::McpRegistry>>>>,
    /// Cached MCP tool wrappers (rebuilt after each connect).
    tools: Arc<tokio::sync::RwLock<Vec<Arc<dyn crate::providers::Tool>>>>,
    /// Number of servers connected on the last successful `connect()`.
    server_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl McpManager {
    /// Create a new manager (does NOT connect yet).
    /// `connect()` must be called before use.
    pub fn new() -> Self {
        Self {
            registry: Arc::new(tokio::sync::RwLock::new(None)),
            tools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            server_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Connect to all configured MCP servers and build tool wrappers.
    ///
    /// This is idempotent: calling it twice will reconnect.
    pub async fn connect(&self, configs: &[crate::config::mcp::McpServerConfig]) -> anyhow::Result<()> {
        tracing::info!(count = configs.len(), "MCP manager connecting to servers");

        // Convert from config::mcp::McpServerConfig (user-facing) to
        // mcp::config_types::McpServerConfig (the one McpRegistry understands).
        let registry_configs: Vec<crate::mcp::config_types::McpServerConfig> =
            configs.iter().map(crate::mcp::config_types::McpServerConfig::from).collect();

        let registry = crate::mcp::McpRegistry::connect_all(&registry_configs).await?;

        let connected_count = registry.server_count();
        let total_tool_count = registry.tool_count();

        tracing::info!(
            servers_connected = connected_count,
            tools_available = total_tool_count,
            "MCP servers connected"
        );

        // Build wrappers. Each wrapper holds an Arc clone of the registry.
        let registry_arc = Arc::new(registry);
        let wrappers = self.build_wrappers(Arc::clone(&registry_arc)).await;

        // Store both so tools() and is_connected() can be called later.
        {
            let mut reg_lock = self.registry.write().await;
            *reg_lock = Some(Arc::clone(&registry_arc));
        }
        let wrapped_count = wrappers.len();
        {
            let mut tools_lock = self.tools.write().await;
            *tools_lock = wrappers;
        }

        self.server_count.store(connected_count, std::sync::atomic::Ordering::Relaxed);

        tracing::debug!(
            wrapped_tools = wrapped_count,
            "MCP tool wrappers built"
        );

        Ok(())
    }

    /// Build `McpToolWrapper` instances for every tool in the registry.
    async fn build_wrappers(
        &self,
        registry: Arc<crate::mcp::McpRegistry>,
    ) -> Vec<Arc<dyn crate::providers::Tool>> {
        let mut wrappers = Vec::new();
        let tool_names = registry.tool_names();

        for prefixed_name in tool_names {
            if let Some(def) = registry.get_tool_def(&prefixed_name).await {
                let wrapper = crate::mcp::McpToolWrapper::new(
                    prefixed_name.clone(),
                    def,
                    Arc::clone(&registry),
                );
                wrappers.push(Arc::new(wrapper) as Arc<dyn crate::providers::Tool>);
            }
        }

        wrappers
    }

    /// All MCP tool wrappers as `dyn Tool`, for injection into ToolRegistry + ToolRegistry + SkillManager.
    ///
    /// Returns an empty vec if `connect()` has not been called yet.
    pub async fn tools(&self) -> Vec<Arc<dyn crate::providers::Tool>> {
        self.tools.read().await.clone()
    }

    /// Number of connected MCP servers.
    pub async fn server_count(&self) -> usize {
        self.server_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of available MCP tools (after wrapping).
    pub async fn tool_count(&self) -> usize {
        self.tools.read().await.len()
    }

    /// Whether any MCP servers are connected.
    pub async fn is_connected(&self) -> bool {
        self.registry.read().await.is_some()
    }

    /// Get (name, instructions) for all connected MCP servers.
    pub async fn server_instructions(&self) -> Vec<(String, String)> {
        let reg = self.registry.read().await;
        match &*reg {
            Some(r) => r.server_instructions().await,
            None => vec![],
        }
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── McpServerConfig conversion: config::mcp → mcp::config_types ─────────────────

impl From<&crate::config::mcp::McpServerConfig> for crate::mcp::config_types::McpServerConfig {
    fn from(cfg: &crate::config::mcp::McpServerConfig) -> Self {
        use crate::config::mcp::McpTransport;
        use crate::mcp::config_types::McpTransport as TargetTransport;

        let transport = match cfg.transport {
            McpTransport::Stdio => TargetTransport::Stdio,
            McpTransport::Http => TargetTransport::Http,
            McpTransport::Sse => TargetTransport::Sse,
        };

        crate::mcp::config_types::McpServerConfig {
            name: cfg.name.clone(),
            command: cfg.command.clone(),
            args: cfg.args.clone(),
            env: cfg.env.clone(),
            tool_timeout_secs: cfg.tool_timeout_secs,
            transport,
            url: cfg.url.clone(),
            headers: cfg.headers.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_is_not_connected() {
        let mgr = McpManager::new();
        assert!(!mgr.is_connected().await);
        assert_eq!(mgr.tool_count().await, 0);
    }

    #[tokio::test]
    async fn connect_empty_is_connected_but_empty() {
        let mgr = McpManager::new();
        mgr.connect(&[]).await.expect("connect with empty config must succeed");
        assert!(mgr.is_connected().await);
        assert_eq!(mgr.server_count().await, 0);
        assert!(mgr.tools().await.is_empty());
    }
}
