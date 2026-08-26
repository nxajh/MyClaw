//! Memory and session domain — trait, types, and decorators.
//!
//! ## Public API
//!
//! - [`Memory`] trait — implement to add a new storage backend
//! - [`MemoryEntry`], [`MemoryCategory`], [`ExportFilter`], [`ProceduralMessage`] — data types
//! - [`SharedMemory`] — namespace-isolated decorator for cross-session memory
//! - [`PrivateMemory`] — namespace-isolated decorator for per-session memory
//! - [`SessionBackend`] trait — session persistence

pub mod json_file;
mod memory;
mod private;
mod session;
mod shared;
mod types;

pub use json_file::JsonFileBackend;
pub use memory::{ExportFilter, Memory, MemoryCategory, MemoryEntry, ProceduralMessage};
pub use private::PrivateMemory;
pub use session::{
    ChatMessage, DelegationCheckpoint, SavedSessionFile, SessionBackend, SessionInfo,
    SummaryRecord, session_file_name, write_session_file,
};
pub use shared::SharedMemory;
pub use types::{MemoryConfig, MemoryPolicyConfig, Provider, SearchMode, build_proxy_client};
