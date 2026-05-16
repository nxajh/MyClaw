use std::sync::Arc;

use crate::providers::Capability;

use super::AgentLoop;

impl AgentLoop {
    /// Attach pending image URLs and base64 data to the last user message if model supports it.
    pub(crate) fn attach_images_if_supported(&self, messages: &mut [crate::providers::ChatMessage], model_id: &str) {
        if !self.request_builder.has_images() {
            return;
        }

        let supports_image = self
            .registry
            .get_chat_model_config(model_id)
            .map(|cfg| cfg.supports_image_input())
            .unwrap_or(false);

        if !supports_image {
            tracing::debug!(
                model = %model_id,
                "model does not support image input, ignoring images"
            );
            return;
        }

        if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
            if let Some(urls) = self.request_builder.image_urls() {
                for url in urls {
                    last_user.parts.push(crate::providers::ContentPart::ImageUrl {
                        url: url.clone(),
                        detail: crate::providers::ImageDetail::Auto,
                    });
                }
            }
            if let Some(b64s) = self.request_builder.image_b64() {
                for b64 in b64s {
                    last_user.parts.push(crate::providers::ContentPart::ImageB64 {
                        b64_json: b64.clone(),
                        media_type: None,
                        detail: crate::providers::ImageDetail::Auto,
                    });
                }
            }
            let total = self.request_builder.image_urls().map_or(0, |v| v.len())
                + self.request_builder.image_b64().map_or(0, |v| v.len());
            tracing::info!("attached {} image(s) to user message", total);
        }
    }

    /// Select a vision-capable model from the fallback chain.
    /// Falls back to the default chat provider if no vision model is found.
    pub(crate) async fn select_vision_provider(&self) -> anyhow::Result<(Arc<dyn crate::providers::ChatProvider>, String)> {
        // Walk the routing list directly (not the fallback chain, which collapses
        // all providers into a single FallbackChatProvider entry).
        // We need the original model list to find vision-capable models.
        let routing_models = self.registry.get_chat_routing_models();
        for model_id in &routing_models {
            if let Ok(cfg) = self.registry.get_chat_model_config(model_id) {
                if cfg.supports_image_input() {
                    // Try direct model provider first (bypasses fallback chain).
                    if let Some((provider, id)) = self.registry.get_chat_provider_by_model(model_id) {
                        tracing::info!(model = %id, "selected vision-capable model for image input (direct)");
                        return Ok((provider, id));
                    }
                    // Fall back to the primary provider (FallbackChatProvider).
                    tracing::info!(model = %model_id, "selected vision-capable model for image input (fallback)");
                    let (provider, _) = self.registry.get_chat_provider(Capability::Chat)?;
                    return Ok((provider, model_id.clone()));
                }
            }
        }

        // No vision model found — warn and fall back to default.
        tracing::warn!("no vision-capable model found, images may be ignored");
        self.registry.get_chat_provider(Capability::Chat)
    }
}
