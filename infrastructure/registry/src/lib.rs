//! registry — ServiceRegistry, capability routing center.

pub mod capability;
pub mod registry;
pub mod routing;

pub use capability::Capability;
pub use registry::{ModelConfig, ProviderConfig, Registry};
pub use routing::{RouteEntry, RoutingConfig, RoutingStrategy};
