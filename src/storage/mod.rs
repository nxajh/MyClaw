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
pub mod json_file;

pub use memory::{Memory, MemoryCategory, MemoryEntry, ExportFilter, ProceduralMessage};
pub use shared::SharedMemory;
pub use private::PrivateMemory;
pub use types::{SearchMode, MemoryConfig, MemoryPolicyConfig, Provider, build_proxy_client};
pub use session::{SessionBackend, SessionInfo, SummaryRecord, ChatMessage};
pub use json_file::JsonFileBackend;

/// Per-session subdirectory (under `{sessions_root}/{session_id}/`) holding the
/// auxiliary-model image descriptions written by `PersistentDescriptionCache`,
/// a sibling of the session's `blobs/`. Shared by the writer
/// (`agents::modality_adapter`) and the GC sweep (`json_file`) so the two never
/// drift onto different paths.
pub const SESSION_DESCRIPTIONS_DIR: &str = "descriptions";
