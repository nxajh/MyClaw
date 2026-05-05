//! Memory and session domain — trait, types, and decorators.
//!
//! ## Public API
//!
//! - [`Memory`] trait — implement to add a new storage backend
//! - [`MemoryEntry`], [`MemoryCategory`], [`ExportFilter`], [`ProceduralMessage`] — data types
//! - [`SharedMemory`] — namespace-isolated decorator for cross-session memory
//! - [`PrivateMemory`] — namespace-isolated decorator for per-session memory
//! - [`SessionBackend`] trait — session persistence

mod memory;
mod shared;
mod private;
mod types;
mod session;

pub use memory::{Memory, MemoryCategory, MemoryEntry, ExportFilter, ProceduralMessage};
pub use shared::SharedMemory;
pub use private::PrivateMemory;
pub use types::{SearchMode, MemoryConfig, MemoryPolicyConfig, Provider, build_proxy_client};
pub use session::{SessionBackend, SessionInfo, SummaryRecord, ChatMessage};
