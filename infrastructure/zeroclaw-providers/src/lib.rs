//! zeroclaw-providers — Infrastructure layer for model inference backends.
//!
//! This crate hosts concrete provider implementations and the factory.
//! Re-exports from the existing `zeroclaw-providers` crate during migration.
//!
//! ## Architecture
//!
//! ```text
//! application/zeroclaw-runtime
//!        |
//!        v
//! infrastructure/zeroclaw-providers   ← Factory + all provider implementations
//!        |
//!        v
//! interface/zeroclaw-channels
//! ```

// Re-export the full provider subsystem from the existing crate.
// During migration, this establishes the infrastructure boundary.
pub use zeroclaw_providers::*;