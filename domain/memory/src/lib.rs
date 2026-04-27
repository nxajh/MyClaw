//! Memory domain — trait, types, and decorators.
//!
//! ## Public API
//!
//! - [`Memory`] trait — implement to add a new storage backend
//! - [`MemoryEntry`], [`MemoryCategory`], [`ExportFilter`], [`ProceduralMessage`] — data types
//! - [`SharedMemory`] — namespace-isolated decorator for cross-session memory
//! - [`PrivateMemory`] — namespace-isolated decorator for per-session memory
//!
//! ## Architecture
//!
//! ```text
//! AgentLoop
//!   |
//!   +-- shared_memory: SharedMemory (namespace="shared")
//!   |     `-- wraps: Arc<dyn Memory>
//!   |
//!   +-- private_memory: PrivateMemory (namespace="private_{session_id}")
//!         `-- wraps: Arc<dyn Memory>
//! ```
//!
//! ## Backends
//!
//! Backend implementations live in `infrastructure/memory-storage/`:
//! - `SqliteMemory` — SQLite with vector + BM25 hybrid search
//! - `QdrantMemory` — Qdrant vector DB backend
//! - `LucidMemory` — Lucid CLI bridge
//! - `NoneMemory` — no-op backend

pub use crate::memory::{Memory, MemoryCategory, MemoryEntry, ExportFilter, ProceduralMessage};

mod memory;
mod shared;
mod private;
mod types;

pub use shared::SharedMemory;
pub use private::PrivateMemory;
pub use types::{SearchMode, MemoryConfig, MemoryPolicyConfig, Provider, build_proxy_client};
