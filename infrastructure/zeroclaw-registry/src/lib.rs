//! zeroclaw-registry — ServiceRegistry, capability routing center.
//!
//! ## Core types
//!
//! - [`Capability`] — enum for capability queries and routing (from domain)
//! - [`RoutingConfig`], [`RouteEntry`], [`RoutingStrategy`] — routing rule types
//! - [`Registry`] — implements [`zeroclaw_capability::ServiceRegistry`]
//!
//! ## Usage
//!
//! ```rust,ignore
//! let registry = Registry::new(providers, routing);
//! registry.register_chat(Box::new(openai_provider), "gpt-4o".to_string());
//! let (chat, model_id) = registry.get_chat_provider(Capability::Chat)?;
//! ```

pub mod capability;
pub mod registry;
pub mod routing;

pub use capability::Capability;
pub use registry::{ModelConfig, ProviderConfig, Registry};
pub use routing::{RouteEntry, RoutingConfig, RoutingStrategy};