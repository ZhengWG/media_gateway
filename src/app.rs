use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{
    routing::{get, post},
    Json, Router,
};
use metrics::{counter, gauge, histogram};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::config::RunMode;
use crate::error::GatewayError;
use crate::hf_sidecar::HfSidecarClient;
use crate::pipeline::preprocess_request;

#[derive(Clone)]
pub struct AppState {
    pub config: crate::config::AppConfig,
    pub registry: crate::models::ModelRegistry,
    pub http_client: reqwest::Client,
    pub metrics_handle: Arc<metrics_exporter_prometheus::PrometheusHandle>,
    pub hf_sidecar: Option<HfSidecarClient>,
}

pub fn build_router(state: AppState) -> Router {
    let max_inflight = state.config.max_inflight;
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/preprocess", post(preprocess_only))
        .route("/live", get(live))
        .route("/ready", get(ready))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .layer(tower::limit::ConcurrencyLimitLayer::new(max_inflight))
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn live() -> impl IntoResponse {
    (StatusCode::OK, Json(Health { status: "alive" }))
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    if state.registry.is_ready() {
        (StatusCode::OK, Json(Health { status: "ready" }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Health {
                status: "not_ready",
            }),
        )
    }
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let status = if state.registry.is_ready() {
        "ok"
    } else {
        "degraded"
    };
    (
        StatusCode::OK,
        Json(json!({
            "status": status,
            "registry_ready": state.registry.is_ready(),
            "mode": state.config.run_mode.as_str(),
        })),
    )
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics_handle.render();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

async fn preprocess_only(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<impl IntoResponse, GatewayError> {
    let begin = std::time::Instant::now();
    let request_id = request_id(&headers);
    let model_id = extract_model_id(&payload)?;

    if payload.to_string().len() > state.config.max_request_bytes {
        return Err(GatewayError::PayloadTooLarge {
            size: payload.to_string().len(),
            limit: state.config.max_request_bytes,
        });
    }
    gauge!("gateway_queue_depth").set(0.0);

    gauge!("gateway_inflight").increment(1.0);
    counter!("gateway_requests_total", "route" => "preprocess", "model_id" => model_id.clone())
        .increment(1);
    let out = preprocess_request(
        &state.config,
        &state.registry,
        &state.http_client,
        state.hf_sidecar.as_ref(),
        payload,
    )
    .await?;
    histogram!("gateway_request_seconds", "route" => "preprocess", "model_id" => model_id)
        .record(begin.elapsed().as_secs_f64());
    gauge!("gateway_inflight").decrement(1.0);

    Ok((
        StatusCode::OK,
        Json(json!({
            "request_id": request_id,
            "processed": true,
            "media_changed": out.changed_items,
            "payload": out.payload,
        })),
    ))
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, GatewayError> {
    let begin = std::time::Instant::now();
    let request_id = request_id(&headers);
    let model_id = extract_model_id(&payload)?;

    if payload.to_string().len() > state.config.max_request_bytes {
        return Err(GatewayError::PayloadTooLarge {
            size: payload.to_string().len(),
            limit: state.config.max_request_bytes,
        });
    }
    gauge!("gateway_queue_depth").set(0.0);

    gauge!("gateway_inflight").increment(1.0);
    counter!("gateway_requests_total", "route" => "chat_completions", "model_id" => model_id.clone()).increment(1);
    let out = preprocess_request(
        &state.config,
        &state.registry,
        &state.http_client,
        state.hf_sidecar.as_ref(),
        payload.clone(),
    )
    .await?;

    let response = match state.config.run_mode {
        RunMode::PreprocessOnly => (
            StatusCode::OK,
            Json(json!({
                "request_id": request_id,
                "processed": true,
                "media_changed": out.changed_items,
                "payload": out.payload,
            })),
        )
            .into_response(),
        RunMode::Proxy => {
            // Preserve full SGLang-compatible fields by forwarding transformed payload as-is.
            let upstream = state
                .http_client
                .post(format!(
                    "{}/v1/chat/completions",
                    state
                        .config
                        .upstream_url
                        .as_deref()
                        .unwrap_or_default()
                        .trim_end_matches('/')
                ))
                .header("content-type", "application/json")
                .header("x-mm-preprocessed", "1")
                .json(&out.payload)
                .send()
                .await
                .map_err(|e| GatewayError::Upstream(format!("upstream request failed: {e}")))?;

            if payload
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let streamed = stream_proxy_response(upstream).await?;
                histogram!(
                    "gateway_request_seconds",
                    "route" => "chat_completions",
                    "model_id" => model_id
                )
                .record(begin.elapsed().as_secs_f64());
                gauge!("gateway_inflight").decrement(1.0);
                return Ok(streamed);
            }
            let status = axum::http::StatusCode::from_u16(upstream.status().as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let text = upstream
                .text()
                .await
                .map_err(|e| GatewayError::Upstream(format!("read upstream body failed: {e}")))?;
            (status, text).into_response()
        }
    };

    histogram!("gateway_request_seconds", "route" => "chat_completions", "model_id" => model_id)
        .record(begin.elapsed().as_secs_f64());
    gauge!("gateway_inflight").decrement(1.0);
    Ok(response)
}

fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            use std::time::{SystemTime, UNIX_EPOCH};
            static SEQ: AtomicU64 = AtomicU64::new(1);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            format!("req-{now}-{seq}")
        })
}

fn extract_model_id(payload: &Value) -> Result<String, GatewayError> {
    payload
        .get("model")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| GatewayError::BadRequest("missing field: model".to_string()))
}

async fn stream_proxy_response(upstream: reqwest::Response) -> Result<Response, GatewayError> {
    use axum::body::Body;
    use axum::http::header::CONTENT_TYPE;
    use reqwest::header::HeaderMap as ReqwestHeaderMap;

    let status = axum::http::StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    let headers: ReqwestHeaderMap = upstream.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();
    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| GatewayError::Upstream(format!("read upstream stream failed: {e}")))?;
    let mut resp = axum::response::Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    if let Ok(v) = axum::http::HeaderValue::from_str(&content_type) {
        resp.headers_mut().insert(CONTENT_TYPE, v);
    }
    Ok(resp)
}
