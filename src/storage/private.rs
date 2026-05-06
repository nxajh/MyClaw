//! Private Memory — per-session, session-scoped memory namespace.
//!
//! Namespace: `"private_{session_id}"`
//!
//! Used for session-specific information extracted from compressed history.
//! Entries are stored under a per-session namespace and are cleared when the
//! session ends (caller is responsible for purging the namespace).

use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;

use crate::storage::{Memory, MemoryCategory, MemoryEntry};

/// Private memory namespace decorator.
///
/// Wraps any `Memory` backend and enforces `namespace = "private_{session_id}"`
/// on all operations. Each session gets its own isolated namespace.
pub struct PrivateMemory {
    inner: Arc<dyn Memory>,
    session_id: String,
}

impl fmt::Debug for PrivateMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateMemory")
            .field("inner", &self.inner.name())
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl PrivateMemory {
    /// Wrap an existing memory backend with per-session private-memory semantics.
    pub fn new(inner: Arc<dyn Memory>, session_id: String) -> Self {
        Self { inner, session_id }
    }

    /// The session ID this private memory is scoped to.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn namespace(&self) -> String {
        format!("private_{}", self.session_id)
    }
}

#[async_trait]
impl Memory for PrivateMemory {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // PrivateMemory is inherently per-session; ignore passed session_id.
        self.inner
            .store_with_metadata(
                key,
                content,
                category,
                Some(&self.session_id),
                Some(&self.namespace()),
                None,
            )
            .await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner
            .recall_namespaced(&self.namespace(), query, limit, Some(&self.session_id), since, until)
            .await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let entry = self.inner.get(key).await?;
        Ok(entry.filter(|e| e.namespace == self.namespace()))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self
            .inner
            .list(category, Some(&self.session_id))
            .await?;
        let ns = self.namespace();
        Ok(entries.into_iter().filter(|e| e.namespace == ns).collect())
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let existed = self.get(key).await?.is_some();
        if existed {
            self.inner.forget(key).await?;
        }
        Ok(existed)
    }

    async fn purge_session(&self, _session_id: &str) -> anyhow::Result<usize> {
        // Purging our own namespace only.
        let entries = self.list(None, None).await?;
        let ns = self.namespace();
        let mut count = 0;
        for entry in entries {
            if entry.namespace == ns {
                self.inner.forget(&entry.key).await?;
                count += 1;
            }
        }
        Ok(count)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let all = self.inner.list(None, Some(&self.session_id)).await?;
        let ns = self.namespace();
        Ok(all.into_iter().filter(|e| e.namespace == ns).count())
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}
