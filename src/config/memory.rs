//! Memory configuration.

use serde::{Deserialize, Serialize};

// ── MemoryStorage ─────────────────────────────────────────────────────────────

/// Storage backend for memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStorage {
    #[default]
    Sqlite,
    InMemory,
}

// ── MemoryConfig ──────────────────────────────────────────────────────────────

/// Memory subsystem configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Storage backend.
    #[serde(default)]
    pub storage: MemoryStorage,

    /// Path to the SQLite database file (relative to workspace_dir if not absolute).
    #[serde(default = "default_db_path")]
    pub db_path: String,

    /// Enable embedding-based search.
    #[serde(default)]
    pub embedding_enabled: bool,

    /// Embedding model route (e.g. "jina-embeddings-v3").
    pub embedding_model: Option<String>,

    /// Maximum number of memories to return in a search.
    #[serde(default = "default_search_limit")]
    pub search_limit: usize,

    /// Maximum age in days for daily memories before consolidation.
    #[serde(default = "default_consolidation_age_days")]
    pub consolidation_age_days: u32,
}

fn default_db_path() -> String {
    "memory.db".to_string()
}

fn default_search_limit() -> usize {
    10
}

fn default_consolidation_age_days() -> u32 {
    30
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            storage: MemoryStorage::Sqlite,
            db_path: default_db_path(),
            embedding_enabled: false,
            embedding_model: None,
            search_limit: default_search_limit(),
            consolidation_age_days: default_consolidation_age_days(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_memory_config() {
        let config = MemoryConfig::default();
        assert_eq!(config.storage, MemoryStorage::Sqlite);
        assert_eq!(config.db_path, "memory.db");
        assert!(!config.embedding_enabled);
        assert_eq!(config.search_limit, 10);
    }
}
