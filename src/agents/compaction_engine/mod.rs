//! `CompactionEngine` — unified context-management facade.
//!
//! RFC v2 §三.A: collapses `CompactionPolicy` + `CompactionExecutor` into
//! a single type so `Agent.run` interacts with one touch point, not two.
//! The per-session `TokenTracker` lives on `Session.token_tracker` (not
//! here) — methods that need a token count take it as a parameter.
//!
//! Internals are private free functions split across submodules; the public
//! surface is the `CompactionEngine` impl block in `engine`.

mod engine;
mod evidence;
mod fold;
mod summarizer;

pub use engine::CompactionEngine;
pub(crate) use engine::CompactionResult;

#[cfg(test)]
mod tests;
