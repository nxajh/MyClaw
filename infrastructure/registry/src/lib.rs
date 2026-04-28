//! registry — ServiceRegistry, capability routing center.

pub mod capability;
pub mod registry;
pub mod routing;

pub use registry::{ModelConfig, ProviderConfig, Registry};
pub use routing::{RouteEntry, RoutingConfig, RoutingStrategy};

// Re-export ServiceRegistry trait so consumers don't need the capability crate directly.
// Use `::capability` to refer to the external crate, not our local `capability` module.
pub use ::myclaw_capability::service_registry::ServiceRegistry;
