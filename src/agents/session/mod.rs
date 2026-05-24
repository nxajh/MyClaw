//! Session management — session lifecycle, overrides, recovery, and persistence.

pub mod backend;
pub mod manager;
pub mod queue;
pub mod recovery;
pub mod session_override;
pub mod types;

// Re-export all public items so existing code can reference them via
// `crate::agents::session_manager::*` (through agents/mod.rs re-exports).
pub use backend::{BackendPersistHook, InMemoryBackend, PersistHook};
pub use manager::{SessionManager, SessionNotOwned};
pub use queue::{enqueue_message, process_all_queues, process_queue};
pub use recovery::{detect_incomplete_turn, identify_breakpoint, BreakpointItem};
pub use session_override::{sanitize_history, SessionOverride};
pub use types::{Session, SummaryMetadata, TokenTracker, estimate_tokens, estimate_message_tokens};
