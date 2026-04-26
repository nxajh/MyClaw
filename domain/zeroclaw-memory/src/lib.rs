//! Memory domain — trait, types, and shared/private memory decorators.
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
//! Backend implementations live in `infrastructure/zeroclaw-memory-storage/`:
//! - `SqliteMemory` — SQLite with vector + BM25 hybrid search
//! - `QdrantMemory` — Qdrant vector DB backend
//! - `LucidMemory` — Lucid CLI bridge
//! - `NoneMemory` — no-op backend

// Re-export the Memory trait and types from zeroclaw-api.
// These types are the authority; domain/zeroclaw-memory is their canonical home
// in the new layered architecture.
pub use zeroclaw_api::memory_traits::{Memory, MemoryCategory, MemoryEntry, ExportFilter, ProceduralMessage};

mod shared;
mod private;

pub use shared::SharedMemory;
pub use private::PrivateMemory;
