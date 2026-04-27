//! Orchestrator crate — Composition Root for MyClaw.
//!
//! This crate is a thin binary entry point (Composition Root in DDD).
//! It assembles all Infrastructure components and injects them into
//! the Application layer. It does NOT contain business logic.

pub mod daemon;
