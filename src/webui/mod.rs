//! webui — WebSocket server backend for TUI/web clients (L6 composition root layer).

#[cfg(feature = "client")]
pub mod client;

#[cfg(feature = "client")]
pub use client::ClientChannel;
