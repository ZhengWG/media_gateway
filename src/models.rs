use std::collections::HashMap;

use crate::config::{AppConfig, ModelProfile};
use crate::error::GatewayError;

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    default_profile: ModelProfile,
    profiles: HashMap<String, ModelProfile>,
}

impl ModelRegistry {
    pub fn from_config(cfg: &AppConfig) -> Self {
        Self {
            default_profile: cfg.default_profile.clone(),
            profiles: cfg.model_profiles.clone(),
        }
    }

    pub fn is_ready(&self) -> bool {
        // First version is always "ready" once config is loaded.
        true
    }

    pub fn resolve(&self, model_id: &str) -> Result<ModelProfile, GatewayError> {
        if model_id.trim().is_empty() {
            return Err(GatewayError::BadRequest("missing model".to_string()));
        }

        Ok(self
            .profiles
            .get(model_id)
            .cloned()
            .unwrap_or_else(|| self.default_profile.clone()))
    }
}
