//! Telegram Bot API channel adapter.
//!
//! Implements the [`Channel`] trait for the Telegram Bot API.
//!
//! # Features
//!
//! - Long-poll `getUpdates` for incoming messages
//! - Send text messages via `sendRichMessage` with Markdown formatting (Bot API 10.1)
//! - Message chunking (Telegram 32 768-char rich message limit) + 429 rate-limit retry
//! - Typing indicators with circuit breaker (2-failure + 60s TTL)
//! - Allowed-user filtering + @mention / reply_to_bot detection in groups
//! - Message dedup + Thread/Topic support
//! - Ack reactions (👀 on receive) + Status reactions (🤔 thinking, ❌ error)
//! - CallbackQuery handling (button → text + answerCallbackQuery)
//! - Inbound debounce (merge rapid consecutive messages)
//! - Stall watchdog (notify when thinking for too long)
//! - Photo/image download + forward attribution

pub mod channel;
pub mod types;

pub use channel::TelegramChannel;
