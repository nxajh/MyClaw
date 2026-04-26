//! Memory trait and core types.
//!
//! This module defines the `Memory` trait (from `zeroclaw-api/memory_traits`)
//! along with the core data types: `MemoryEntry`, `MemoryCategory`, `ExportFilter`.
//!
//! The trait is re-exported here so that the Domain Layer "owns" the Memory concept,
//! per the architecture definition in Section 1.7.
//!
//! Backends are implemented in `infrastructure/zeroclaw-memory-storage/`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A single memory entry.
#[derive(Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub timestamp: String,
    pub session_id: Option<String>,
    pub score: Option<f64>,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Importance score (0.0–1.0) for prioritized retrieval.
    #[serde(default)]
    pub importance: Option<f64>,
    /// If this entry was superseded by a newer conflicting entry.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

fn default_namespace() -> String {
    "default".into()
}

impl fmt::Debug for MemoryEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryEntry")
            .field("id", &self.id)
            .field("key", &self.key)
            .field("content", &self.content)
            .field("category", &self.category)
            .field("timestamp", &self.timestamp)
            .field("score", &self.score)
            .field("namespace", &self.namespace)
            .field("importance", &self.importance)
            .finish_non_exhaustive()
    }
}

/// Memory categories for organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCategory {
    /// Long-term facts, preferences, decisions.
    Core,
    /// Daily session logs.
    Daily,
    /// Conversation context.
    Conversation,
    /// User-defined custom category.
    Custom(String),
}

impl fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core => write!(f, "core"),
            Self::Daily => write!(f, "daily"),
            Self::Conversation => write!(f, "conversation"),
            Self::Custom(name) => write!(f, "{name}"),
        }
    }
}

impl Serialize for MemoryCategory {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for MemoryCategory {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "core" => Self::Core,
            "daily" => Self::Daily,
            "conversation" => Self::Conversation,
            _ => Self::Custom(s),
        })
    }
}

/// Filter criteria for bulk memory export (GDPR Art. 20 data portability).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportFilter {
    pub namespace: Option<String>,
    pub session_id: Option<String>,
    pub category: Option<MemoryCategory>,
    /// RFC 3339 lower bound (inclusive) on created_at.
    pub since: Option<String>,
    /// RFC 3339 upper bound (inclusive) on created_at.
    pub until: Option<String>,
}

/// A single message in a conversation trace for procedural memory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProceduralMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Core memory trait — implement for any persistence backend.
#[async_trait]
pub trait Memory: Send + Sync {
    /// Backend name.
    fn name(&self) -> &str;

    /// Store a memory entry.
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()>;

    /// Recall memories matching a query, optionally scoped to session and time range.
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>>;

    /// Get a specific memory by key.
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>>;

    /// List all memory keys, optionally filtered.
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>>;

    /// Remove a memory by key.
    async fn forget(&self, key: &str) -> anyhow::Result<bool>;

    /// Bulk-remove all memories in a namespace.
    async fn purge_namespace(&self, namespace: &str) -> anyhow::Result<usize> {
        anyhow::bail!("purge_namespace not supported by this memory backend")
    }

    /// Bulk-remove all memories in a session.
    async fn purge_session(&self, session_id: &str) -> anyhow::Result<usize> {
        anyhow::bail!("purge_session not supported by this memory backend")
    }

    /// Count total memories.
    async fn count(&self) -> anyhow::Result<usize>;

    /// Health check.
    async fn health_check(&self) -> bool;

    /// Store procedural memory (default: no-op).
    async fn store_procedural(
        &self,
        _messages: &[ProceduralMessage],
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Recall with namespace filter.
    async fn recall_namespaced(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self
            .recall(query, limit * 2, session_id, since, until)
            .await?;
        Ok(entries
            .into_iter()
            .filter(|e| e.namespace == namespace)
            .take(limit)
            .collect())
    }

    /// Store with namespace and importance (default: delegates to store).
    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        _namespace: Option<&str>,
        _importance: Option<f64>,
    ) -> anyhow::Result<()> {
        self.store(key, content, category, session_id).await
    }
}
