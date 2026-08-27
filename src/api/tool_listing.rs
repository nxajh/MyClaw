//! tool_listing — L0 facade over `agents::ToolRegistry`.
//!
//! #151 Phase 8+: `tool_search` (L3) only reads the registry; the
//! register/get mutation surface stays behind the agents layer. The
//! concrete `ToolRegistry` implements this trait in the agents layer; the
//! daemon keeps passing `Arc<ToolRegistry>` unchanged (generic
//! constructor coerces to `Arc<dyn ToolListing>`).

use std::sync::Arc;

use crate::api::tool::Tool;

/// Read-only view of the tool registry for search/listing.
pub trait ToolListing: Send + Sync {
    fn all_tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// Static, immutable listing — test double and minimal-assembly scenarios.
pub struct StaticToolListing {
    tools: Vec<Arc<dyn Tool>>,
}

impl StaticToolListing {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }
}

impl ToolListing for StaticToolListing {
    fn all_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}
