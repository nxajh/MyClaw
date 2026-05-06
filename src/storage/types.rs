//! Local type stubs replacing external zeroclaw-config and zeroclaw-api dependencies.
//!
//! These are simplified, self-contained definitions sufficient for compiling
//! the memory domain layer without external zeroclaw crate dependencies.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── SearchMode ────────────────────────────────────────────────────────────────

/// Search strategy for memory queries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchMode {
    /// Pure keyword search (FTS5 BM25)
    Bm25,
    /// Pure vector/semantic search
    Embedding,
    /// Weighted combination of keyword + vector (default)
    #[default]
    Hybrid,
}

// ── MemoryPolicyConfig ───────────────────────────────────────────────────────

/// Memory policy configuration — enforces namespace quotas, category limits,
/// read-only namespaces, and per-category retention.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryPolicyConfig {
    /// Maximum entries per namespace (0 = unlimited).
    #[serde(default)]
    pub max_entries_per_namespace: usize,
    /// Maximum entries per category (0 = unlimited).
    #[serde(default)]
    pub max_entries_per_category: usize,
    /// Retention days by category (overrides global).
    /// Keys: "core", "daily", "conversation".
    #[serde(default)]
    pub retention_days_by_category: HashMap<String, u32>,
    /// Namespaces that are read-only (writes are rejected).
    #[serde(default)]
    pub read_only_namespaces: Vec<String>,
}

// ── MemoryConfig ─────────────────────────────────────────────────────────────

/// Memory backend configuration — simplified from zeroclaw-config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// "sqlite" | "lucid" | "qdrant" | "markdown" | "none"
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Auto-save user input to memory.
    #[serde(default)]
    pub auto_save: bool,
    /// Run hygiene (archiving + retention cleanup).
    #[serde(default = "default_true")]
    pub hygiene_enabled: bool,
    /// Search strategy.
    #[serde(default)]
    pub search_mode: SearchMode,
    /// Default namespace.
    #[serde(default = "default_namespace")]
    pub default_namespace: String,
    /// Policy configuration.
    #[serde(default)]
    pub policy: MemoryPolicyConfig,
}

fn default_backend() -> String {
    "none".into()
}
fn default_true() -> bool {
    true
}
fn default_namespace() -> String {
    "default".into()
}

// ── HTTP client helper ────────────────────────────────────────────────────────

/// Build a reqwest HTTP client, optionally configured for proxying.
/// This is a simplified replacement for zeroclaw-config's build_runtime_proxy_client.
pub fn build_proxy_client(_service_key: &str) -> reqwest::Client {
    reqwest::Client::new()
}

// ── Provider trait (simplified) ──────────────────────────────────────────────

/// Simple LLM provider trait for memory consolidation.
/// Simplified from zeroclaw_api::provider::Provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Simple one-shot chat.
    async fn simple_chat(
        &self,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String>;

    /// One-shot chat with optional system prompt.
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String>;
}
