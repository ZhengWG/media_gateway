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
        let normalized = model_id.trim();
        if normalized.is_empty() {
            return Err(GatewayError::BadRequest("missing model".to_string()));
        }
        if !is_supported_model_family(normalized) {
            return Err(GatewayError::BadRequest(format!(
                "unsupported model `{normalized}`: only Qwen/Kimi model families are supported"
            )));
        }

        Ok(self
            .profiles
            .get(normalized)
            .cloned()
            .unwrap_or_else(|| self.default_profile.clone()))
    }
}

fn is_supported_model_family(model_id: &str) -> bool {
    let m = model_id.to_ascii_lowercase();
    m.contains("qwen") || m.contains("kimi")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry() -> ModelRegistry {
        let cfg = AppConfig {
            bind_addr: "127.0.0.1:0".parse().expect("addr"),
            run_mode: crate::config::RunMode::PreprocessOnly,
            request_timeout: std::time::Duration::from_secs(1),
            fetch_timeout: std::time::Duration::from_secs(1),
            max_request_bytes: 1024,
            max_inflight: 1,
            allow_private_network: true,
            allowed_hosts: Default::default(),
            hf_processor_mode: crate::config::HfProcessorMode::Disabled,
            hf_python_bin: "python3".to_string(),
            hf_sidecar_script: "scripts/hf_processor_sidecar.py".to_string(),
            hf_sidecar_command_template: "{python_bin} {script_path}".to_string(),
            hf_sidecar_timeout: std::time::Duration::from_secs(1),
            inject_processor_output: false,
            default_profile: ModelProfile::default(),
            model_profiles: Default::default(),
        };
        ModelRegistry::from_config(&cfg)
    }

    #[test]
    fn resolve_accepts_qwen_and_kimi() {
        let registry = test_registry();
        assert!(registry.resolve("Qwen/Qwen2.5-VL-3B-Instruct").is_ok());
        assert!(registry.resolve("moonshotai/Kimi-VL-A3B-Instruct").is_ok());
    }

    #[test]
    fn resolve_rejects_unsupported_model_family() {
        let registry = test_registry();
        let err = registry
            .resolve("meta-llama/Llama-3.2-11B-Vision")
            .expect_err("reject");
        assert!(format!("{err}").contains("only Qwen/Kimi"));
    }
}
