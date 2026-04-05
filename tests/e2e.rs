use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use tower::ServiceExt;

#[path = "../src/app.rs"]
mod app;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/error.rs"]
mod error;
#[path = "../src/hf_sidecar.rs"]
mod hf_sidecar;
#[path = "../src/media.rs"]
mod media;
#[path = "../src/models.rs"]
mod models;
#[path = "../src/pipeline.rs"]
mod pipeline;

fn test_config() -> config::AppConfig {
    config::AppConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().expect("addr"),
        run_mode: config::RunMode::PreprocessOnly,
        request_timeout: Duration::from_secs(5),
        fetch_timeout: Duration::from_secs(5),
        max_request_bytes: 1024 * 1024,
        max_inflight: 8,
        allow_private_network: true,
        allowed_hosts: HashSet::new(),
        default_profile: config::ModelProfile {
            target_image_edge: 32,
            max_media_bytes: 1024 * 1024,
        },
        model_profiles: HashMap::new(),
        hf_processor_mode: config::HfProcessorMode::Disabled,
        hf_sidecar_command_template: "{python_bin} {script_path}".to_string(),
        hf_python_bin: "python3".to_string(),
        hf_sidecar_script: "scripts/hf_processor_sidecar.py".to_string(),
        hf_sidecar_timeout: Duration::from_secs(30),
        inject_processor_output: false,
    }
}

fn test_metrics_handle() -> Arc<metrics_exporter_prometheus::PrometheusHandle> {
    static HANDLE: OnceLock<Arc<metrics_exporter_prometheus::PrometheusHandle>> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            Arc::new(
                metrics_exporter_prometheus::PrometheusBuilder::new()
                    .install_recorder()
                    .expect("install metrics"),
            )
        })
        .clone()
}

#[tokio::test]
async fn preprocess_local_file_not_found_returns_400() {
    let cfg = test_config();
    let state = app::AppState {
        registry: models::ModelRegistry::from_config(&cfg),
        http_client: reqwest::Client::new(),
        metrics_handle: test_metrics_handle(),
        config: cfg,
        hf_sidecar: None,
    };
    let router = app::build_router(state);
    let payload = serde_json::json!({
        "model": "demo",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": "file:///tmp/definitely-not-exist-12345.png" }
            }]
        }]
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/preprocess")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request");

    let resp = router.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn preprocess_bad_base64_returns_400() {
    let cfg = test_config();
    let state = app::AppState {
        registry: models::ModelRegistry::from_config(&cfg),
        http_client: reqwest::Client::new(),
        metrics_handle: test_metrics_handle(),
        config: cfg,
        hf_sidecar: None,
    };
    let router = app::build_router(state);
    let payload = serde_json::json!({
        "model": "demo",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": "data:image/png;base64,not-base64@@@" }
            }]
        }]
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/preprocess")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request");

    let resp = router.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn preprocess_non_image_payload_returns_500() {
    let cfg = test_config();
    let state = app::AppState {
        registry: models::ModelRegistry::from_config(&cfg),
        http_client: reqwest::Client::new(),
        metrics_handle: test_metrics_handle(),
        config: cfg,
        hf_sidecar: None,
    };
    let router = app::build_router(state);
    let bad_bytes = base64::engine::general_purpose::STANDARD.encode(b"not-an-image");
    let payload = serde_json::json!({
        "model": "demo",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": format!("data:image/png;base64,{bad_bytes}") }
            }]
        }]
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/preprocess")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request");

    let resp = router.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn proxy_mode_forwards_with_skip_header() {
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(
            |headers: axum::http::HeaderMap, Json(payload): Json<serde_json::Value>| async move {
                let skip = headers
                    .get("x-mm-preprocessed")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                Json(serde_json::json!({
                    "received_skip": skip,
                    "model": payload.get("model").and_then(|v| v.as_str()).unwrap_or_default(),
                }))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, upstream_app).await;
    });

    let mut cfg = test_config();
    cfg.run_mode = config::RunMode::Proxy;
    let state = app::AppState {
        registry: models::ModelRegistry::from_config(&cfg),
        http_client: reqwest::Client::new(),
        metrics_handle: test_metrics_handle(),
        config: cfg,
        hf_sidecar: None,
    };
    let router = app::build_router(state);
    let img = image::DynamicImage::new_rgb8(2, 2);
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("png encode");
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    let payload = serde_json::json!({
        "upstream_url": format!("http://{addr}"),
        "model": "demo",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": format!("data:image/png;base64,{b64}") }
            }]
        }]
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request");
    let resp = router.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn preprocess_supports_sglang_audio_url_shape() {
    let cfg = test_config();
    let state = app::AppState {
        registry: models::ModelRegistry::from_config(&cfg),
        http_client: reqwest::Client::new(),
        metrics_handle: test_metrics_handle(),
        config: cfg,
        hf_sidecar: None,
    };
    let router = app::build_router(state);
    let payload = serde_json::json!({
        "model": "demo",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "audio_url",
                "audio_url": { "url": "data:audio/mpeg;base64,aGVsbG8=" }
            }]
        }]
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/preprocess")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request");
    let resp = router.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn preprocess_injects_processor_output_from_sidecar_when_enabled() {
    let sidecar_app = Router::new().route(
        "/sidecar",
        post(|Json(_payload): Json<serde_json::Value>| async move {
            Json(serde_json::json!({
                "payload": {
                    "url": "data:application/octet-stream;base64,aGVsbG8="
                },
                "changed_items": 1,
                "processor_output": {
                    "pixel_values": [[[[0.1, 0.2], [0.3, 0.4]]]],
                    "attention_mask": [1, 1, 1],
                    "input_ids": [10, 20, 30]
                }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sidecar");
    let addr = listener.local_addr().expect("sidecar addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, sidecar_app).await;
    });

    let mut cfg = test_config();
    cfg.hf_processor_mode = config::HfProcessorMode::PythonSidecar;
    cfg.inject_processor_output = true;
    cfg.hf_sidecar_command_template = format!(
        "python3 -c \"import json,sys,urllib.request; req=json.loads(sys.stdin.readline()); r=urllib.request.Request('http://{addr}/sidecar', data=json.dumps(req).encode('utf-8'), headers={{'content-type':'application/json'}}); print(urllib.request.urlopen(r, timeout=5).read().decode('utf-8'))\""
    );

    let state = app::AppState {
        registry: models::ModelRegistry::from_config(&cfg),
        http_client: reqwest::Client::new(),
        metrics_handle: test_metrics_handle(),
        config: cfg.clone(),
        hf_sidecar: Some(hf_sidecar::HfSidecarClient::new(
            cfg.hf_sidecar_command_template.clone(),
            cfg.hf_sidecar_timeout,
        )),
    };
    let router = app::build_router(state);
    let payload = serde_json::json!({
        "model": "demo",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": "data:image/png;base64,aGVsbG8=" }
            }]
        }]
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/preprocess")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request");
    let resp = router.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let part = &v["payload"]["messages"][0]["content"][0]["image_url"];
    assert!(part.get("processor_output").is_some());
    let po = &part["processor_output"];
    assert!(po.get("pixel_values").is_some());
    assert!(po.get("attention_mask").is_none());
    assert!(po.get("input_ids").is_none());
}

#[tokio::test]
async fn proxy_mode_requires_upstream_url_in_body() {
    let cfg = test_config();
    let mut cfg = cfg;
    cfg.run_mode = config::RunMode::Proxy;
    let state = app::AppState {
        registry: models::ModelRegistry::from_config(&cfg),
        http_client: reqwest::Client::new(),
        metrics_handle: test_metrics_handle(),
        config: cfg,
        hf_sidecar: None,
    };
    let router = app::build_router(state);
    let payload = serde_json::json!({
        "model": "demo",
        "messages": [{
            "role": "user",
            "content": [{"type":"text","text":"hello"}]
        }]
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request");
    let resp = router.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
