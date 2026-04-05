use std::collections::{HashMap, HashSet};
use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub target_image_edge: u32,
    pub max_media_bytes: usize,
}

impl Default for ModelProfile {
    fn default() -> Self {
        Self {
            target_image_edge: 1024,
            max_media_bytes: 20 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum RunMode {
    Proxy,
    PreprocessOnly,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::PreprocessOnly => "preprocess_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HfProcessorMode {
    Disabled,
    PythonSidecar,
}

impl HfProcessorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::PythonSidecar => "python_sidecar",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub upstream_url: Option<String>,
    pub run_mode: RunMode,
    pub request_timeout: Duration,
    pub fetch_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_inflight: usize,
    pub allow_private_network: bool,
    pub allowed_hosts: HashSet<String>,
    pub hf_processor_mode: HfProcessorMode,
    pub hf_python_bin: String,
    pub hf_sidecar_script: String,
    pub default_profile: ModelProfile,
    pub model_profiles: HashMap<String, ModelProfile>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, String> {
        let bind_addr = env_or("BIND_ADDR", "0.0.0.0:8080")
            .parse::<SocketAddr>()
            .map_err(|e| format!("invalid BIND_ADDR: {e}"))?;
        let upstream_url = env::var("UPSTREAM_URL")
            .ok()
            .filter(|v| !v.trim().is_empty());

        let run_mode = match env_or("RUN_MODE", "auto").to_lowercase().as_str() {
            "proxy" => RunMode::Proxy,
            "preprocess_only" => RunMode::PreprocessOnly,
            "auto" => {
                if upstream_url.is_some() {
                    RunMode::Proxy
                } else {
                    RunMode::PreprocessOnly
                }
            }
            other => {
                return Err(format!(
                    "invalid RUN_MODE={other}, expected proxy|preprocess_only|auto"
                ))
            }
        };
        if matches!(run_mode, RunMode::Proxy) && upstream_url.is_none() {
            return Err("RUN_MODE=proxy requires UPSTREAM_URL".to_string());
        }

        let request_timeout_ms = env_parse("REQUEST_TIMEOUT_MS", 30_000_u64)?;
        let fetch_timeout_ms = env_parse("FETCH_TIMEOUT_MS", 15_000_u64)?;
        let max_request_bytes = env_parse("MAX_REQUEST_BYTES", 16 * 1024 * 1024_usize)?;
        let max_inflight = env_parse("MAX_INFLIGHT", 64_usize)?;
        let allow_private_network = env_parse("ALLOW_PRIVATE_NETWORK", false)?;
        let allowed_hosts = parse_csv_set("ALLOWED_HOSTS");
        let hf_processor_mode = match env_or("HF_PROCESSOR_MODE", "disabled")
            .to_ascii_lowercase()
            .as_str()
        {
            "disabled" => HfProcessorMode::Disabled,
            "python_sidecar" => HfProcessorMode::PythonSidecar,
            other => {
                return Err(format!(
                    "invalid HF_PROCESSOR_MODE={other}, expected disabled|python_sidecar"
                ))
            }
        };
        let hf_python_bin = env_or("HF_PYTHON_BIN", "python3");
        let hf_sidecar_script = env_or("HF_SIDECAR_SCRIPT", "scripts/hf_processor_sidecar.py");

        let default_profile = ModelProfile {
            target_image_edge: env_parse("DEFAULT_TARGET_IMAGE_EDGE", 1024_u32)?,
            max_media_bytes: env_parse("DEFAULT_MAX_MEDIA_BYTES", 20 * 1024 * 1024_usize)?,
        };
        let model_profiles = parse_model_profiles();

        Ok(Self {
            bind_addr,
            upstream_url,
            run_mode,
            request_timeout: Duration::from_millis(request_timeout_ms),
            fetch_timeout: Duration::from_millis(fetch_timeout_ms),
            max_request_bytes,
            max_inflight,
            allow_private_network,
            allowed_hosts,
            hf_processor_mode,
            hf_python_bin,
            hf_sidecar_script,
            default_profile,
            model_profiles,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T>(key: &str, default: T) -> Result<T, String>
where
    T: std::str::FromStr,
    <T as std::str::FromStr>::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(raw) => raw
            .parse::<T>()
            .map_err(|e| format!("failed to parse {key}={raw}: {e}")),
        Err(_) => Ok(default),
    }
}

fn parse_csv_set(key: &str) -> HashSet<String> {
    env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect::<HashSet<_>>()
}

fn parse_model_profiles() -> HashMap<String, ModelProfile> {
    // MODEL_PROFILES_JSON example:
    // {"qwen2-vl":{"target_image_edge":1344,"max_media_bytes":31457280}}
    match env::var("MODEL_PROFILES_JSON") {
        Ok(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw).unwrap_or_default(),
        _ => HashMap::new(),
    }
}
