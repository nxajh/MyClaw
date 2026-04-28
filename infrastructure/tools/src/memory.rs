//! Memory tools with shared in-memory store.
//!
//! MemoryStore is a simple key-value store with categories.
//! Thread-safe via Arc<RwLock>. Can be replaced with a persistent backend later.

use async_trait::async_trait;
use chrono::Utc;
use myclaw_capability::tool::{Tool, ToolResult};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

// ── Shared Memory Store ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub content: String,
    pub category: String,
    pub created_at: String,
}

/// Thread-safe in-memory store. Shared across all memory tools.
#[derive(Clone)]
pub struct MemoryStore {
    entries: Arc<RwLock<HashMap<String, MemoryEntry>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn store(&self, key: &str, content: &str, category: &str) {
        let entry = MemoryEntry {
            key: key.to_string(),
            content: content.to_string(),
            category: category.to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        self.entries.write().insert(key.to_string(), entry);
    }

    pub fn recall_by_key(&self, key: &str) -> Option<MemoryEntry> {
        self.entries.read().get(key).cloned()
    }

    /// Simple keyword search: returns entries where query appears in key or content.
    pub fn recall_by_query(&self, query: &str, category: Option<&str>, limit: usize) -> Vec<MemoryEntry> {
        let query_lower = query.to_lowercase();
        let guard = self.entries.read();
        let mut results: Vec<MemoryEntry> = guard
            .values()
            .filter(|e| {
                // Category filter.
                if let Some(cat) = category {
                    if e.category != cat {
                        return false;
                    }
                }
                // Keyword match on key or content.
                e.key.to_lowercase().contains(&query_lower)
                    || e.content.to_lowercase().contains(&query_lower)
            })
            .cloned()
            .collect();

        // Sort by created_at descending (most recent first).
        results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        results.truncate(limit);
        results
    }

    pub fn forget(&self, key: &str) -> bool {
        self.entries.write().remove(key).is_some()
    }

    pub fn list_all(&self) -> Vec<MemoryEntry> {
        self.entries.read().values().cloned().collect()
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── MemoryStoreTool ──────────────────────────────────────────────────────────

pub struct MemoryStoreTool {
    store: MemoryStore,
}

impl MemoryStoreTool {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store a fact, preference, or note in memory for later recall."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Unique key for this memory (e.g. 'user_lang', 'project_stack')."
                },
                "content": {
                    "type": "string",
                    "description": "The information to remember."
                },
                "category": {
                    "type": "string",
                    "description": "Category: 'core' (permanent), 'daily' (session), 'conversation' (chat), or custom. Default: 'daily'."
                }
            },
            "required": ["key", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args["key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'key' is required"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'content' is required"))?;
        let category = args["category"].as_str().unwrap_or("daily");

        self.store.store(key, content, category);

        Ok(ToolResult {
            success: true,
            output: format!("stored memory '{}' [{}]", key, category),
            error: None,
        })
    }
}

// ── MemoryRecallTool ─────────────────────────────────────────────────────────

pub struct MemoryRecallTool {
    store: MemoryStore,
}

impl MemoryRecallTool {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search memory for relevant facts, preferences, or context. Supports keyword search."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or phrase to search for."
                },
                "category": {
                    "type": "string",
                    "description": "Filter by category."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default 5)."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args["query"].as_str().unwrap_or("");
        let category = args["category"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(5) as usize;

        // If query is empty, list all (up to limit).
        let results = if query.is_empty() {
            let all = self.store.list_all();
            let mut all = all;
            all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            all.truncate(limit);
            all
        } else {
            self.store.recall_by_query(query, category, limit)
        };

        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "no memories found".to_string(),
                error: None,
            });
        }

        let output = results.iter()
            .map(|e| format!("[{}] {} ({}): {}", e.created_at, e.category, e.key, e.content))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

// ── MemoryForgetTool ─────────────────────────────────────────────────────────

pub struct MemoryForgetTool {
    store: MemoryStore,
}

impl MemoryForgetTool {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Delete a memory entry by key."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key of the memory to delete."
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args["key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'key' is required"))?;

        let found = self.store.forget(key);

        Ok(ToolResult {
            success: found,
            output: if found {
                format!("forgot memory '{}'", key)
            } else {
                format!("memory '{}' not found", key)
            },
            error: if found { None } else { Some(format!("key '{}' not found", key)) },
        })
    }
}
