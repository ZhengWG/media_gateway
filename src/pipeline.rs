use serde_json::Value;

use crate::config::AppConfig;
use crate::error::{GatewayError, Result};
use crate::hf_sidecar::HfSidecarClient;
use crate::media::{decode_data_url, encode_data_url, fetch_media, preprocess_image, MediaKind};
use crate::models::ModelRegistry;

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
        _ => None,
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
        });
        let sidecar_res = sidecar.preprocess(model_id, &sidecar_input).await?;
        let sidecar_url = sidecar_res
            .payload
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| GatewayError::Internal("hf sidecar payload missing url".to_string()))?;
        media_obj.insert("url".to_string(), Value::String(sidecar_url.to_string()));
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
        MediaKind::Image => preprocess_image(fetched, profile.target_image_edge)
            .map_err(|e| GatewayError::Internal(format!("image preprocess failed: {e}")))?,
        MediaKind::Video | MediaKind::Audio => fetched,
    };
    media_obj.insert(
        "url".to_string(),
        Value::String(encode_data_url(&normalized)),
    );

    metrics::counter!("media_gateway_media_processed_total", "kind" => kind.as_str()).increment(1);
    Ok(true)
}
