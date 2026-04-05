mod app;
mod config;
mod error;
mod hf_sidecar;
mod media;
mod models;
mod pipeline;

use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

use crate::app::{build_router, AppState};
use crate::config::{AppConfig, HfProcessorMode};
use crate::hf_sidecar::HfSidecarClient;
use crate::models::ModelRegistry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "media_gateway=info,tower_http=info".to_string()),
        )
        .init();

    let config = AppConfig::from_env()?;
    let registry = ModelRegistry::from_config(&config);
    let metrics_handle = Arc::new(PrometheusBuilder::new().install_recorder()?);
    let http_client = reqwest::Client::builder()
        .connect_timeout(config.fetch_timeout)
        .timeout(config.request_timeout)
        .build()?;
    let hf_sidecar = if config.hf_processor_mode == HfProcessorMode::PythonSidecar {
        let command = config
            .hf_sidecar_command_template
            .replace("{python_bin}", &config.hf_python_bin)
            .replace("{script_path}", &config.hf_sidecar_script);
        Some(HfSidecarClient::new(command, config.hf_sidecar_timeout))
    } else {
        None
    };
    let state = AppState {
        config: config.clone(),
        registry,
        http_client,
        metrics_handle,
        hf_sidecar,
    };
    let app = build_router(state);
    let listener = TcpListener::bind(config.bind_addr).await?;

    info!("media_gateway listening on {}", config.bind_addr);
    axum::serve(listener, app).await?;
    Ok(())
}
