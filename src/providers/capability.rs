//! Re-export shim — canonical definitions live in `crate::api::capability`
//! (L0 contract layer, moved in #151 Phase 3c).

pub use crate::api::capability::{
    BasicModelConfig, BasicPricing, Capability, ChatModelConfig, ChatPricing, EmbeddingModelConfig,
    EmbeddingPricing, Modality, RotationStrategy,
};
