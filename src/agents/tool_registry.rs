//! Tool registry — pure tool registration and dispatch.
//!
//! Separated from SkillManager because tools (what the model can call)
//! and skills (behavioral guidance injected into prompts) change at
//! different frequencies and for different reasons.

use std::collections::HashMap;
use std::sync::Arc;

use crate::providers::capability_tool::Tool;

/// Pure tool registry — maps tool name to tool instance.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool. Panics if a tool with the same name already exists.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        let existing = self.tools.insert(name.clone(), tool);
        if existing.is_some() {
            panic!("Tool '{}' is already registered", name);
        }
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(Arc::clone)
    }

    /// Get all registered tools.
    pub fn all_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.values().map(Arc::clone).collect()
    }

    /// Number of registered tools.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Tool names, sorted for deterministic output.
    pub fn tool_names_sorted(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().map(|s| s.to_string()).collect();
        names.sort();
        names
    }
}
