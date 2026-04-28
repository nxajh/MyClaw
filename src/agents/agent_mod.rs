//! Orchestrator — Application Service for message routing and session lifecycle.

#[allow(clippy::module_inception)]
mod orchestrator;

pub use orchestrator::{Orchestrator, OrchestratorParts};
