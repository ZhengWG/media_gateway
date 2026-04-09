use serde_json::Value;

use crate::config::AppConfig;
use crate::error::{GatewayError, Result};
use crate::hf_sidecar::HfSidecarClient;
use crate::media::{
    decode_data_url, encode_data_url, fetch_media, preprocess_image_to_pixel_values, MediaKind,
};
use crate::models::ModelRegistry;
use crate::preprocess_ops::video::{preprocess_video, VideoSamplePolicy};

pub struct PreprocessOutput {
    pub payload: Value,
    pub changed_items: usize,
}

pub fn extract_model_id(payload: &Value) -> Result<String> {
    payload
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| GatewayError::BadRequest("missing required field `model`".to_string()))
}

pub async fn preprocess_request(
    cfg: &AppConfig,
    registry: &ModelRegistry,
    http_client: &reqwest::Client,
    hf_sidecar: Option<&HfSidecarClient>,
    mut payload: Value,
) -> Result<PreprocessOutput> {
    let model_id = extract_model_id(&payload)?;
    let profile = registry.resolve(&model_id)?;

    let mut changed = 0usize;
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return Err(GatewayError::BadRequest(
            "missing or invalid `messages`".to_string(),
        ));
    };

    for message in messages {
        let Some(content) = message.get_mut("content") else {
            continue;
        };
        let Some(parts) = content.as_array_mut() else {
            continue;
        };

        for part in parts {
            let Some(part_type) = part.get("type").and_then(Value::as_str) else {
                continue;
            };
            let Some((key, kind)) = detect_media_kind(part_type) else {
                continue;
            };

            if process_part(
                part,
                key,
                kind,
                &model_id,
                &profile,
                http_client,
                hf_sidecar,
                cfg.inject_processor_output,
                profile.max_media_bytes,
                cfg.fetch_timeout,
                &cfg.allowed_hosts,
                cfg.allow_private_network,
            )
            .await?
            {
                changed += 1;
            }
        }
    }

    if let Some(obj) = payload.as_object_mut() {
        obj.insert("mm_preprocessed".to_string(), Value::Bool(true));
    }

    Ok(PreprocessOutput {
        payload,
        changed_items: changed,
    })
}

fn detect_media_kind(part_type: &str) -> Option<(&'static str, MediaKind)> {
    match part_type {
        "image_url" => Some(("image_url", MediaKind::Image)),
        "video_url" => Some(("video_url", MediaKind::Video)),
        "audio" => Some(("audio", MediaKind::Audio)),
        "audio_url" => Some(("audio_url", MediaKind::Audio)),
        _ => None,
    }
}

fn allowed_processor_output_keys(kind: MediaKind) -> &'static [&'static str] {
    match kind {
        // Core image fields commonly consumed by SGLang multimodal processors.
        MediaKind::Image => &["pixel_values", "image_grid_thw", "image_sizes"],
        // Core video fields for processor_output-like video consumption.
        MediaKind::Video => &[
            "pixel_values_videos",
            "video_grid_thw",
            "second_per_grid_ts",
        ],
        // Core audio fields used by HF/SGLang-style audio processor pipelines.
        MediaKind::Audio => &[
            "input_features",
            "audio_features",
            "audio_feature_lens",
            "input_features_mask",
            "feature_attention_mask",
            "audio_attention_mask",
        ],
    }
}

fn sanitize_processor_output(kind: MediaKind, processor_output: Value) -> Option<Value> {
    let Value::Object(obj) = processor_output else {
        return None;
    };
    let mut filtered = serde_json::Map::new();
    for key in allowed_processor_output_keys(kind) {
        if let Some(v) = obj.get(*key) {
            filtered.insert((*key).to_string(), v.clone());
        }
    }
    if filtered.is_empty() {
        None
    } else {
        Some(Value::Object(filtered))
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_part(
    part: &mut Value,
    key: &str,
    kind: MediaKind,
    model_id: &str,
    profile: &crate::config::ModelProfile,
    http_client: &reqwest::Client,
    hf_sidecar: Option<&HfSidecarClient>,
    inject_processor_output: bool,
    max_media_bytes: usize,
    fetch_timeout: std::time::Duration,
    allowed_hosts: &std::collections::HashSet<String>,
    allow_private_network: bool,
) -> Result<bool> {
    let Some(media_obj) = part.get_mut(key).and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let Some(raw_url_str) = media_obj.get("url").and_then(Value::as_str) else {
        return Ok(false);
    };
    let raw_url = raw_url_str.to_string();

    if let Some(sidecar) = hf_sidecar {
        let sidecar_input = serde_json::json!({
            "model": model_id,
            "kind": kind.as_str(),
            "url": raw_url,
            "need_processor_output": inject_processor_output,
        });
        let sidecar_res = sidecar.preprocess(model_id, &sidecar_input).await?;
        let sidecar_url = sidecar_res
            .payload
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| GatewayError::Internal("hf sidecar payload missing url".to_string()))?;
        media_obj.insert("url".to_string(), Value::String(sidecar_url.to_string()));
        if inject_processor_output {
            if let Some(processor_output) = sidecar_res.processor_output {
                if let Some(filtered) = sanitize_processor_output(kind, processor_output) {
                    media_obj.insert("processor_output".to_string(), filtered);
                }
            }
        }
        metrics::counter!("media_gateway_media_processed_total", "kind" => kind.as_str())
            .increment(1);
        return Ok(true);
    }

    let fetched = if let Some(p) = decode_data_url(&raw_url)? {
        p
    } else {
        fetch_media(
            http_client,
            &raw_url,
            max_media_bytes,
            fetch_timeout,
            allowed_hosts,
            allow_private_network,
        )
        .await?
    };
    let normalized = match kind {
        MediaKind::Image => preprocess_image_to_pixel_values(fetched, profile.target_image_edge)
            .map_err(|e| GatewayError::Internal(format!("image preprocess failed: {e}")))?,
        MediaKind::Video => {
            preprocess_video(
                fetched,
                VideoSamplePolicy {
                    max_frames: 8,
                    frame_interval: 2,
                },
            )
            .map_err(|e| GatewayError::Internal(format!("video preprocess failed: {e}")))?
            .payload
        }
        MediaKind::Audio => fetched,
    };
    media_obj.insert(
        "url".to_string(),
        Value::String(encode_data_url(&normalized)),
    );

    metrics::counter!("media_gateway_media_processed_total", "kind" => kind.as_str()).increment(1);
    Ok(true)
}
