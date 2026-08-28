//! websocket — WebSocket server backend for TUI/web clients (L6 composition root layer).

#[cfg(feature = "websocket")]
pub mod server;

#[cfg(feature = "websocket")]
pub use client::WebSocketChannel;
