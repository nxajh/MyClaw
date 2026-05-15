//! Protocol-specific message rendering and client implementations.
//!
//! Each subdirectory corresponds to a wire protocol (OpenAI, Anthropic, etc.).
//! The goal is to share as much code as possible between providers that speak
//! the same protocol, while keeping vendor-specific quirks in profiles.

pub mod openai;
pub mod anthropic;