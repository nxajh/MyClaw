//! Shared Memory — cross-session, persistent memory namespace.
//!
//! Namespace: `"shared"`
//!
//! Used for facts, preferences, decisions, and long-term knowledge that should
//! persist across all sessions. Backed by any `Memory` implementation.

use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;

use crate::{Memory, MemoryCategory, MemoryEntry};

/// Shared memory namespace decorator.
///
/// Wraps any `Memory` backend and enforces `namespace = "shared"` on all operations.
/// All stored entries are accessible from any session.
pub struct SharedMemory {
    inner: Arc<dyn Memory>,
}

impl SharedMemory {
    /// Wrap an existing memory backend with shared-memory semantics.
    pub fn new(inner: Arc<dyn Memory>) -> Self {
        Self { inner }
    }

    /// Backend name.
    pub fn name(&self) -> &str {
        self.inner.name()
    }
}

impl fmt::Debug for SharedMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedMemory")
            .field("inner", &self.inner.name())
            .finish()
    }
}

#[async_trait]
impl Memory for SharedMemory {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner
            .store_with_metadata(key, content, category, session_id, Some("shared"), None)
            .await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner
            .recall_namespaced("shared", query, limit, session_id, since, until)
            .await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let entry = self.inner.get(key).await?;
        Ok(entry.filter(|e| e.namespace == "shared"))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self.inner.list(category, session_id).await?;
        Ok(entries
            .into_iter()
            .filter(|e| e.namespace == "shared")
            .collect())
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let existed = self.get(key).await?.is_some();
        if existed {
            self.inner.forget(key).await?;
        }
        Ok(existed)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let all = self.inner.list(None, None).await?;
        Ok(all.into_iter().filter(|e| e.namespace == "shared").count())
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}
